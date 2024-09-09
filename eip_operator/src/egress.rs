use crate::eip::v2::Eip;
use crate::Error;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams, Patch, PatchParams};
use std::collections::{BTreeMap, BTreeSet};
use tracing::{info, instrument};

use crate::EGRESS_GATEWAY_NODE_SELECTOR_LABEL_KEY;
use crate::EGRESS_GATEWAY_NODE_SELECTOR_LABEL_VALUE;

/// Applies label specifying the ready status of the egress gateway node.
#[instrument(skip(api), err)]
pub(crate) async fn add_gateway_status_label(
    api: &Api<Node>,
    name: &str,
    status: &str,
) -> Result<Node, kube::Error> {
    info!(
        "Adding gateway status label {} value {} to node {}",
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

/// Update the ready status label on egress nodes.
/// This controls whether traffic is allowed to egress through the node.
/// Note: Egress traffic will be immediately dropped when the ready status label value is changed away from "true".
#[instrument(skip(), err)]
pub(crate) async fn label_egress_nodes(
    eip_api: &Api<Eip>,
    node_api: &Api<Node>,
) -> Result<(), Error> {
    let egress_nodes = get_egress_nodes(&Api::all(node_api.clone().into())).await?;
    if egress_nodes.is_empty() {
        info!("No egress nodes found. Skipping egress node ready status labeling.");
        return Ok(());
    }

    // Build up a list of EIP resourceId's to check EIP attachment against node names.
    let eip_resource_ids: BTreeSet<String> = eip_api
        .list(&ListParams::default())
        .await?
        .items
        .into_iter()
        .filter_map(|eip| eip.status.and_then(|s| s.resource_id))
        .collect();

    // Build up a map of egress node names along with their
    // ready status and whether or not they have an EIP attached.
    let mut node_map: BTreeMap<String, (String, bool)> = BTreeMap::new();
    for node in egress_nodes {
        if let Some(ref node_name) = node.metadata.name {
            let ready_status = node
                .status
                .as_ref()
                .and_then(|status| status.conditions.as_ref())
                .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
                .map(|condition| condition.status.clone())
                .ok_or(Error::MissingNodeReadyCondition)?;
            let eip_attached_status = eip_resource_ids.contains(node_name.as_str());
            node_map.insert(node_name.clone(), (ready_status, eip_attached_status));
        }
    }

    let egress_nodes_ready_with_eip: BTreeMap<String, (String, bool)> = node_map
        .iter()
        .filter(|(_, &(ref ready_status, eip_attached_status))| {
            ready_status == "True" && eip_attached_status
        })
        .map(|(node_name, &(ref ready_status, eip_attached_status))| {
            (
                node_name.clone(),
                (ready_status.clone(), eip_attached_status),
            )
        })
        .collect();

    // Wait to label egress nodes until an EIP is attached.
    // Setting the ready status label to "ready" routes egress traffic through the node
    // Wait to label egress nodes until they are ready and have an EIP attached.
    if egress_nodes_ready_with_eip.is_empty() {
        info!(
            "No egress nodes found with a ready status and attached EIP. Skipping egress labeling."
        );
        return Ok(());
    }

    // At least one egress node should be ready with an EIP attached in this current implementation.
    // This allows the ready status label to be set and traffic to be routed through the node.
    assert!(egress_nodes_ready_with_eip.len() == 1);
    if let Some((first_key, _)) = egress_nodes_ready_with_eip.first_key_value() {
        let node_name = first_key.clone();
        add_gateway_status_label(node_api, node_name.as_str(), "true").await?;
    }

    // We immediately disassociate EIPs from egress nodes that are in an
    // unresponsive state in the eip node controller.
    // This allows the EIP to be re-associated with a new healthy node.
    // However, unresponsive nodes may still exist with a ready-status egress label
    // set to "true". This allows the old node to still serve traffic if possible until a
    // new node is ready to take over.
    // Clean up unresponsive egress nodes in a ready state of `Unknown` if another node is ready with an EIP.
    // Egress nodes in a ready state of "False" are not re-labelled since they should be able to be cleaned
    // up by normal methods.
    let egress_nodes_not_ready_without_eip: BTreeMap<String, (String, bool)> = node_map
        .iter()
        .filter(|(_, &(ref ready_status, eip_attached_status))| {
            (ready_status == "Unknown") && !eip_attached_status
        })
        .map(|(node_name, &(ref ready_status, eip_attached_status))| {
            (
                node_name.clone(),
                (ready_status.clone(), eip_attached_status),
            )
        })
        .collect();
    for node_name in egress_nodes_not_ready_without_eip.keys() {
        add_gateway_status_label(node_api, node_name.as_str(), "false").await?;
    }

    Ok(())
}
