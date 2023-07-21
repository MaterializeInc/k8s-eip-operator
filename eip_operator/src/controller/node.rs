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
        // Find an EIP and claim if not claimed
        let name = node.metadata.name.as_ref().ok_or(Error::MissingNodeName)?;
        event!(Level::INFO, name = %name, "Applying node.");
        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );
        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips
            .into_iter()
            .find(|eip| eip.matches_node_labels(node_labels) && eip.status.is_some())
            .ok_or(Error::NoEipResourceWithThatNodeSelector)?;
        // do nothing if eip already claimed
        let eip_name = eip.name().ok_or(Error::MissingEipName)?;
        if eip.claimed() {
            info!(
                "Found claimed ip {} matching node {}, skipping",
                eip_name, name,
            );
            return Ok(None);
        };
        let eip_name = eip.name().ok_or(Error::MissingEipName)?;
        // ensure there's an allocation id before claiming
        let _allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
        crate::eip::set_status_claimed(&eip_api, eip_name, name).await?;
        Ok(None)
    }

    #[instrument(skip(self, client, node), err)]
    async fn cleanup(
        &self,
        client: Client,
        node: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        // remove claim
        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );
        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips
            .into_iter()
            .filter(|eip| eip.attached())
            .find(|eip| eip.matches_node_labels(node_labels));
        if let Some(eip) = eip {
            crate::eip::set_status_detached(&eip_api, eip.name().ok_or(Error::MissingEipName)?)
                .await?;
        }
        Ok(None)
    }
}
