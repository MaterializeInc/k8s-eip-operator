use std::collections::HashMap;

use aws_sdk_ec2::error::SdkError;
use aws_sdk_ec2::operation::allocate_address::{AllocateAddressError, AllocateAddressOutput};
use aws_sdk_ec2::operation::associate_address::{AssociateAddressError, AssociateAddressOutput};
use aws_sdk_ec2::operation::describe_addresses::{DescribeAddressesError, DescribeAddressesOutput};
use aws_sdk_ec2::operation::describe_instances::{DescribeInstancesError, DescribeInstancesOutput};
use aws_sdk_ec2::operation::disassociate_address::DisassociateAddressError;
use aws_sdk_ec2::operation::release_address::{ReleaseAddressError, ReleaseAddressOutput};
use aws_sdk_ec2::types::{Address, DomainType, Filter, ResourceType, Tag, TagSpecification};
use aws_sdk_ec2::Client as Ec2Client;
use tracing::{debug, error, info, instrument};

pub(crate) const LEGACY_CLUSTER_NAME_TAG: &str = "eip.aws.materialize.com/cluster_name";

pub(crate) const POD_NAME_TAG: &str = "eip.materialize.cloud/pod_name";
pub(crate) const NODE_SELECTOR_TAG: &str = "eip.materialize.cloud/node_selector";
pub(crate) const EIP_UID_TAG: &str = "eip.materialize.cloud/eip_uid";
pub(crate) const EIP_NAME_TAG: &str = "eip.materialize.cloud/eip_name";
pub(crate) const CLUSTER_NAME_TAG: &str = "eip.materialize.cloud/cluster_name";
pub(crate) const NAMESPACE_TAG: &str = "eip.materialize.cloud/namespace";
pub(crate) const NAME_TAG: &str = "Name";

/// Allocates an AWS Elastic IP, and tags it with the pod uid it will later be associated with.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn allocate_address(
    ec2_client: &Ec2Client,
    eip_uid: &str,
    eip_name: &str,
    eip_selector: &crate::eip::v2::EipSelector,
    cluster_name: &str,
    namespace: &str,
    default_tags: &HashMap<String, String>,
) -> Result<AllocateAddressOutput, SdkError<AllocateAddressError>> {
    let mut tags: Vec<Tag> = default_tags
        .iter()
        .map(|(k, v)| Tag::builder().key(k).value(v).build())
        .collect();
    tags.push(Tag::builder().key(EIP_UID_TAG).value(eip_uid).build());
    tags.push(Tag::builder().key(EIP_NAME_TAG).value(eip_name).build());
    tags.push(Tag::builder().key(NAMESPACE_TAG).value(namespace).build());
    tags.push(
        Tag::builder()
            .key(CLUSTER_NAME_TAG)
            .value(cluster_name)
            .build(),
    );
    match eip_selector {
        crate::eip::v2::EipSelector::Pod { pod_name } => {
            tags.push(Tag::builder().key(POD_NAME_TAG).value(pod_name).build());
        }
        crate::eip::v2::EipSelector::Node { selector } => {
            tags.push(
                Tag::builder()
                    .key(NODE_SELECTOR_TAG)
                    .value(serde_json::to_string(selector).unwrap())
                    .build(),
            );
        }
    }

    tags.push(
        Tag::builder()
            .key(NAME_TAG)
            .value(format!(
                "eip-operator:{}:{}:{}",
                cluster_name, namespace, eip_name
            ))
            .build(),
    );
    ec2_client
        .allocate_address()
        .domain(DomainType::Vpc)
        .tag_specifications(
            TagSpecification::builder()
                .resource_type(ResourceType::ElasticIp)
                .set_tags(Some(tags))
                .build(),
        )
        .send()
        .await
}

/// Releases (deletes) an AWS Elastic IP.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn release_address(
    ec2_client: &Ec2Client,
    allocation_id: &str,
) -> Result<ReleaseAddressOutput, SdkError<ReleaseAddressError>> {
    ec2_client
        .release_address()
        .allocation_id(allocation_id)
        .send()
        .await
}

