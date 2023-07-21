use std::collections::HashMap;

use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams};
use kube::Client;
use kube_runtime::controller::Action;
use tracing::instrument;
use tracing::{info, warn};

use eip_operator_shared::Error;

use crate::eip::v2::{Eip, EipSelector};
use crate::kube_ext::{NodeExt, PodExt};

pub(crate) struct Context {
    ec2_client: aws_sdk_ec2::Client,
    cluster_name: String,
    default_tags: HashMap<String, String>,
}

impl Context {
    pub(crate) fn new(
        ec2_client: aws_sdk_ec2::Client,
        cluster_name: String,
        default_tags: HashMap<String, String>,
    ) -> Self {
        Self {
            ec2_client,
            cluster_name,
            default_tags,
        }
    }
}

#[async_trait::async_trait]
impl k8s_controller::Context for Context {
    type Resource = Eip;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/destroy";

    #[instrument(skip(self, client, eip), err)]
    async fn apply(
        &self,
        client: Client,
        eip: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let eip_api = Api::namespaced(client.clone(), eip.namespace().unwrap());
        let pod_api = Api::<Pod>::namespaced(client.clone(), eip.namespace().unwrap());
        let node_api = Api::<Node>::all(client.clone());

        // Ensure EIP is Created
        let uid = eip.metadata.uid.as_ref().ok_or(Error::MissingEipUid)?;
        let name = eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
        let selector = &eip.spec.selector;
        let mut status = eip.status.clone().unwrap_or_default();

        let addresses = crate::aws::describe_addresses_with_tag_value(
            &self.ec2_client,
            crate::aws::EIP_UID_TAG,
            uid,
        )
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?;
        let allocation_id = match addresses.len() {
            0 => {
                let response = crate::aws::allocate_address(
                    &self.ec2_client,
                    uid,
                    name,
                    selector,
                    &self.cluster_name,
                    eip.namespace().unwrap(),
                    &self.default_tags,
                )
                .await?;
                let allocation_id = response.allocation_id.ok_or(Error::MissingAllocationId)?;
                let public_ip = response.public_ip.ok_or(Error::MissingPublicIp)?;
                status.allocation_id = Some(allocation_id.clone());
                status.public_ip_address = Some(public_ip);
                allocation_id
            }
            1 => {
                let allocation_id = addresses[0]
                    .allocation_id
                    .as_ref()
                    .ok_or(Error::MissingAllocationId)?;
                let public_ip = addresses[0]
                    .public_ip
                    .as_ref()
                    .ok_or(Error::MissingPublicIp)?;
                status.allocation_id = Some(allocation_id.clone());
                status.public_ip_address = Some(public_ip.clone());
                allocation_id.to_owned()
            }
            _ => {
                return Err(Error::MultipleEipsTaggedForPod);
            }
        };

        // disassociate if unclaimed or if claimed and the node / pod terminating
        let mut disassociate = status.claim.is_none();
        if let Some(claim) = status.claim.clone() {
            match &eip.spec.selector {
                EipSelector::Node { selector: _ } => {
                    if let Some(node) = node_api.get_opt(&claim.clone()).await? {
                        disassociate = node.metadata.deletion_timestamp.is_some();
                    }
                }
                EipSelector::Pod { pod_name: _ } => {
                    if let Some(pod) = pod_api.get_opt(&claim.clone()).await? {
                        disassociate = pod.metadata.deletion_timestamp.is_some()
                    }
                }
            }
        }
        if disassociate {
            crate::aws::disassociate_eip(&self.ec2_client, &allocation_id).await?;
            status.claim = None;
            status.eni = None;
            status.private_ip_address = None;
        }

        // search for new resource to be claimed by
        match eip.spec.selector {
            EipSelector::Node { selector: _ } => {
                let nodes: Vec<Node> = node_api
                    .list(&ListParams::default())
                    .await?
                    .into_iter()
                    .filter(|node| eip.matches_node(node))
                    .collect();
                match nodes.len() {
                    0 => {
                        warn!("Eip {} matches no nodes", name);
                    }
                    1 => {
                        let node_name = nodes[0]
                            .metadata
                            .name
                            .clone()
                            .ok_or(Error::MissingNodeName)?;
                        info!("Eip {} matches node {}, updating claim", name, node_name,);
                        status.claim = Some(node_name);
                    }
                    _ => {
                        warn!(
                            "Eip {} matches multiple nodes - {}",
                            name,
                            nodes
                                .iter()
                                .map(|node| { node.metadata.name.clone().unwrap_or_default() })
                                .collect::<Vec<String>>()
                                .join(",")
                        );
                    }
                }
            }
            EipSelector::Pod { pod_name: _ } => {
                let pods: Vec<Pod> = pod_api
                    .list(&ListParams::default())
                    .await?
                    .into_iter()
                    .filter(|pod| eip.matches_pod(pod))
                    .collect();
                match pods.len() {
                    0 => {
                        warn!("Eip {} matches no pods", name);
                    }
                    1 => {
                        info!(
                            "Eip {} matches pod {}, updating claim",
                            name,
                            pods[0].metadata.name.clone().ok_or(Error::MissingPodName)?,
                        );
                        status.claim =
                            Some(pods[0].metadata.name.clone().ok_or(Error::MissingPodName)?);
                    }
                    _ => {
                        info!(
                            "Eip {} matches multiple pods - {}",
                            name,
                            pods.iter()
                                .map(|pod| { pod.metadata.name.clone().unwrap_or_default() })
                                .collect::<Vec<String>>()
                                .join(",")
                        );
                    }
                }
            }
        }

        // Associate if claimed
        if status.claim.is_some() && !eip.attached() {
            let claim = status.claim.clone().unwrap();
            let (node, ip) = match &eip.spec.selector {
                EipSelector::Node { selector: _ } => {
                    let node = node_api.get_opt(&claim).await?.ok_or(Error::MissingNode)?;
                    let node_ip = node.ip().ok_or(Error::MissingNodeIp)?;
                    (node.to_owned(), node_ip.to_owned())
                }
                EipSelector::Pod { pod_name: _ } => {
                    let pod = pod_api.get_opt(&claim).await?.ok_or(Error::MissingPod)?;
                    let node_name = pod.node_name().ok_or(Error::MissingNodeName)?;
                    let node = node_api
                        .get_opt(node_name)
                        .await?
                        .ok_or(Error::MissingNode)?;
                    let pod_ip = pod.ip().ok_or(Error::MissingPodIp)?;
                    (node, pod_ip.to_owned())
                }
            };
            // attach to node
            let provider_id = node.provider_id().ok_or(Error::MissingProviderId)?;
            let instance_id = provider_id
                .rsplit_once('/')
                .ok_or(Error::MalformedProviderId)?
                .1;
            let instance_description =
                crate::aws::describe_instance(&self.ec2_client, instance_id).await?;
            let allocation_id = status
                .allocation_id
                .clone()
                .ok_or(Error::MissingAllocationId)?;
            let eni_id = crate::aws::get_eni_from_private_ip(&instance_description, &ip)
                .ok_or(Error::NoInterfaceWithThatIp)?;
            crate::aws::associate_eip(&self.ec2_client, &allocation_id, &eni_id, &ip).await?;
            status.eni = Some(eni_id);
            status.private_ip_address = Some(ip.to_owned());
        }

        if eip.status != Some(status.clone()) {
            crate::eip::update_status(&eip_api, name, &status).await?;
        }
        Ok(None)
    }

    #[instrument(skip(self, _client, eip), err)]
    async fn cleanup(
        &self,
        _client: Client,
        eip: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let uid = eip.metadata.uid.as_ref().ok_or(Error::MissingEipUid)?;
        let addresses = crate::aws::describe_addresses_with_tag_value(
            &self.ec2_client,
            crate::aws::EIP_UID_TAG,
            uid,
        )
        .await?
        .addresses;
        if let Some(addresses) = addresses {
            for address in addresses {
                crate::aws::disassociate_and_release_address(&self.ec2_client, &address).await?;
            }
        }
        Ok(None)
    }
}
