use rusoto_core::RusotoError;
use rusoto_ec2::{
    AllocateAddressError, AllocateAddressRequest, AllocateAddressResult, AssociateAddressError,
    AssociateAddressRequest, AssociateAddressResult, DescribeAddressesError,
    DescribeAddressesRequest, DescribeAddressesResult, DisassociateAddressError,
    DisassociateAddressRequest, Ec2, Ec2Client, Filter, ReleaseAddressError, ReleaseAddressRequest,
    Tag, TagSpecification,
};

const EIP_POD_UID_TAG: &'static str = "eip.aws.materialize.com/pod_uid";

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

pub(crate) async fn describe_addresses(
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

// possible (sane) states:
//
// unknown        1. pod exists, nothing else yet
// allocating     2. pod exists, finalizer in place
// associating    3. pod exists, finalizer in place, EIP created,
// annotating     4. pod exists, finalizer in place, EIP created, EIP associated
// ready          5. pod exists, finalizer in place, EIP created, EIP associated, DNS annotation applied
//
// deannotating   6. pod exists, finalizer in place, EIP created, EIP associated, DNS annotation exists
// disassociating 6. pod exists, finalizer in place, EIP created, EIP associated, DNS annotation removed
// deallocating   7. pod exists, finalizer in place, EIP exists, EIP disassociated
// deallocated    8. pod exists, finalizer in place, EIP released
//                9. pod exists, finalizer removed, EIP released
