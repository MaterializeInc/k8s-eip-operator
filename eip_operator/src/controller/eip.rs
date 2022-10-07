use std::collections::HashMap;

use kube::api::Api;
use kube::{Client, ResourceExt};
use tracing::{event, instrument, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;

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
impl eip_operator_shared::controller::Context for Context {
    type Res = Eip;
    type Err = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/destroy";

    #[instrument(skip(self, _client, api, eip), err)]
    async fn apply(
        &self,
        _client: Client,
        api: Api<Self::Res>,
        eip: &Self::Res,
    ) -> Result<(), Self::Err> {
        let uid = eip.metadata.uid.as_ref().ok_or(Error::MissingEipUid)?;
        let name = eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
        let selector = &eip.spec.selector;
        event!(Level::INFO, %uid, %name, %selector, "Applying EIP.");
        let addresses = crate::aws::describe_addresses_with_tag_value(
            &self.ec2_client,
            crate::aws::EIP_UID_TAG,
            uid,
        )
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?;
        let (allocation_id, public_ip) = match addresses.len() {
            0 => {
                let response = crate::aws::allocate_address(
                    &self.ec2_client,
                    uid,
                    name,
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
        crate::eip::set_status_created(&api, name, &allocation_id, &public_ip).await?;
        Ok(())
    }

    #[instrument(skip(self, _client, _api, eip), err)]
    async fn cleanup(
        &self,
        _client: Client,
        _api: Api<Self::Res>,
        eip: &Self::Res,
    ) -> Result<(), Self::Err> {
        let name = eip.metadata.name.as_ref().ok_or(Error::MissingEipName)?;
        let uid = eip.metadata.uid.as_ref().ok_or(Error::MissingEipUid)?;
        event!(Level::INFO, name = %name, uid = %uid, "Cleaning up eip.");
        let addresses = crate::aws::describe_addresses_with_tag_value(
            &self.ec2_client,
            crate::aws::EIP_UID_TAG,
            uid,
        )
        .await?
        .addresses;
        if let Some(addresses) = addresses {
            for address in addresses {
                crate::aws::disassociate_and_release_address(&self.ec2_client, &address).await?;
            }
        }
        Ok(())
    }
}
