use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::Client;
use kube_runtime::controller::Action;
use tracing::{event, info, instrument, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;
use crate::kube_ext::NodeExt;

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
        // Find an EIP and claim if not claimed
        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let eip = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .find(|eip| eip.matches_node_labels(node_labels) && eip.status.is_some())
            .ok_or(Error::NoEipResourceWithThatNodeSelector)?;
        let eip_name = eip.name().ok_or(Error::MissingEipName)?;

        // Claim it if unclaimed
        if !eip.claimed() {
            let _allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
            crate::eip::set_status_claimed(&eip_api, eip_name, name).await?;
            Ok(None)
        } else {
            if eip.claimed_by(name) {
                info!(
                    "Found EIP {} already appropriately claimed by node {}",
                    eip_name, name,
                );
            } else {
                info!(
                    "Found EIP {} matching node {}, but claimed by {}",
                    eip_name,
                    name,
                    eip.claim().ok_or(Error::MissingEipClaim)?
                );
            }
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
        let name = node.metadata.name.as_ref().ok_or(Error::MissingNodeName)?;
        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );
        let eip = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|eip| eip.attached())
            .find(|eip| eip.claimed_by(name));
        if let Some(eip) = eip {
            crate::eip::set_status_unclaimed(&eip_api, eip.name().ok_or(Error::MissingEipName)?)
                .await?;
        }
        Ok(None)
    }
}
