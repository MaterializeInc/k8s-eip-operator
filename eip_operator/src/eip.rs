use std::collections::HashMap;

use aws_sdk_ec2::error::{
    AllocateAddressError, AssociateAddressError, DescribeAddressesError, DisassociateAddressError,
    ReleaseAddressError,
};
use aws_sdk_ec2::model::{Address, DomainType, Filter, ResourceType, Tag, TagSpecification};
use aws_sdk_ec2::output::{
    AllocateAddressOutput, AssociateAddressOutput, DescribeAddressesOutput, ReleaseAddressOutput,
};
use aws_sdk_ec2::types::SdkError;
use aws_sdk_ec2::Client as Ec2Client;
use tracing::{info, instrument};

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
    eip_selector: &crate::v2::EipSelector,
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
        crate::v2::EipSelector::Pod { pod_name } => {
            tags.push(Tag::builder().key(POD_NAME_TAG).value(pod_name).build());
        }
        crate::v2::EipSelector::Node { selector } => {
            tags.push(
                Tag::builder()
                    .key(NODE_SELECTOR_TAG)
                    .value(&serde_json::to_string(selector).unwrap())
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
    allocation_id: String,
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
    eip_id: String,
    eni_id: String,
    private_ip: String,
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
    allocation_id: String,
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
    value: String,
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
    association_id: String,
) -> Result<(), SdkError<DisassociateAddressError>> {
    match ec2_client
        .disassociate_address()
        .association_id(&association_id)
        .send()
        .await
    {
        Ok(_) => Ok(()),
        Err(e) if e.to_string().contains("InvalidAssociationID.NotFound") => {
            info!(already_disassociated = true);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

/// Disassociates EIP if it is attached to a NIC, then deletes the EIP.
#[instrument(skip(ec2_client), err)]
pub(crate) async fn disassociate_and_release_address(
    ec2_client: &Ec2Client,
    address: &Address,
) -> Result<(), crate::Error> {
    if let Some(association_id) = &address.association_id {
        disassociate_eip(ec2_client, association_id.to_owned()).await?;
    }
    if let Some(allocation_id) = &address.allocation_id {
        // Is it actually possible the allocation_id won't exist?
        release_address(ec2_client, allocation_id.to_owned()).await?;
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