/// Associates an AWS Elastic IP with the Elastic Network Interface.
/// The private IP of the association will be the pod IP supplied.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn associate_eip(
    ec2_client: &Ec2Client,
    eip_id: &str,
    eni_id: &str,
    private_ip: &str,
) -> Result<AssociateAddressOutput, SdkError<AssociateAddressError>> {
    ec2_client
        .associate_address()
        .allocation_id(eip_id)
        .allow_reassociation(true)
        .network_interface_id(eni_id)
        .private_ip_address(private_ip)
        .send()
        .await
}

/// Describes a single EIP with the specified allocation ID.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn describe_address(
    ec2_client: &Ec2Client,
    allocation_id: &str,
) -> Result<DescribeAddressesOutput, SdkError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses()
        .allocation_ids(allocation_id)
        .send()
        .await
}

/// Describes any EIPs tagged with the specified pod uid.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn describe_addresses_with_tag_value(
    ec2_client: &Ec2Client,
    key: &str,
    value: &str,
) -> Result<DescribeAddressesOutput, SdkError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses()
        .filters(
            Filter::builder()
                .name(format!("tag:{}", key))
                .values(value)
                .build(),
        )
        .send()
        .await
}

/// Disassociates an Elastic IP from an Elastic Network Interface.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn disassociate_eip(
    ec2_client: &Ec2Client,
    association_id: &str,
) -> Result<(), DisassociateAddressError> {
    match ec2_client
        .disassociate_address()
        .association_id(association_id)
        .send()
        .await
    {
        Ok(_) => Ok(()),
        Err(e) => {
            let e = e.into_service_error();
            if e.meta().code() == Some("InvalidAssociationID.NotFound") {
                info!("Association id {} already disassociated", association_id);
                Ok(())
            } else {
                error!("Error disassociating {} - {:?}", association_id, e);
                Err(e)
            }
        }
    }
}

/// Disassociates EIP if it is attached to a NIC, then deletes the EIP.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn disassociate_and_release_address(
    ec2_client: &Ec2Client,
    address: &Address,
) -> Result<(), crate::Error> {
    if let Some(association_id) = &address.association_id {
        disassociate_eip(ec2_client, association_id).await?;
    }
    if let Some(allocation_id) = &address.allocation_id {
        // Is it actually possible the allocation_id won't exist?
        release_address(ec2_client, allocation_id).await?;
    }
    Ok(())
}

/// Searches tags on the supplied address and returns the value if it exists.
pub(crate) fn get_tag_from_address<'a>(address: &'a Address, key: &str) -> Option<&'a str> {
    address
        .tags
        .as_ref()?
        .iter()
        .find(|&tag| tag.key.as_deref() == Some(key))?
        .value
        .as_deref()
}

/// Describes an AWS EC2 instance with the supplied instance_id.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn describe_instance(
    ec2_client: &Ec2Client,
    instance_id: &str,
) -> Result<DescribeInstancesOutput, SdkError<DescribeInstancesError>> {
    ec2_client
        .describe_instances()
        .instance_ids(instance_id)
        .send()
        .await
}

pub(crate) fn get_eni_from_private_ip(
    instance: &DescribeInstancesOutput,
    private_ip_address: &str,
) -> Option<String> {
    instance
        .reservations
        .as_ref()
        .and_then(|reservations| reservations[0].instances.as_ref())
        .and_then(|instances| instances[0].network_interfaces.as_ref())
        .and_then(|network_interfaces| {
            network_interfaces.iter().find_map(|nic| {
                nic.private_ip_addresses.as_ref()?.iter().find_map(|ip| {
                    match ip.private_ip_address.as_ref()? {
                        x if x == private_ip_address => {
                            debug!(
                                "Found matching NIC: {} {} {}",
                                nic.network_interface_id.as_ref()?,
                                private_ip_address,
                                ip.private_ip_address.as_ref()?,
                            );
                            Some(nic.network_interface_id.as_ref()?.to_owned())
                        }
                        _ => None,
                    }
                })
            })
        })
}
