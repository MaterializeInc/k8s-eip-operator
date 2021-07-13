use std::fmt::Debug;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use kube_runtime::finalizer::{finalizer, Event};
use rusoto_core::region::Region;
use rusoto_core::RusotoError;
use rusoto_ec2::{
    AllocateAddressError, AssociateAddressError, DescribeAddressesError, DescribeInstancesError,
    DescribeInstancesRequest, DescribeInstancesResult, DisassociateAddressError, Ec2, Ec2Client,
    ReleaseAddressError,
};

mod eip;

const FIELD_MANAGER: &'static str = "eip.aws.materialize.com";
const MANAGE_EIP_LABEL: &'static str = "eip.aws.materialize.com/manage";
const FINALIZER_NAME: &'static str = "eip.aws.materialize.com/disassociate";
const EIP_ALLOCATION_ID_ANNOTATION: &'static str = "eip.aws.materialize.com/allocation_id";
const EXTERNAL_DNS_TARGET_ANNOTATION: &'static str = "external-dns.alpha.kubernetes.io/target";

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

async fn add_dns_target_annotation(
    pod_api: &Api<Pod>,
    pod_name: String,
    eip_address: String,
    allocation_id: String,
) -> Result<Pod, kube::Error> {
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "annotations": {
                EIP_ALLOCATION_ID_ANNOTATION: allocation_id,
                EXTERNAL_DNS_TARGET_ANNOTATION: eip_address
            }
        }
    });
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(FIELD_MANAGER);
    pod_api.patch(&pod_name, &params, &patch).await
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

async fn apply(
    ec2_client: &Ec2Client,
    node_api: &Api<Node>,
    pod_api: &Api<Pod>,
    pod: Pod,
) -> Result<ReconcilerAction, Error> {
    println!("Associating...");
    let pod_uid = pod
        .metadata
        .uid
        .as_ref()
        .ok_or_else(|| Error::MissingPodUid)?;
    let addresses = &eip::describe_addresses(&ec2_client, pod_uid.to_owned())
        .await?
        .addresses
        .ok_or_else(|| Error::MissingAddresses)?;
    let (allocation_id, public_ip) = match addresses.len() {
        0 => {
            let response = eip::allocate_address(ec2_client, pod_uid.to_owned()).await?;
            let allocation_id = response
                .allocation_id
                .ok_or_else(|| Error::MissingAllocationId)?;
            let public_ip = response.public_ip.ok_or_else(|| Error::MissingPublicIp)?;
            (allocation_id, public_ip)
        }
        1 => {
            let allocation_id = addresses[0]
                .allocation_id
                .as_ref()
                .ok_or_else(|| Error::MissingAllocationId)?;
            let public_ip = addresses[0]
                .public_ip
                .as_ref()
                .ok_or_else(|| Error::MissingPublicIp)?;
            (allocation_id.to_owned(), public_ip.to_owned())
        }
        _ => {
            return Err(Error::MultipleEipsTaggedForPod);
        }
    };

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
                    x if x == pod_ip => {
                        println!(
                            "Found matching NIC: {} {} {}",
                            nic.network_interface_id.as_ref().unwrap(),
                            pod_ip,
                            ip.private_ip_address.as_ref().unwrap()
                        );
                        Some(nic.network_interface_id.as_ref()?.to_owned())
                    }
                    _ => None,
                }
            })
        })
        .ok_or_else(|| Error::NoInterfaceWithThatIp)?;

    // TODO add if statement
    // TODO If it is not associated, or associated with something else, associate it with the pod
    // If it is associated with the pod, continue
    eip::associate_eip_with_pod_eni(
        &ec2_client,
        allocation_id.to_owned(),
        eni_id,
        pod_ip.to_owned(),
    )
    .await?;
    let pod_name = pod
        .metadata
        .name
        .as_ref()
        .ok_or_else(|| Error::MissingPodName)?;
    add_dns_target_annotation(
        pod_api,
        pod_name.to_owned(),
        public_ip.to_owned(),
        allocation_id,
    )
    .await?;
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(300)),
    })
}

