use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::Client;
use tracing::{event, instrument, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;
use crate::kube_ext::NodeExt;

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
impl eip_operator_shared::controller::Context for Context {
    type Resource = Node;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/disassociate_node";

    #[instrument(skip(self, client, _api, node), err)]
    async fn apply(
        &self,
        client: Client,
        _api: Api<Self::Resource>,
        node: &Self::Resource,
    ) -> Result<(), Self::Error> {
        let name = node.metadata.name.as_ref().ok_or(Error::MissingNodeName)?;
        event!(Level::INFO, name = %name, "Applying node.");

        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );

        let node_ip = node.ip().ok_or(Error::MissingNodeIp)?;
        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let provider_id = node.provider_id().ok_or(Error::MissingProviderId)?;
        let instance_id = provider_id
            .rsplit_once('/')
            .ok_or(Error::MalformedProviderId)?
            .1;
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips
            .into_iter()
            .find(|eip| eip.matches_node(node_labels))
            .ok_or(Error::NoEipResourceWithThatNodeSelector)?;
        let eip_name = eip.name().ok_or(Error::MissingEipName)?;
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
        if eip_description.network_interface_id != Some(eni_id.to_owned())
            || eip_description.private_ip_address != Some(node_ip.to_owned())
        {
            crate::aws::associate_eip(&self.ec2_client, allocation_id, &eni_id, node_ip).await?;
        }
        crate::eip::set_status_attached(&eip_api, eip_name, &eni_id, node_ip).await?;

        Ok(())
    }

    #[instrument(skip(self, client, _api, node), err)]
    async fn cleanup(
        &self,
        client: Client,
        _api: Api<Self::Resource>,
        node: &Self::Resource,
    ) -> Result<(), Self::Error> {
        let name = node.metadata.name.as_ref().ok_or(Error::MissingNodeName)?;
        event!(Level::INFO, name = %name, "Cleaning up node.");

        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );

        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips
            .into_iter()
            .filter(|eip| eip.attached())
            .find(|eip| eip.matches_node(node_labels));
        if let Some(eip) = eip {
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
            crate::eip::set_status_detached(
                &eip_api,
                eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?,
            )
            .await?;
        }
        Ok(())
    }
}
