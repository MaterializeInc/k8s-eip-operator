use std::fmt::Debug;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams};
use kube::{Client, Resource};
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use kube_runtime::finalizer::{finalizer, Event};
use rusoto_core::region::Region;
use rusoto_core::RusotoError;
use rusoto_ec2::{
    AssociateAddressError, AssociateAddressRequest, AssociateAddressResult, DescribeAddressesError,
    DescribeAddressesRequest, DescribeAddressesResult, DescribeInstancesError,
    DescribeInstancesRequest, DescribeInstancesResult, DisassociateAddressError,
    DisassociateAddressRequest, Ec2, Ec2Client,
};

const EIP_LABEL: &'static str = "eip.aws.materialize.com/id";
const FINALIZER_NAME: &'static str = "eip.aws.materialize.com/disassociate";

struct ContextData {
    namespace: String,
    k8s_client: Client,
    ec2_client: Ec2Client,
}

impl ContextData {
    fn new(namespace: String, k8s_client: Client, ec2_client: Ec2Client) -> ContextData {
        ContextData {
            namespace,
            k8s_client,
            ec2_client,
        }
    }
}

async fn describe_instance(
    ec2_client: &Ec2Client,
    instance_id: String,
) -> Result<DescribeInstancesResult, RusotoError<DescribeInstancesError>> {
    ec2_client
        .describe_instances(DescribeInstancesRequest {
            dry_run: None,
            filters: None,
            instance_ids: Some(vec![instance_id]),
            max_results: None,
            next_token: None,
        })
        .await
}

async fn associate_eip_with_pod_eni(
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

async fn describe_addresses(
    ec2_client: &Ec2Client,
    allocation_ids: Option<Vec<String>>,
) -> Result<DescribeAddressesResult, RusotoError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses(DescribeAddressesRequest {
            allocation_ids,
            dry_run: None,
            filters: None,
            public_ips: None,
        })
        .await
}

async fn disassociate_eip(
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

async fn apply(
    ec2_client: &Ec2Client,
    node_api: Api<Node>,
    eip_id: String,
    pod: Pod,
) -> Result<ReconcilerAction, Error> {
    println!("Associating...");
    let pod_ip = pod
        .status
        .as_ref()
        .ok_or_else(|| Error::MissingPodIp)?
        .pod_ip
        .as_ref()
        .ok_or_else(|| Error::MissingPodIp)?;

    let node_name = pod
        .spec
        .as_ref()
        .ok_or_else(|| Error::MissingNodeName)?
        .node_name
        .as_ref()
        .ok_or_else(|| Error::MissingNodeName)?;

    let node = node_api.get(node_name).await?;

    let provider_id = node
        .spec
        .as_ref()
        .ok_or_else(|| Error::MissingProviderId)?
        .provider_id
        .as_ref()
        .ok_or_else(|| Error::MissingProviderId)?;

    let instance_id = provider_id
        .rsplit_once('/')
        .ok_or_else(|| Error::MalformedProviderId)?
        .1;

    let instance_description = describe_instance(&ec2_client, instance_id.to_owned()).await?;

    let eni_id = instance_description
        .reservations
        .as_ref()
        .ok_or_else(|| Error::MissingReservations)?[0]
        .instances
        .as_ref()
        .ok_or_else(|| Error::MissingInstances)?[0]
        .network_interfaces
        .as_ref()
        .ok_or_else(|| Error::MissingNetworkInterfaces)?
        .iter()
        .find_map(|nic| {
            nic.private_ip_addresses.as_ref()?.iter().find_map(|ip| {
                match ip.private_ip_address.as_ref()? {
                    x if x == pod_ip => Some(nic.network_interface_id.as_ref()?.to_owned()),
                    _ => None,
                }
            })
        })
        .ok_or_else(|| Error::NoInterfaceWithThatIp)?;

    associate_eip_with_pod_eni(&ec2_client, eip_id.to_owned(), eni_id, pod_ip.to_owned()).await?;
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(300)),
    })
}

