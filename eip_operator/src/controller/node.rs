use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::error::ErrorResponse;
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{event, info, instrument, warn, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;
use crate::kube_ext::NodeExt;
use crate::EGRESS_GATEWAY_NODE_SELECTOR_LABEL_KEY;
use crate::EGRESS_GATEWAY_NODE_SELECTOR_LABEL_VALUE;

pub(crate) struct Context {
    ec2_client: aws_sdk_ec2::Client,
    namespace: Option<String>,
}

impl Context {
    pub(crate) fn new(ec2_client: aws_sdk_ec2::Client, namespace: Option<String>) -> Self {
        Self {
            ec2_client,
            namespace,
        }
    }
}

#[async_trait::async_trait]
impl k8s_controller::Context for Context {
    type Resource = Node;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/disassociate_node";

    #[instrument(skip(self, client, node), err)]
    async fn apply(
        &self,
        client: Client,
        node: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let name = node.name_unchecked();
        event!(Level::INFO, name = %name, "Applying node.");

        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );

        let node_ip = node.ip().ok_or(Error::MissingNodeIp)?;
        let node_labels = node.labels();
        let node_condition_ready_status =
            get_ready_status_from_node(node).ok_or(Error::MissingNodeReadyCondition)?;

        let provider_id = node.provider_id().ok_or(Error::MissingProviderId)?;
        let instance_id = provider_id
            .rsplit_once('/')
            .ok_or(Error::MalformedProviderId)?
            .1;
        let matched_eips: Vec<Eip> = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|eip| eip.matches_node(node_labels))
            .collect();
        if matched_eips.is_empty() {
            return Err(Error::NoEipResourceWithThatNodeSelector);
        }
        let eip = matched_eips.into_iter().find(|eip| {
            eip.status.as_ref().is_some_and(|s| {
                s.resource_id.is_none()
                    || s.resource_id.as_ref().map(|r| r == &name).unwrap_or(false)
            })
        });
        let Some(eip) = eip else {
            info!("No un-associated eips found for node {}", &name);
            return Ok(None);
        };
        let eip_name = eip.name_unchecked();
        let allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
        let eip_description = crate::aws::describe_address(&self.ec2_client, allocation_id)
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?
            .swap_remove(0);
        let instance_description =
            crate::aws::describe_instance(&self.ec2_client, instance_id).await?;
        let eni_id = crate::aws::get_eni_from_private_ip(&instance_description, node_ip)
            .ok_or(Error::NoInterfaceWithThatIp)?;

        let network_interface_mismatch =
            eip_description.network_interface_id != Some(eni_id.to_owned());
        let private_ip_mismatch = eip_description.private_ip_address != Some(node_ip.to_owned());
        let attach_result = match (
            network_interface_mismatch,
            private_ip_mismatch,
            node_condition_ready_status.as_str(),
        ) {
            (true, false, "True") | (false, true, "True") | (true, true, "True") => {
                crate::eip::set_status_should_attach(&eip_api, &eip, &eni_id, node_ip, &name).await
            }
            // Node is in a ready state of `unknown`. Disassociate the EIP.
            (false, false, "Unknown") => {
                // This code path runs a limited number of times once a node goes unresponsive.
                // The EIP will be disassociated and then this apply() will exit early since no EIP is associated
                warn!("Node {} is in Unknown state, disassociating EIP", &name);
                crate::aws::disassociate_eip(&self.ec2_client, &eip_name).await?;
                crate::eip::set_status_detached(&eip_api, &eip_name).await?;

                return Ok(None);
            }
            _ => return Ok(None),
        };

        match attach_result {
            Ok(_) => {
                info!("Found matching Eip, attaching it");
                let association_id =
                    crate::aws::associate_eip(&self.ec2_client, allocation_id, &eni_id, node_ip)
                        .await?
                        .association_id
                        .ok_or(Error::MissingAssociationId)?;
                crate::eip::set_status_association_id(&eip_api, &eip_name, &association_id).await?;

                add_gateway_status_label(&Api::all(client.clone()), &name, "ready").await?;
            }
            Err(Error::Kube {
                source: kube::Error::Api(ErrorResponse { reason, .. }),
            }) if reason == "Conflict" => {
                warn!(
                    "Node {} failed to claim eip {}, rescheduling to try another",
                    name, eip_name
                );
                return Ok(Some(Action::requeue(Duration::from_secs(3))));
            }
            Err(e) => return Err(e),
        }

        Ok(None)
    }

    #[instrument(skip(self, client, node), err)]
    async fn cleanup(
        &self,
        client: Client,
        node: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );
        let node_labels = node.labels();
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        // find all eips that match (there should be one, but lets not lean on that)
        let matched_eips = all_eips.into_iter().filter(|eip| {
            eip.matches_node(node_labels)
                && eip
                    .status
                    .as_ref()
                    .is_some_and(|s| s.resource_id == Some(node.name_unchecked().clone()))
        });
        for eip in matched_eips {
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
            crate::eip::set_status_detached(&eip_api, &eip.name_unchecked()).await?;
        }
        Ok(None)
    }
}

