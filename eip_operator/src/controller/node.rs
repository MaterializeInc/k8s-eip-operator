use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::Client;
use kube_runtime::controller::Action;
use tracing::{event, info, instrument, Level};

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

        let node_ip = node.ip().ok_or(Error::MissingNodeIp)?;
        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let provider_id = node.provider_id().ok_or(Error::MissingProviderId)?;
        let instance_id = provider_id
            .rsplit_once('/')
            .ok_or(Error::MalformedProviderId)?
            .1;
        let all_eips: Vec<Eip> = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|eip| eip.matches_node(node_labels))
            .collect();
        if all_eips.is_empty() {
            return Err(Error::NoEipResourceWithThatNodeSelector);
        }
        let eip = all_eips
            .into_iter()
            .find(|eip| eip.status.as_ref().is_some_and(|s| s.resource_id.is_none()));
        if eip.is_none() {
            info!("No un-associated eips found for node {}", name);
            return Ok(None);
        }
        let eip = eip.unwrap();
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
            match crate::eip::set_status_attached(&eip_api, &eip, &eni_id, node_ip, name).await {
                Ok(_) => {
                    info!("Found matching Eip, claiming it");
                    crate::aws::associate_eip(&self.ec2_client, allocation_id, &eni_id, node_ip)
                        .await?;
                }
                Err(err)
                    if err
                        .to_string()
                        .contains("Operation cannot be fulfilled on eips.materialize.cloud") =>
                {
                    info!(
                        "Pod {} failed to claim eip {}, rescheduling to try another",
                        name, eip_name
                    );
                    return Ok(Some(Action::requeue(Duration::from_secs(1))));
                }
                Err(e) => return Err(e),
            };
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

        let node_labels = node.labels().ok_or(Error::MissingNodeLabels)?;
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        let eip = all_eips
            .into_iter()
            .filter(|eip| eip.attached())
            .find(|eip| {
                eip.matches_node(node_labels)
                    && eip
                        .status
                        .as_ref()
                        .is_some_and(|s| s.resource_id == Some(node.metadata.name.clone().unwrap()))
            });
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
        Ok(None)
    }
}
