use rusoto_core::RusotoError;
use rusoto_ec2::{
    AllocateAddressError, AllocateAddressRequest, AllocateAddressResult, AssociateAddressError,
    AssociateAddressRequest, AssociateAddressResult, DescribeAddressesError,
    DescribeAddressesRequest, DescribeAddressesResult, DisassociateAddressError,
    DisassociateAddressRequest, Ec2, Ec2Client, Filter, ReleaseAddressError, ReleaseAddressRequest,
    Tag, TagSpecification,
};

const EIP_POD_UID_TAG: &str = "eip.aws.materialize.com/pod_uid";

pub(crate) async fn allocate_address(
    ec2_client: &Ec2Client,
    pod_uid: String,
) -> Result<AllocateAddressResult, RusotoError<AllocateAddressError>> {
    ec2_client
        .allocate_address(AllocateAddressRequest {
            address: None,
            customer_owned_ipv_4_pool: None,
            domain: Some("vpc".to_owned()),
            dry_run: None,
            network_border_group: None,
            public_ipv_4_pool: None,
            tag_specifications: Some(vec![TagSpecification {
                resource_type: Some("elastic-ip".to_owned()),
                tags: Some(vec![Tag {
                    key: Some(EIP_POD_UID_TAG.to_owned()),
                    value: Some(pod_uid),
                }]),
            }]),
        })
        .await
}

pub(crate) async fn release_address(
    ec2_client: &Ec2Client,
    allocation_id: String,
) -> Result<(), RusotoError<ReleaseAddressError>> {
    ec2_client
        .release_address(ReleaseAddressRequest {
            allocation_id: Some(allocation_id),
            dry_run: None,
            network_border_group: None,
            public_ip: None,
        })
        .await
}

pub(crate) async fn associate_eip_with_pod_eni(
    ec2_client: &Ec2Client,
    eip_id: String,
    eni_id: String,
    pod_ip: String,
) -> Result<AssociateAddressResult, RusotoError<AssociateAddressError>> {
    ec2_client
        .associate_address(AssociateAddressRequest {
            allocation_id: Some(eip_id),
            allow_reassociation: Some(true),
            dry_run: None,
            instance_id: None,
            network_interface_id: Some(eni_id),
            private_ip_address: Some(pod_ip),
            public_ip: None,
        })
        .await
}

/// Describes a single EIP with the specified allocation ID.
pub(crate) async fn describe_address(
    ec2_client: &Ec2Client,
    allocation_id: String,
) -> Result<DescribeAddressesResult, RusotoError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses(DescribeAddressesRequest {
            allocation_ids: Some(vec![allocation_id]),
            dry_run: None,
            filters: None,
            public_ips: None,
        })
        .await
}

/// Describes any EIPs tagged with the specified pod uid.
pub(crate) async fn describe_addresses_with_pod_uid(
    ec2_client: &Ec2Client,
    pod_uid: String,
) -> Result<DescribeAddressesResult, RusotoError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses(DescribeAddressesRequest {
            allocation_ids: None,
            dry_run: None,
            filters: Some(vec![Filter {
                name: Some(format!("tag:{}", EIP_POD_UID_TAG)),
                values: Some(vec![pod_uid]),
            }]),
            public_ips: None,
        })
        .await
}

pub(crate) async fn disassociate_eip(
    ec2_client: &Ec2Client,
    association_id: String,
) -> Result<(), RusotoError<DisassociateAddressError>> {
    ec2_client
        .disassociate_address(DisassociateAddressRequest {
            association_id: Some(association_id),
            dry_run: None,
            public_ip: None,
        })
        .await
}