async fn cleanup(ec2_client: &Ec2Client, eip_id: String) -> Result<ReconcilerAction, Error> {
    if let Some(association_id) = &describe_addresses(&ec2_client, Some(vec![eip_id.to_owned()]))
        .await?
        .addresses
        .ok_or_else(|| Error::MissingAddresses)?[0]
        .association_id
    {
        disassociate_eip(&ec2_client, association_id.to_owned()).await?;
    }
    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

// https://dzone.com/articles/oxidizing-the-kubernetes-operator
async fn reconcile(
    pod: Pod,
    context: Context<ContextData>,
) -> Result<ReconcilerAction, kube_runtime::finalizer::Error<Error>> {
    let namespace = &context.get_ref().namespace;
    let k8s_client = context.get_ref().k8s_client.clone();
    let pod_api: Api<Pod> = Api::namespaced(k8s_client.clone(), namespace);
    let node_api: Api<Node> = Api::all(k8s_client.clone());
    let eip_id = pod.meta().labels[EIP_LABEL].to_owned();
    finalizer(&pod_api, FINALIZER_NAME, pod, |event| async {
        let ec2_client = context.get_ref().ec2_client.clone();
        match event {
            Event::Apply(pod) => apply(&ec2_client, node_api, eip_id, pod).await,
            Event::Cleanup(_pod) => cleanup(&ec2_client, eip_id).await,
        }
    })
    .await
}

fn on_error(
    _error: &kube_runtime::finalizer::Error<Error>,
    _context: Context<ContextData>,
) -> ReconcilerAction {
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(5)),
    }
}

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Kubernetes error: {source}")]
    KubeError {
        #[from]
        source: kube::Error,
    },
    #[error("Pod does not have an IP address.")]
    MissingPodIp,
    #[error("Pod does not have a node name in its spec.")]
    MissingNodeName,
    #[error("Node does not have a provider_id in its spec.")]
    MissingProviderId,
    #[error("Node provider_id is not in expected format.")]
    MalformedProviderId,
    #[error("DescribeInstancesResult.reservations was None.")]
    MissingReservations,
    #[error("DescribeInstancesResult.reservations[0].instances was None.")]
    MissingInstances,
    #[error("DescribeInstancesResult.reservations[0].instances[0].network_insterfaces was None.")]
    MissingNetworkInterfaces,
    #[error("No interface found with IP matching pod.")]
    MissingAddresses,
    #[error("DescribeAddressesResult.addresses was None.")]
    NoInterfaceWithThatIp,
    #[error("Rusoto describe_instances reported error: {source}")]
    DescribeInstancesError {
        #[from]
        source: rusoto_core::RusotoError<DescribeInstancesError>,
    },
    #[error("Rusoto describe_addresses reported error: {source}")]
    DescribeAddressesError {
        #[from]
        source: rusoto_core::RusotoError<DescribeAddressesError>,
    },
    #[error("Rusoto associate_address reported error: {source}")]
    AssociateAddressError {
        #[from]
        source: rusoto_core::RusotoError<AssociateAddressError>,
    },
    #[error("Rusoto disassociate_address reported error: {source}")]
    DisassociateAddressError {
        #[from]
        source: rusoto_core::RusotoError<DisassociateAddressError>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let k8s_client = Client::try_default().await?;
    let ec2_client = Ec2Client::new(Region::UsEast1);
    let namespace = std::env::var("NAMESPACE").unwrap_or_else(|_| "default".into());

    let api = Api::<Pod>::namespaced(k8s_client.clone(), &namespace);
    let context: Context<ContextData> =
        Context::new(ContextData::new(namespace, k8s_client.clone(), ec2_client));
    Controller::new(api, ListParams::default().labels(EIP_LABEL))
        .run(reconcile, on_error, context)
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(resource) => println!("Reconciliation successful. Resource: {:?}", resource),
                Err(err) => eprintln!("Reconciliation error: {:?}", err),
            }
        })
        .await;
    Ok(())
}

// This tool is responsible for:
// 1. Assigning the EIP to the ENI
//
// This means it needs to get the ENI ID, which it can do by querying AWS for ENI with the IP of the pod.
// It also needs to query the association of the EIP.
// https://rusoto.github.io/rusoto/rusoto_ec2/struct.DescribeAddressesRequest.html
// https://rusoto.github.io/rusoto/rusoto_ec2/struct.DescribeInstancesRequest.html
// aws ec2 describe-instances --profile mz-cloud-staging-admin --filters Name=network-interface.addresses.private-ip-address,Values=10.1.121.254

// https://github.com/LogMeIn/k8s-aws-operator
// https://github.com/kubernetes-sigs/external-dns/pull/2115/files
//
// labels:
// eip.aws.materialize.com/id=eip-123456789 should be set by cloud app
//
// annotations:
// external-dns.alpha.kubernetes.io/hostname: jj3q1eoaaci.alexhunt.staging.materialize.cloud
// external-dns.alpha.kubernetes.io/target: "1.2.3.4" should be set by cloud app
//
//
//
// TODO
// Create EIP with appropriate tags.
// Assign label to pod with EIP assignment.
// Create finalizer to destroy the EIP.