/// Applies label to node specifying the status of the egress gateway node.
#[instrument(skip(api), err)]
async fn add_gateway_status_label(
    api: &Api<Node>,
    name: &str,
    status: &str,
) -> Result<Node, kube::Error> {
    info!(
        "Adding gateway status label {} key {} to node {}",
        crate::EGRESS_NODE_STATUS_LABEL,
        status,
        name
    );
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "labels": {
                // Ensure the status is lowercase to match conventions
                crate::EGRESS_NODE_STATUS_LABEL: status.to_lowercase(),
            }
        }
    });
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(crate::FIELD_MANAGER);
    api.patch(name, &params, &patch).await
}

/// Retrieve all egress nodes in the cluster.
async fn get_egress_nodes(api: &Api<Node>) -> Result<Vec<Node>, kube::Error> {
    let params = ListParams::default().labels(
        format!(
            "{}={}",
            EGRESS_GATEWAY_NODE_SELECTOR_LABEL_KEY, EGRESS_GATEWAY_NODE_SELECTOR_LABEL_VALUE
        )
        .as_str(),
    );

    match api.list(&params).await {
        Ok(node_list) => Ok(node_list.items),
        Err(e) => Err(e),
    }
}

/// Get Ready status from the node status field.
fn get_ready_status_from_node(node: &Node) -> Option<String> {
    node.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|condition| condition.status.clone())
}

/// Retrieve node names and ready status given a list of nodes.
fn get_nodes_ready_status(node_list: Vec<Node>) -> Result<BTreeMap<String, String>, Error> {
    let mut node_ready_status_map = BTreeMap::new();

    for node in node_list {
        if let Some(ref node_name) = node.metadata.name {
            let ready_status =
                get_ready_status_from_node(&node).ok_or(Error::MissingNodeReadyCondition)?;

            node_ready_status_map.insert(node_name.to_string(), ready_status);
        }
    }

    Ok(node_ready_status_map)
}

/// Find old nodes in a ready state of `Unknown`.
/// Update their status label if another node is available.
/// Note: Egress traffic will be immediately dropped once the egress status label is changed away from "true"
#[instrument(skip(), err)]
pub(crate) async fn cleanup_old_egress_nodes(
    eip_api: &Api<Eip>,
    node_api: &Api<Node>,
) -> Result<(), Error> {
    // Gather a list of egress nodes and EIPs to check for potential cleanup.
    let node_list = get_egress_nodes(&Api::all(node_api.clone().into())).await?;
    if node_list.len() < 2 {
        info!("Less than two egress nodes found, no cleanup is possible.");
        return Ok(());
    }

    let node_names_and_status: BTreeMap<String, String> = get_nodes_ready_status(node_list)?;
    let (nodes_status_ready, nodes_status_unknown): (BTreeSet<String>, BTreeSet<String>) =
        node_names_and_status.iter().fold(
            (BTreeSet::new(), BTreeSet::new()),
            |(mut ready, mut unknown), (name, status)| {
                match status.as_str() {
                    "True" => {
                        ready.insert(name.clone());
                    }
                    "Unknown" => {
                        unknown.insert(name.clone());
                    }
                    // Ignore nodes in other states.
                    &_ => {}
                }
                (ready, unknown)
            },
        );

    // Ensure an egress node exists with an EIP and Ready state of `true`.
    let eip_resource_ids: BTreeSet<String> = eip_api
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|eip| eip.status.and_then(|s| s.resource_id))
        .collect();
    let matched_ready_nodes_with_eip: BTreeSet<String> = nodes_status_ready
        .intersection(&eip_resource_ids)
        .cloned()
        .collect();
    info!(
        "Found ready nodes with EIPs: {:?}",
        matched_ready_nodes_with_eip
    );

    // At least one node is ready with an EIP, update the status label of nodes in an unknown state.
    if !matched_ready_nodes_with_eip.is_empty() {
        for node_name in nodes_status_unknown {
            add_gateway_status_label(node_api, &node_name, "unknown").await?;
        }
    }

    Ok(())
}
