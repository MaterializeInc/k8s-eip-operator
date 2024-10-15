use crate::eip::v2::Eip;
use crate::Error;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::ResourceExt;
use kube_runtime::reflector::Lookup;
use tracing::{info, instrument};

/// Adds label specifying the ready status of the egress gateway node.
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

/// Removes label specifying the ready status of the egress gateway node.
#[instrument(skip(api), err)]
pub(crate) async fn remove_gateway_status_label(
    api: &Api<Node>,
    name: &str,
    status: &str,
) -> Result<Node, kube::Error> {
    info!(
        "Removing gateway status label {} value {} to node {}",
        crate::EGRESS_NODE_STATUS_LABEL,
        status,
        name
    );
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Node",
        "metadata": {
            "labels": {
                crate::EGRESS_NODE_STATUS_LABEL: serde_json::Value::Null
            }
        }
    });
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(crate::FIELD_MANAGER);
    api.patch(name, &params, &patch).await
}

/// Update the ready status label on egress nodes.
/// If a node is provided it's ready status label will be set
/// All other nodes with a matching ready status label will have their
/// labels removed.
/// This controls whether traffic is allowed to egress through the node.
/// Note: Egress traffic will be immediately dropped when the ready status label value is changed away from "true".
#[instrument(skip(), err)]
pub(crate) async fn label_egress_nodes_ready(
    eip: &Eip,
    node: Option<Node>,
    node_api: &Api<Node>,
) -> Result<(), Error> {
    // We'll look for nodes with ready labels where the value is the eips
    // allocation id. This ensures that we only adjust nodes using this eip.
    // There should at most be one other, but we'll remove all incase something has
    // gone awry.
    let egress_nodes: Vec<Node> = node_api
        .list(&ListParams {
            label_selector: Some(format!(
                "{}={}",
                crate::EGRESS_NODE_STATUS_LABEL,
                eip.allocation_id()
            )),
            ..Default::default()
        })
        .await?
        .items
        .into_iter()
        .filter(|n| node.map(|claimed_node| claimed_node.id() != n.uid()))
        .collect();

    if egress_nodes.is_empty() {
        info!("No egress nodes found. Skipping egress node ready status labeling.");
        return Ok(());
    }
    if let Some(node) = node {
        add_gateway_status_label(
            node_api,
            node.name_unchecked().as_str(),
            eip.allocation_id(),
        )
        .await?;
    }
    for node in egress_nodes {
        remove_gateway_status_label(
            node_api,
            node.name_unchecked().as_str(),
            eip.allocation_id(),
        )
        .await?;
    }
    Ok(())
}
