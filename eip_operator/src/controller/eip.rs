use std::collections::HashMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::Metadata;
use kube::api::{Api, ListParams};
use kube::Client;
use kube_runtime::controller::Action;
use tracing::instrument;
use tracing::{error, info, warn};

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
        let namespaced = Api::namespaced(client.clone(), eip.namespace().unwrap());
        let eip_api = namespaced;
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

        // Ensure the EIP Exists
        let _allocation_id = match addresses.len() {
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

        // Handle association / disassociatino
        // disassociate if:
        // - associated, but no longer has a claim (with caveot)
        // - resource with claim is terminating or gone
        // - resource with claim no longer matches
        // associate if
        //  - not associated and claim
        let originally_unclaimed = status.claim.is_some();

        // get potential attachments
        let matched_pods: Vec<Pod> = if eip.selects_with_pod() {
            pod_api
                .list(&ListParams::default())
                .await?
                .into_iter()
                .filter(|pod| eip.matches_pod(pod) && pod.metadata.deletion_timestamp.is_none())
                .collect()
        } else {
            vec![]
        };
        let matched_nodes: Vec<Node> = if eip.selects_with_node() {
            node_api
                .list(&ListParams::default())
                .await?
                .into_iter()
                .filter(|node| {
                    eip.matches_node(node) && node.metadata().deletion_timestamp.is_none()
                })
                .collect()
        } else {
            vec![]
        };

        // Check to make sure our resource still matches
        // incase our selectors have updated, or the nodes/pods we have
        // claimed have changed
        let selector_or_resource_changed_claim = match &eip.spec.selector {
            EipSelector::Pod { pod_name: _ } => {
                // pods match by name which is unique within a namespace and cannot be renamed
                // if this list is empty we should drop the claim and disassociate
                // otherwise we're good
                matched_pods.is_empty()
            }
            EipSelector::Node { selector: _ } => {
                // check if our claim is still in the node match list
                // if so we don't need to disassociate
                !matched_nodes
                    .iter()
                    .any(|n| n.metadata.name == status.claim)
            }
        };
        // We need to remove our claim
        if selector_or_resource_changed_claim {
            info!("Selector or resource changed, claim has been updated!");
            status.claim = None;
        }

        // Find new claim, it's possible it'll match the existing
        // association, in which case, we won't disassociate
        if status.claim.is_none() {
            match eip.spec.selector {
                EipSelector::Node { selector: _ } => match matched_nodes.len() {
                    0 => {
                        warn!("Eip {} matches no nodes", name);
                    }
                    1 => {
                        let node_name = matched_nodes[0]
                            .metadata
                            .name
                            .clone()
                            .ok_or(Error::MissingNodeName)?;
                        info!("Eip {} matches node {}, updating claim", name, node_name,);
                        status.claim = Some(node_name);
                    }
                    _ => {
                        warn!(
                            "Eip {} matches multiple nodes - {}, choosing the first",
                            name,
                            matched_nodes
                                .iter()
                                .map(|node| { node.metadata.name.clone().unwrap_or_default() })
                                .collect::<Vec<String>>()
                                .join(",")
                        );
                        let node_name = matched_nodes[0]
                            .metadata
                            .name
                            .clone()
                            .ok_or(Error::MissingNodeName)?;
                        info!("Eip {} matches node {}, updating claim", name, node_name,);
                        status.claim = Some(node_name);
                    }
                },
                EipSelector::Pod { pod_name: _ } => match matched_pods.len() {
                    0 => {
                        warn!("Eip {} matches no pods", name);
                    }
                    1 => {
                        info!(
                            "Eip {} matches pod {}, updating claim",
                            name,
                            matched_pods[0]
                                .metadata
                                .name
                                .clone()
                                .ok_or(Error::MissingPodName)?,
                        );
                        status.claim = Some(
                            matched_pods[0]
                                .metadata
                                .name
                                .clone()
                                .ok_or(Error::MissingPodName)?,
                        );
                    }
                    _ => {
                        error!(
                            "Eip {} matches multiple pods - {}",
                            name,
                            matched_pods
                                .iter()
                                .map(|pod| { pod.metadata.name.clone().unwrap_or_default() })
                                .collect::<Vec<String>>()
                                .join(",")
                        );
                    }
                },
            }
        }

        // TODO (jubrad)
        // This code should handle migration to claims
        // a bit more elegantly, it can be removed after all
        // eips are using claims.
        // We want to avoid disassociate/reassociate
        // in the case that an eip prior to the introduction
        // of claims is correctly associated without a claim
        let mut correctly_associated_but_originally_unclaimed: bool = false;
        if eip.attached() && originally_unclaimed && status.claim.is_some() {
            let claim = status.claim.clone().unwrap();
            correctly_associated_but_originally_unclaimed = match &eip.spec.selector {
                EipSelector::Node { selector: _ } => {
                    status.private_ip_address.as_deref()
                        == node_api
                            .get_opt(&claim)
                            .await?
                            .ok_or(Error::MissingNode)?
                            .ip()
                }
                EipSelector::Pod { pod_name: _ } => {
                    status.private_ip_address.as_deref()
                        == pod_api
                            .get_opt(&claim)
                            .await?
                            .ok_or(Error::MissingPod)?
                            .ip()
                }
            };
        };

        // Disassociaate if conditions are met
        if eip.attached()
            && (status.claim.is_none()
                || (selector_or_resource_changed_claim
                    && !correctly_associated_but_originally_unclaimed))
        {
            info!("Disassociating EIP! {}", name);
            let association_id = eip.association_id(&self.ec2_client).await?;
            if let Some(id) = association_id {
                crate::aws::disassociate_eip(&self.ec2_client, &id).await?;
            } else {
                info!("EIP {} was already disassociated", name);
            }
            status.eni = None;
            status.private_ip_address = None;
        }

        // Associate if claimed and not attached
        if status.claim.is_some() && !eip.attached() {
            info!("Associating EIP! {}", name);
            let claim = status.claim.clone().unwrap();
            // find the node and ip to associate
            let (node, ip) = match &eip.spec.selector {
                EipSelector::Node { selector: _ } => {
                    let node = node_api.get_opt(&claim).await?.ok_or(Error::MissingNode)?;
                    let node_ip = node.ip().ok_or(Error::MissingNodeIp)?;
                    (node.to_owned(), node_ip.to_owned())
                }
                EipSelector::Pod { pod_name: _ } => {
                    let pod = pod_api.get_opt(&claim).await?.ok_or(Error::MissingPod)?;
                    let node_name = pod.node_name();
                    if node_name.is_none() {
                        warn!(
                            "Pod {} does not yet have a node assigned- Rescheduling",
                            pod.metadata.name.ok_or(Error::MissingPodName)?
                        );
                        return Ok(Some(Action::requeue(Duration::from_secs(1))));
                    }
                    let node = node_api
                        .get_opt(node_name.unwrap())
                        .await?
                        .ok_or(Error::MissingNode)?;
                    let pod_ip = pod.ip();
                    if pod_ip.is_none() {
                        warn!(
                            "Pod {} does not have a ip assigned - Rescheduling",
                            pod.metadata.name.ok_or(Error::MissingPodName)?
                        );
                        return Ok(Some(Action::requeue(Duration::from_secs(1))));
                    }
                    (node, pod_ip.unwrap().to_owned())
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

        // Ensure status is up-to-date
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
