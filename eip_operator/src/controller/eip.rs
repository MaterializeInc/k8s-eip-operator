use std::collections::HashMap;
use std::time::Duration;

use k8s_openapi::api::core::v1::{Node, Pod};
use k8s_openapi::Metadata;
use kube::api::{Api, ListParams};
use kube::{Client, ResourceExt};
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

        // Ensure the EIP Exists
        let (allocation_id, public_ip) = match addresses.len() {
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
                (allocation_id, public_ip)
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
                (allocation_id.to_owned(), public_ip.to_owned())
            }
            _ => {
                return Err(Error::MultipleEipsTaggedForPod);
            }
        };
        crate::eip::set_status_created(&eip_api, name, &allocation_id, &public_ip).await?;
        status.allocation_id = Some(allocation_id);
        status.public_ip_address = Some(public_ip);

        // get potential attachments
        let mut matched_pods: Vec<Pod> = vec![];
        let mut matched_nodes: Vec<Node> = vec![];

        match eip.spec.selector {
            EipSelector::Pod { ref pod_name } => {
                matched_pods = pod_api
                    .list(&ListParams {
                        field_selector: Some(format!("metadata.name={}", pod_name)),
                        ..Default::default()
                    })
                    .await?
                    .into_iter()
                    .filter(|pod| pod.metadata.deletion_timestamp.is_none())
                    .collect::<Vec<Pod>>();
                matched_pods.sort_unstable_by_key(|s| s.name_unchecked());
            }
            EipSelector::Node { ref selector } => {
                let label_selector = selector
                    .iter()
                    .map(|(k, v)| format!("{}={}", k, v))
                    .collect::<Vec<String>>()
                    .join(",");
                matched_nodes = node_api
                    .list(&ListParams {
                        label_selector: Some(label_selector),
                        ..Default::default()
                    })
                    .await?
                    .into_iter()
                    .filter(|node| node.metadata().deletion_timestamp.is_none())
                    .collect::<Vec<Node>>();
                matched_nodes.sort_unstable_by_key(|s| s.name_unchecked());
            }
        }

        // Check to make sure our resource still matches
        // incase our selectors have updated, or the nodes/pods have changed
        let mut disassociate = false;
        match &eip.spec.selector {
            EipSelector::Pod { pod_name: _ } => {
                if matched_pods.is_empty() {
                    disassociate = true;
                }
            }
            EipSelector::Node { selector: _ } => {
                disassociate = true;
                for node in &matched_nodes {
                    if eip.associated_with_node(node) {
                        disassociate = false;
                        break;
                    }
                }
            }
        };

        // Disassociaate if conditions are met
        if disassociate {
            info!("Disassociating EIP! {}", name);
            let association_id = match addresses.len() {
                0 => Ok(None),
                1 => Ok(addresses[0].association_id().map(|id| id.to_owned())),
                _ => Err(Error::MultipleAddressesAssociatedToEip),
            }?;

            if let Some(id) = association_id {
                crate::aws::disassociate_eip(&self.ec2_client, &id).await?;
            } else {
                info!("EIP {} was already disassociated", name);
            }
            status.eni = None;
            status.private_ip_address = None;
            crate::eip::update_status(&eip_api, name, &status).await?;
        }

        // Find new resource to associate with there is no current claim
        let mut associatiate_with_ip: Option<&str> = None;
        let mut associatiate_with_node: Option<Node> = None;
        if !eip.attached() {
            match eip.spec.selector {
                EipSelector::Node { selector: _ } => match matched_nodes.len() {
                    0 => {
                        warn!("Eip {} matches no nodes", name);
                    }
                    _ => {
                        let node_name = matched_nodes[0].name_unchecked();
                        info!("Eip {} matches node {}, updating", name, node_name,);
                        associatiate_with_node = Some(matched_nodes[0].clone());
                        associatiate_with_ip =
                            Some(matched_nodes[0].ip().ok_or(Error::MissingNodeIp)?);
                        if matched_nodes.len() > 1 {
                            warn!(
                                "Eip {} matches multiple nodes - {}, choosing the first",
                                name,
                                matched_nodes
                                    .iter()
                                    .map(|node| node.name_unchecked())
                                    .collect::<Vec<String>>()
                                    .join(",")
                            );
                        }
                    }
                },
                EipSelector::Pod { pod_name: _ } => match matched_pods.len() {
                    0 => {
                        warn!("Eip {} matches no pods", name);
                    }
                    1 => {
                        info!(
                            "Eip {} matches pod {}, updating",
                            name,
                            matched_pods[0].name_unchecked()
                        );
                        let node = node_api
                            .get_opt(
                                matched_pods[0]
                                    .node_name()
                                    .ok_or(Error::MissingPodNodeName)?,
                            )
                            .await?
                            .ok_or(Error::MissingNode)?;
                        associatiate_with_node = Some(node);
                        if let Some(ip) = matched_pods[0].ip() {
                            associatiate_with_ip = Some(ip);
                        } else {
                            // This is a case where we've found a pod but it has yet to be
                            // scheduled, we need to retry
                            return Ok(Some(Action::requeue(Duration::from_secs(1))));
                        }
                    }
                    _ => {
                        error!(
                            "Eip {} matches multiple pods - {}",
                            name,
                            matched_pods
                                .iter()
                                .map(|pod| pod.name_unchecked())
                                .collect::<Vec<String>>()
                                .join(",")
                        );
                    }
                },
            }
        }

        match (associatiate_with_node, associatiate_with_ip) {
            (Some(node), Some(ip)) => {
                status.eni = Some(
                    eip.associate_with_node_and_ip(&self.ec2_client, &node, ip)
                        .await?,
                );
                status.private_ip_address = Some(ip.to_owned());
                info!(
                    "Eip {} has been successfully associated after reconciliation",
                    eip.name().unwrap()
                );
            }
            (None, None) => {
                info!("Eip {} is correctly associated!", eip.name_unchecked());
            }
            (_, _) => {
                // this should not be possible
                error!("Bad state, need both node and eip to associate");
            }
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
