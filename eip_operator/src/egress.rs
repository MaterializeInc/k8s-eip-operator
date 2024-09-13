use crate::eip::v2::Eip;
use crate::Error;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, Patch, PatchParams};
use kube::ResourceExt;
use tracing::{info, instrument};

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

/// Update the ready status label on egress nodes.
/// This controls whether traffic is allowed to egress through the node.
/// Note: Egress traffic will be immediately dropped when the ready status label value is changed away from "true".
#[instrument(skip(), err)]
pub(crate) async fn label_egress_nodes(eip: &Eip, node_api: &Api<Node>) -> Result<(), Error> {
    let egress_nodes = node_api.list(&eip.get_resource_list_params()).await?.items;
    if egress_nodes.is_empty() {
        info!("No egress nodes found. Skipping egress node ready status labeling.");
        return Ok(());
    }

    // If EIP being reconciled has a resource id, find it's node, and check if it's ready.
    // If not ready, return early.
    // If ready, mark that node as egress_ready=true, and mark all other nodes as egress_ready=false.
    if let Some(resource_id) = eip.status.as_ref().and_then(|s| s.resource_id.as_ref()) {
        let node = egress_nodes
            .iter()
            .find(|node| {
                node.metadata
                    .name
                    .as_ref()
                    .map(|n| n == resource_id)
                    .unwrap_or(false)
            })
            .ok_or(Error::MissingNodeName)?;
        let node_ready_status = node
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
            .map(|condition| condition.status.clone())
            .ok_or(Error::MissingNodeReadyCondition)?;
        if node_ready_status != "True" {
            return Ok(());
        } else {
            // mark node with EIP as ready for egress traffic
            add_gateway_status_label(node_api, node.name_unchecked().as_str(), "true").await?;
            // mark all other nodes as not ready for egress traffic
            for other_node in &egress_nodes {
                // skip the node we just set to ready
                if other_node.name_unchecked() != node.name_unchecked() {
                    add_gateway_status_label(
                        node_api,
                        other_node.name_unchecked().as_str(),
                        "false",
                    )
                    .await?;
                }
            }
        }
    }

    Ok(())
}
