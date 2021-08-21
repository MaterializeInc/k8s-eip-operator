use aws_sdk_ec2::error::{
    AllocateAddressError, AssociateAddressError, DescribeAddressesError, DisassociateAddressError,
    ReleaseAddressError,
};
use aws_sdk_ec2::model::{DomainType, Filter, ResourceType, Tag, TagSpecification};
use aws_sdk_ec2::output::{
    AllocateAddressOutput, AssociateAddressOutput, DescribeAddressesOutput,
    DisassociateAddressOutput, ReleaseAddressOutput,
};
use aws_sdk_ec2::{Client as Ec2Client, SdkError};

const EIP_POD_UID_TAG: &str = "eip.aws.materialize.com/pod_uid";

/// Allocates an AWS Elastic IP, and tags it with the pod uid it will later be associated with.
pub(crate) async fn allocate_address(
    ec2_client: &Ec2Client,
    pod_uid: String,
) -> Result<AllocateAddressOutput, SdkError<AllocateAddressError>> {
    ec2_client
        .allocate_address()
        .domain(DomainType::Vpc)
        .tag_specifications(
            TagSpecification::builder()
                .resource_type(ResourceType::ElasticIp)
                .tags(Tag::builder().key(EIP_POD_UID_TAG).value(pod_uid).build())
                .build(),
        )
        .send()
        .await
}

/// Releases (deletes) an AWS Elastic IP.
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
pub(crate) async fn associate_eip_with_pod_eni(
    ec2_client: &Ec2Client,
    eip_id: String,
    eni_id: String,
    pod_ip: String,
) -> Result<AssociateAddressOutput, SdkError<AssociateAddressError>> {
    ec2_client
        .associate_address()
        .allocation_id(eip_id)
        .allow_reassociation(true)
        .network_interface_id(eni_id)
        .private_ip_address(pod_ip)
        .send()
        .await
}

/// Describes a single EIP with the specified allocation ID.
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
pub(crate) async fn describe_addresses_with_pod_uid(
    ec2_client: &Ec2Client,
    pod_uid: String,
) -> Result<DescribeAddressesOutput, SdkError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses()
        .filters(
            Filter::builder()
                .name(format!("tag:{}", EIP_POD_UID_TAG))
                .values(pod_uid.to_owned())
                .build(),
        )
        .send()
        .await
}

/// Disassociates an Elastic IP from an Elastic Network Interface.
pub(crate) async fn disassociate_eip(
    ec2_client: &Ec2Client,
    association_id: String,
) -> Result<DisassociateAddressOutput, SdkError<DisassociateAddressError>> {
    ec2_client
        .disassociate_address()
        .association_id(association_id)
        .send()
        .await
}
