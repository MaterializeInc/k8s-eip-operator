use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use serde::Deserialize;
use tracing::{event, instrument, warn, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;
use crate::kube_ext::{NodeExt, PodExt};

pub(crate) struct Context {
    ec2_client: aws_sdk_ec2::Client,
}

impl Context {
    pub(crate) fn new(ec2_client: aws_sdk_ec2::Client) -> Self {
        Self { ec2_client }
    }
}

#[async_trait::async_trait]
impl k8s_controller::Context for Context {
    type Resource = Pod;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/disassociate";

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(skip(self, client, pod), err)]
    async fn apply(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let name = pod.name_unchecked();

        let eip_api = Api::<Eip>::namespaced(client.clone(), &pod.namespace().unwrap());
        let pod_api = Api::<Pod>::namespaced(client.clone(), &pod.namespace().unwrap());
        let node_api = Api::<Node>::all(client.clone());

        if should_autocreate_eip(pod) {
            event!(Level::INFO, should_autocreate_eip = true);
            crate::eip::create_for_pod(&eip_api, &name).await?;
        }

        let node_name = match pod.node_name() {
            Some(node_name) => node_name,
            None => {
                warn!("Pod {} is not yet scheduled", name);
                return Ok(Some(Action::requeue(Duration::from_secs(3))));
            }
        };
        let pod_ip = pod.ip().ok_or(Error::MissingPodIp)?;
        let node = node_api.get(node_name).await?;

        let provider_id = node.provider_id().ok_or(Error::MissingProviderId)?;
        let instance_id = provider_id
            .rsplit_once('/')
            .ok_or(Error::MalformedProviderId)?
            .1;

        let eni_id = match get_eni_id_from_annotation(pod) {
            Some(eni_id) => eni_id,
            None => {
                let instance_description =
                    crate::aws::describe_instance(&self.ec2_client, instance_id).await?;

                crate::aws::get_eni_from_private_ip(&instance_description, pod_ip)
                    .ok_or(Error::NoInterfaceWithThatIp)?
            }
        };
        let eips = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|eip| eip.matches_pod(&name))
            .collect::<Vec<_>>();
        let eip = match eips.len() {
            0 => {
                // It is ok to not have an EIP
                // if one is created later it should ask this pod to re-reconcile
                warn!("No EIP found with pod name {}.", name);
                return Ok(None);
            }
            1 => eips[0].clone(),
            _ => return Err(Error::MultipleEipsTaggedForPod),
        };
        let allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
        let eip_description = crate::aws::describe_address(&self.ec2_client, allocation_id)
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?
            .swap_remove(0);
        let public_ip = eip_description.public_ip.ok_or(Error::MissingPublicIp)?;
        crate::eip::set_status_should_attach(&eip_api, &eip, &eni_id, pod_ip, &name).await?;
        if eip_description.network_interface_id != Some(eni_id.to_owned())
            || eip_description.private_ip_address != Some(pod_ip.to_owned())
        {
            let association_id =
                crate::aws::associate_eip(&self.ec2_client, allocation_id, &eni_id, pod_ip)
                    .await?
                    .association_id
                    .ok_or(Error::MissingAssociationId)?;
            crate::eip::set_status_association_id(&eip_api, &eip.name_unchecked(), &association_id)
                .await?;
        }
        add_dns_target_annotation(&pod_api, &name, &public_ip, allocation_id).await?;
        Ok(None)
    }

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(skip(self, client, pod), err)]
    async fn cleanup(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let name = pod.name_unchecked();

        let eip_api = Api::<Eip>::namespaced(client.clone(), &pod.namespace().unwrap());

        let all_eips = eip_api.list(&ListParams::default()).await?.items;

        let eips = all_eips.into_iter().filter(|eip| {
            eip.status.as_ref().map_or(false, |s| {
                s.resource_id.as_ref().map_or(false, |r| *r == name)
            })
        });

        for eip in eips {
            let allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
            let addresses = crate::aws::describe_address(&self.ec2_client, allocation_id)
                .await?
                .addresses
                .ok_or(Error::MissingAddresses)?;
            for address in addresses {
                if let Some(association_id) = address.association_id {
                    crate::aws::disassociate_eip(&self.ec2_client, &association_id).await?;
                }
            }
            crate::eip::set_status_detached(&eip_api, &eip).await?;
        }
        if should_autocreate_eip(pod) {
            event!(Level::INFO, should_autocreate_eip = true);
            crate::eip::delete(&eip_api, &name).await?;
        }
        Ok(None)
    }
}

/// Checks if the autocreate label is set to true on a pod.
fn should_autocreate_eip(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|label| {
            label
                .get(crate::AUTOCREATE_EIP_LABEL)
                .map(|s| (*s).as_ref())
        })
        .unwrap_or("false")
        .to_lowercase()
        == "true"
}

/// Parse the vpc.amazonaws.com/pod-eni annotation if it exists, and return the ENI ID.
#[instrument(skip(pod))]
fn get_eni_id_from_annotation(pod: &Pod) -> Option<String> {
    event!(Level::INFO, "Getting ENI ID from annotation.");
    let annotation = pod
        .metadata
        .annotations
        .as_ref()?
        .get("vpc.amazonaws.com/pod-eni")?;
    event!(Level::INFO, annotation = %annotation);

    /// An annotation attached to a pod by EKS describing the branch network
    /// interfaces when using per-pod security groups.
    /// example: [{
    ///     "eniId":"eni-0e42914a33ee3c5ce",
    ///     "ifAddress":"0e:cb:3c:0d:97:3b",
    ///     "privateIp":"10.1.191.190",
    ///     "vlanId":1,
    ///     "subnetCidr":"10.1.160.0/19"
    /// }]
    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct EniDescription {
        eni_id: String,
    }
    let eni_descriptions: Vec<EniDescription> = serde_json::from_str(annotation).ok()?;

    Some(eni_descriptions.first()?.eni_id.to_owned())
}

/// Applies annotation to pod specifying the target IP for external-dns.
#[instrument(skip(api), err)]
async fn add_dns_target_annotation(
    api: &Api<Pod>,
    name: &str,
    eip_address: &str,
    allocation_id: &str,
) -> Result<Pod, kube::Error> {
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "annotations": {
                crate::EIP_ALLOCATION_ID_ANNOTATION: allocation_id,
                crate::EXTERNAL_DNS_TARGET_ANNOTATION: eip_address
            }
        }
    });
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(crate::FIELD_MANAGER);
    api.patch(name, &params, &patch).await
}
