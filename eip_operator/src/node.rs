use std::collections::BTreeMap;

use eip_operator_shared::Error;
use k8s_openapi::api::core::v1::Node;

/// Get Ready status from the node status field.
pub(crate) fn get_ready_status_from_node(node: &Node) -> Option<String> {
    node.status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .find(|c| c.type_ == "Ready")
        .map(|condition| condition.status.clone())
}

/// Retrieve node names and ready status given a list of nodes.
pub(crate) fn get_nodes_ready_status(
    node_list: Vec<Node>,
) -> Result<BTreeMap<String, String>, Error> {
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
