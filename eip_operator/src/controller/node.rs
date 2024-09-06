use std::time::Duration;

use k8s_openapi::api::core::v1::Node;
use kube::api::{Api, ListParams};
use kube::error::ErrorResponse;
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{event, info, instrument, warn, Level};

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

        // Check the existing Node for EIP association and ready status.
        // Node's that go in to a "NotReady" or "Unknown" state should have their EIP
        // disassociated to allow a new node to spawn and use the EIP.
        let node_condition_ready_status = node
            .status
            .as_ref()
            .and_then(|status| status.conditions.as_ref())
            .and_then(|conditions| conditions.iter().find(|c| c.type_ == "Ready"))
            .map(|condition| condition.status.clone())
            .ok_or(Error::MissingNodeReadyCondition)?;
        match node_condition_ready_status.as_str() {
            // Remove the EIP from nodes with an Unknown or NotReady ready status.
            // An Unknown ready status could mean the node is unresponsive or experienced a hardware failure.
            // A NotReady ready status could mean the node is experiencing a network issue.
            "Unknown" | "False" => {
                // Skip disassociation if no EIP is not associated with the node.
                let node_eip = matched_eips.iter().find(|eip| {
                    eip.status.as_ref().is_some_and(|s| {
                        s.resource_id.is_some()
                            && s.resource_id.as_ref().map(|r| r == &name).unwrap_or(false)
                    })
                });
                if let Some(eip) = node_eip {
                    let node_eip_name = eip.name_unchecked();
                    warn!(
                        "Node {} is in an unknown state, disassociating EIP {}",
                        &name.clone(),
                        &node_eip_name
                    );
                    crate::aws::disassociate_eip(&self.ec2_client, &node_eip_name).await?;
                    crate::eip::set_status_detached(&eip_api, &eip).await?;

                    return Ok(None);
                }
            }
            // Skip Ready Status True and continue with EIP node association.
            _ => {}
        }

        let eip = matched_eips.into_iter().find(|eip| {
            eip.status.as_ref().is_some_and(|s| {
                s.resource_id.is_none()
                    || s.resource_id.as_ref().map(|r| r == &name).unwrap_or(false)
            })
        });
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
        // Ensure only node's marked with ready status True are associated with an EIP.
        // We don't want to associate an EIP with a node that is not ready and potentially remove it
        // in the next reconciliation loop if the node is still coming online.
        if (eip_description.network_interface_id != Some(eni_id.to_owned())
            || eip_description.private_ip_address != Some(node_ip.to_owned()))
            && node_condition_ready_status == "True"
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
        Ok(None)
    }
}
