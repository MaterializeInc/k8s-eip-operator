use crate::eip::v2::Eip;
use crate::Error;
use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams, Patch, PatchParams};
use std::collections::{BTreeMap, BTreeSet};
use tracing::{info, instrument};

use crate::EGRESS_GATEWAY_NODE_SELECTOR_LABEL_KEY;
use crate::EGRESS_GATEWAY_NODE_SELECTOR_LABEL_VALUE;

/// Applies label to node specifying the status of the egress gateway node.
#[instrument(skip(api), err)]
async fn add_gateway_status_label(
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

/// Update state label on egress nodes.
/// Note: Egress traffic will be immediately dropped when the status label is changed away from "true".
#[instrument(skip(), err)]
pub(crate) async fn label_egress_nodes(
    eip_api: &Api<Eip>,
    node_api: &Api<Node>,
) -> Result<(), Error> {
    info!("Updating egress node status labels.");
    let node_list = get_egress_nodes(&Api::all(node_api.clone().into())).await?;
    if node_list.is_empty() {
        info!("No egress nodes found. Skipping egress cleanup.");
        return Ok(());
    }

    let node_names_and_status: BTreeMap<String, String> =
        crate::node::get_nodes_ready_status(node_list)?;
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
                    &_ => {
                        info!("Ignoring node {} with status {}", name, status);
                    }
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

    if matched_ready_nodes_with_eip.is_empty() {
        info!("No ready egress nodes found with EIPs. Skipping egress labeling.");
        return Ok(());
    }

    info!(
        "Found ready egress nodes with EIPs: {:?}",
        matched_ready_nodes_with_eip
    );
    // Set egress status for nodes ready with an EIP attached.
    for node_name in nodes_status_ready {
        add_gateway_status_label(node_api, &node_name, "ready").await?;
    }
    // Attempt cleanup of nodes in a ready state of `Unknown` if another node is ready with an EIP.
    for node_name in nodes_status_unknown {
        add_gateway_status_label(node_api, &node_name, "unknown").await?;
    }

    Ok(())
}
