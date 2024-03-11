use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{event, info, instrument, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;

pub(crate) struct Context {
    namespace: Option<String>,
}

impl Context {
    pub(crate) fn new(namespace: Option<String>) -> Self {
        Self { namespace }
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
        let name = node.metadata.name.as_ref().ok_or(Error::MissingNodeName)?;
        event!(Level::INFO, name = %name, "Applying node.");
        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );
        let node_labels = node.labels();
        let eips = eip_api.list(&ListParams::default()).await?.items;
        // Cases
        // there are some eips
        // - we are associated with one
        //   - it is correct
        //     - done - register dns labels - ok
        //   - it is incorrect
        //     - ask eip to reoncile, reschedule
        // - we are not associated with any
        //   - we want to be
        //     - an eip is not associated (potentially free to grab)
        //       - we ask to claim it
        //     - it is associated (someone else is using it)
        //       - done ok
        //   - we do not want to be
        //     - done ok
        // no eips
        // - done ok
        if !eips.is_empty() {
            // there are eips
            if let Some(eip) = eips.iter().find(|eip| eip.associated_with_node(node)) {
                // we are associated with an eip
                if eip.matches_node_labels(node_labels) {
                    // we are associated and it is correct
                    info!(
                        "Node {} is already attached to EIP {}",
                        node.name_unchecked(),
                        eip.name_unchecked()
                    );
                    return Ok(None);
                } else {
                    // we are associated and it is incorrect
                    info!(
                        "Node {} is attached to EIP {}, but should not be, asking eip to reconcile",
                        node.name_unchecked(),
                        eip.name_unchecked()
                    );
                    crate::eip::trigger_reconciliation(&eip_api, &eip.name_unchecked()).await?;
                    return Ok(None);
                }
            } else {
                // we are not associated
                if let Some(eip) = eips
                    .iter()
                    .find(|eip| eip.matches_node_labels(node_labels) && !eip.attached())
                {
                    // we have an eip we want to associate with
                    info!(
                        "Node {} found a matching unattached EIP {}, asking it to reconcile",
                        node.name_unchecked(),
                        eip.name_unchecked()
                    );
                    crate::eip::trigger_reconciliation(&eip_api, &eip.name_unchecked()).await?;
                    Ok(None)
                } else {
                    // There are no eips we want to associate with or none are free
                    info!(
                        "No matching unattached EIPs for Node {}",
                        node.name_unchecked()
                    );
                    return Ok(None);
                }
            }
        } else {
            // there are no eips
            info!(
                "No matching unattached EIPs for Node {}",
                node.name_unchecked()
            );
            return Ok(None);
        }
    }

    #[instrument(skip(self, client, node), err)]
    async fn cleanup(
        &self,
        client: Client,
        node: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        // remove claim if one exists
        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );
        let eip = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .find(|eip| eip.associated_with_node(node));
        if let Some(eip) = eip {
            info!(
                "Node {} still attached to EIP {}, asking eip to re-reconcile",
                node.name_unchecked(),
                eip.name_unchecked()
            );
            crate::eip::trigger_reconciliation(&eip_api, &eip.name_unchecked()).await?;
            for _ in 0..5 {
                let eip = eip_api.get_opt(&eip.name_unchecked()).await?;
                if let Some(eip) = eip {
                    if !eip.associated_with_node(node) {
                        info!(
                            "Node {} has been disassociated with EIP {}",
                            node.name_unchecked(),
                            eip.name_unchecked()
                        );
                        return Ok(None);
                    }
                } else {
                    info!(
                        "Node {} no longer has an attached eip",
                        node.name_unchecked()
                    );
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            Err(Error::NodeFailedToRemoveEip)
        } else {
            info!(
                "Node {} is not attached to any EIP, cleanup successful",
                node.name_unchecked()
            );
            Ok(None)
        }
    }
}
