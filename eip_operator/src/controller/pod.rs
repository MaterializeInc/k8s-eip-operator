use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{event, instrument, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;

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

    #[instrument(skip(self, client, pod), err)]
    async fn apply(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        // find matching eip and claim, create it if should autocreate
        let name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
        let eip_api = Api::<Eip>::namespaced(client.clone(), &pod.namespace().unwrap());
        let pod_api = Api::<Pod>::namespaced(client.clone(), &pod.namespace().unwrap());

        // create eip if should auto-create
        if should_autocreate_eip(pod) {
            event!(Level::INFO, should_autocreate_eip = true);
            crate::eip::create_for_pod(&eip_api, name).await?;
        }

        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips
            .into_iter()
            .find(|eip| eip.matches_pod(pod))
            .ok_or_else(|| Error::NoEipResourceWithThatPodName(name.to_owned()))?;
        let eip_name = eip.name().ok_or(Error::MissingEipName)?;
        crate::eip::set_status_claimed(&eip_api, eip_name, name).await?;
        let allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
        let eip_description = crate::aws::describe_address(&self.ec2_client, allocation_id)
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?
            .swap_remove(0);
        let public_ip = eip_description.public_ip.ok_or(Error::MissingPublicIp)?;
        add_dns_target_annotation(&pod_api, name, &public_ip, allocation_id).await?;
        Ok(None)
    }

    #[instrument(skip(self, client, pod), err)]
    async fn cleanup(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        // remove claim, delete if autocreate
        let name = pod.metadata.name.as_ref().ok_or(Error::MissingPodUid)?;
        let eip_api = Api::<Eip>::namespaced(client.clone(), &pod.namespace().unwrap());
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips.into_iter().find(|eip| eip.matches_pod(pod));
        if should_autocreate_eip(pod) && eip.is_some() {
            event!(Level::INFO, should_autocreate_eip = true);
            crate::eip::delete(&eip_api, eip.unwrap().name().ok_or(Error::MissingEipName)?).await?;
        } else {
            crate::eip::set_status_detached(&eip_api, name).await?;
        };
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
