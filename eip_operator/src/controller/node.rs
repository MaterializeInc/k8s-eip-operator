use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::error::ErrorResponse;
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{debug, event, info, instrument, warn, Level};

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

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(skip(self, client, node), err)]
    async fn apply(
        &self,
        client: Client,
        node: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let name = node.name_unchecked();
        event!(Level::INFO, name = %name, "Applying node.");

        let eip_api = Api::<Eip>::namespaced(
            client.clone(),
            self.namespace.as_deref().unwrap_or("default"),
        );

        let node_ip = node.ip().ok_or(Error::MissingNodeIp)?;
        let node_labels = node.labels();
        let provider_id = node.provider_id().ok_or(Error::MissingProviderId)?;
        let instance_id = provider_id
            .rsplit_once('/')
            .ok_or(Error::MalformedProviderId)?
            .1;

        let matched_eips: Vec<Eip> = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .filter(|eip| eip.matches_node(node_labels))
            .collect();
        if matched_eips.is_empty() {
            return Err(Error::NoEipResourceWithThatNodeSelector);
        }

        let eip = matched_eips.into_iter().find(|eip| {
            eip.status.as_ref().is_some_and(|s| {
                s.resource_id.is_none()
                    || s.resource_id.as_ref().map(|r| r == &name).unwrap_or(false)
            })
        });

        let node_condition_ready_status = node
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
            .map(|condition| condition.status.clone())
            .ok_or(Error::MissingNodeReadyCondition)?;

        // Check the Node's EIP claim and ready status.
        // Node's with a ready status of "False" or "Unknown" should not have a claim to an EIP.
        if node_condition_ready_status != "True" {
            // If an EIP has a resource id label pointing to this node, remove that label releasing this nodes claim to the EIP
            if let Some(eip) = eip {
                warn!(
                    "Node {} is in an unresponsive state, setting status detached on EIP {}",
                    &name.clone(),
                    &eip.name_unchecked()
                );
                crate::eip::set_status_detached(&eip_api, &eip).await?;
            }

            debug!("Node {} is not ready, skipping EIP claim", &name);
            return Ok(None);
        }

        let Some(eip) = eip else {
            info!("No un-associated eips found for node {}", &name);
            return Ok(None);
        };
        let eip_name = eip.name_unchecked();
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
            match crate::eip::set_status_should_attach(&eip_api, &eip, &eni_id, node_ip, &name)
                .await
            {
                Ok(_) => {
                    info!("Found matching Eip, attaching it");
                    let association_id = crate::aws::associate_eip(
                        &self.ec2_client,
                        allocation_id,
                        &eni_id,
                        node_ip,
                    )
                    .await?
                    .association_id
                    .ok_or(Error::MissingAssociationId)?;
                    crate::eip::set_status_association_id(&eip_api, &eip_name, &association_id)
                        .await?;
                }
                Err(Error::Kube {
                    source: kube::Error::Api(ErrorResponse { reason, .. }),
                }) if reason == "Conflict" => {
                    warn!(
                        "Node {} failed to claim eip {}, rescheduling to try another",
                        name, eip_name
                    );
                    return Ok(Some(Action::requeue(Duration::from_secs(3))));
                }
                Err(e) => return Err(e),
            };
        }
        Ok(None)
    }

    #[allow(clippy::blocks_in_conditions)]
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
        let node_labels = node.labels();
        let all_eips = eip_api.list(&ListParams::default()).await?.items;
        // find all eips that match (there should be one, but lets not lean on that)
        let matched_eips = all_eips.into_iter().filter(|eip| {
            eip.matches_node(node_labels)
                && eip
                    .status
                    .as_ref()
                    .is_some_and(|s| s.resource_id == Some(node.name_unchecked().clone()))
        });
        for eip in matched_eips {
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
            crate::eip::set_status_detached(&eip_api, &eip).await?;
        }

        let node_api = Api::<Node>::all(client);
        crate::egress::add_gateway_status_label(&node_api, node.name_unchecked().as_str(), "false")
            .await?;

        Ok(None)
    }
}