async fn cleanup(ec2_client: &Ec2Client, pod: Pod) -> Result<ReconcilerAction, Error> {
    println!("Cleaning up...");
    let pod_uid = pod
        .metadata
        .uid
        .as_ref()
        .ok_or_else(|| Error::MissingPodUid)?;
    let addresses = &eip::describe_addresses(&ec2_client, pod_uid.to_owned())
        .await?
        .addresses
        .ok_or_else(|| Error::MissingAddresses)?;
    for address in addresses {
        if let Some(association_id) = &address.association_id {
            eip::disassociate_eip(&ec2_client, association_id.to_owned()).await?;
        }
        if let Some(allocation_id) = &address.allocation_id {
            // Is it actually possible the allocation_id won't exist?
            eip::release_address(&ec2_client, allocation_id.to_owned()).await?;
        }
    }
    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

async fn reconcile(
    pod: Pod,
    context: Context<ContextData>,
) -> Result<ReconcilerAction, kube_runtime::finalizer::Error<Error>> {
    let namespace = &context.get_ref().namespace;
    let k8s_client = context.get_ref().k8s_client.clone();
    let pod_api: Api<Pod> = Api::namespaced(k8s_client.clone(), namespace);
    let node_api: Api<Node> = Api::all(k8s_client.clone());
    finalizer(&pod_api, FINALIZER_NAME, pod, |event| async {
        let ec2_client = context.get_ref().ec2_client.clone();
        match event {
            Event::Apply(pod) => apply(&ec2_client, &node_api, &pod_api, pod).await,
            Event::Cleanup(pod) => cleanup(&ec2_client, pod).await,
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
    #[error("io error: {source}")]
    IoError {
        #[from]
        source: std::io::Error,
    },
    #[error("Kubernetes error: {source}")]
    KubeError {
        #[from]
        source: kube::Error,
    },
    #[error("Pod does not have a UID in it's metadata.")]
    MissingPodUid,
    #[error("Pod does not have a name in its metadata.")]
    MissingPodName,
    #[error("Pod does not have an IP address.")]
    MissingPodIp,
    #[error("Pod does not have a node name in its spec.")]
    MissingNodeName,
    #[error("Node does not have a provider_id in its spec.")]
    MissingProviderId,
    #[error("Node provider_id is not in expected format.")]
    MalformedProviderId,
    #[error("Multiple elastic IPs are tagged with this pod's UID.")]
    MultipleEipsTaggedForPod,
    #[error("allocation_id was None.")]
    MissingAllocationId,
    #[error("public_ip was None.")]
    MissingPublicIp,
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
    #[error("Rusoto allocate_address reported error: {source}")]
    AllocateAddressError {
        #[from]
        source: rusoto_core::RusotoError<AllocateAddressError>,
    },
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
    #[error("Rusoto release_address reported error: {source}")]
    ReleaseAddressError {
        #[from]
        source: rusoto_core::RusotoError<ReleaseAddressError>,
    },
}

fn main() -> Result<(), Error> {
    println!("main");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .thread_name_fn(|| {
            static ATOMIC_ID: AtomicUsize = AtomicUsize::new(0);
            let id = ATOMIC_ID.fetch_add(1, Ordering::SeqCst);
            format!("tokio:work-{}", id)
        })
        .enable_all()
        .build()?;
    runtime.block_on(run())?;
    Ok(())
}
async fn run() -> Result<(), Error> {
    println!("run");
    println!("getting k8s_client");
    let k8s_client = Client::try_default().await?;
    println!("getting ec2_client");
    let ec2_client = Ec2Client::new(Region::UsEast1);
    println!("getting namespace from env");
    let namespace = std::env::var("NAMESPACE").unwrap_or_else(|_| "default".into());

    println!("getting pod api");
    let api = Api::<Pod>::namespaced(k8s_client.clone(), &namespace);
    println!("watching for events");
    let context: Context<ContextData> =
        Context::new(ContextData::new(namespace, k8s_client.clone(), ec2_client));
    Controller::new(api, ListParams::default().labels(MANAGE_EIP_LABEL))
        .run(reconcile, on_error, context)
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(resource) => println!("Reconciliation successful. Resource: {:?}", resource),
                Err(err) => eprintln!("Reconciliation error: {:?}", err),
            }
        })
        .await;
    println!("exiting");
    Ok(())
}
