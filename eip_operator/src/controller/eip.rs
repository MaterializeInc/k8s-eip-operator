use rand::Rng;
use std::collections::HashMap;

use kube::api::{Api, PatchParams};
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{info, instrument, warn};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;
use crate::eip::EipStatus;

pub(crate) struct Context {
    ec2_client: aws_sdk_ec2::Client,
    cluster_name: String,
    default_tags: HashMap<String, String>,
}

impl Context {
    pub(crate) fn new(
        ec2_client: aws_sdk_ec2::Client,
        cluster_name: String,
        default_tags: HashMap<String, String>,
    ) -> Self {
        Self {
            ec2_client,
            cluster_name,
            default_tags,
        }
    }
}

#[async_trait::async_trait]
impl k8s_controller::Context for Context {
    type Resource = Eip;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/destroy";

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(skip(self, client, eip), err)]
    async fn apply(
        &self,
        client: Client,
        eip: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let eip_api = Api::namespaced(client.clone(), &eip.namespace().unwrap());

        let uid = eip.uid().ok_or(Error::MissingEipUid)?;
        let name = eip.name_unchecked();
        let selector = &eip.spec.selector;
        let addresses = crate::aws::describe_addresses_with_tag_value(
            &self.ec2_client,
            crate::aws::EIP_UID_TAG,
            &uid,
        )
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?;
        let (allocation_id, public_ip) = match addresses.len() {
            0 => {
                let response = crate::aws::allocate_address(
                    &self.ec2_client,
                    &uid,
                    &name,
                    selector,
                    &self.cluster_name,
                    &eip.namespace().unwrap(),
                    &self.default_tags,
                )
                .await?;
                let allocation_id = response.allocation_id.ok_or(Error::MissingAllocationId)?;
                let public_ip = response.public_ip.ok_or(Error::MissingPublicIp)?;
                (allocation_id, public_ip)
            }
            1 => {
                let allocation_id = addresses[0]
                    .allocation_id
                    .as_ref()
                    .ok_or(Error::MissingAllocationId)?;
                let public_ip = addresses[0]
                    .public_ip
                    .as_ref()
                    .ok_or(Error::MissingPublicIp)?;
                (allocation_id.to_owned(), public_ip.to_owned())
            }
            _ => {
                return Err(Error::MultipleEipsTaggedForPod);
            }
        };
        crate::eip::set_status_created(&eip_api, &name, &allocation_id, &public_ip).await?;

        // Eip has been created we should now,
        // - detach from any resources which should no longer associate with it
        // - attempt to find resource which can associate with it
        let status = eip.status.clone().unwrap_or(EipStatus {
            allocation_id: Some(allocation_id),
            public_ip_address: Some(public_ip),
            ..Default::default()
        });

        let resource_api = eip.get_resource_api(&client);
        let matched_resources = resource_api
            .list(&eip.get_resource_list_params())
            .await?
            .items;

        if let Some(ref associated_resource_id) = status.resource_id {
            // Eip's status says it is associated with something
            // we should ensure that it still matches us
            if matched_resources
                .iter()
                .any(|r| &r.name_any() == associated_resource_id)
            {
                // association is correct
                info!(
                    "Eip {} correctly associated with matched resource {}",
                    name, associated_resource_id
                );
                return Ok(None);
            } else {
                // association is incorrect
                info!(
                    "Eip {} incorrectly associated with matched resource {}, disassociating",
                    name, associated_resource_id
                );
                if let Some(association_id) = status.association_id {
                    crate::aws::disassociate_eip(&self.ec2_client, &association_id).await?;
                }
                crate::eip::set_status_detached(&eip_api, eip).await?;
            }
        }

        // Not associated, ask matched resources to associate
        info!(
            "Eip apply for {} Found matched {} resources",
            name,
            matched_resources.len()
        );
        for resource in matched_resources {
            info!(
                "Updating eip refresh label for {}",
                resource.name_unchecked()
            );
            let data = resource.clone().data(serde_json::json!({
                "metadata": {
                        "labels":{
                           "eip.materialize.cloud/refresh":  format!("{}",rand::thread_rng().gen::<u64>()),

                        }
                }
            }));
            if let Err(err) = resource_api
                .patch_metadata(
                    &resource.name_unchecked(),
                    &PatchParams::default(),
                    &kube::core::params::Patch::Merge(serde_json::json!(data)),
                )
                .await
            {
                warn!(
                    "Failed to patch resource {} refresh label for {}: err {:?}",
                    resource.name_unchecked(),
                    name,
                    err
                );
            };
        }

        Ok(None)
    }

    #[allow(clippy::blocks_in_conditions)]
    #[instrument(skip(self, _client, eip), err)]
    async fn cleanup(
        &self,
        _client: Client,
        eip: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        let uid = eip.uid().ok_or(Error::MissingEipUid)?;
        let addresses = crate::aws::describe_addresses_with_tag_value(
            &self.ec2_client,
            crate::aws::EIP_UID_TAG,
            &uid,
        )
        .await?
        .addresses;
        if let Some(addresses) = addresses {
            for address in addresses {
                crate::aws::disassociate_and_release_address(&self.ec2_client, &address).await?;
            }
        }
        Ok(None)
    }
}
