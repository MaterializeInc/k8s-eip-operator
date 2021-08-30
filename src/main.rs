use std::collections::{HashMap, HashSet};
use std::fmt::Debug;
use std::time::Duration;

use aws_sdk_ec2::error::{
    AllocateAddressError, AssociateAddressError, DescribeAddressesError, DescribeInstancesError,
    DisassociateAddressError, ReleaseAddressError,
};
use aws_sdk_ec2::output::DescribeInstancesOutput;
use aws_sdk_ec2::{Client as Ec2Client, Config, SdkError};
use env_logger::Env;
use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use kube_runtime::finalizer::{finalizer, Event};
use log::{debug, error, info};

mod eip;

const FIELD_MANAGER: &str = "eip.aws.materialize.com";
const MANAGE_EIP_LABEL: &str = "eip.aws.materialize.com/manage";
const FINALIZER_NAME: &str = "eip.aws.materialize.com/disassociate";
const EIP_ALLOCATION_ID_ANNOTATION: &str = "eip.aws.materialize.com/allocation_id";
const EXTERNAL_DNS_TARGET_ANNOTATION: &str = "external-dns.alpha.kubernetes.io/target";

struct ContextData {
    namespace: String,
    cluster_name: String,
    default_tags: HashMap<String, String>,
    k8s_client: Client,
    ec2_client: Ec2Client,
}

impl ContextData {
    fn new(
        namespace: String,
        cluster_name: String,
        default_tags: HashMap<String, String>,
        k8s_client: Client,
        ec2_client: Ec2Client,
    ) -> ContextData {
        ContextData {
            namespace,
            cluster_name,
            default_tags,
            k8s_client,
            ec2_client,
        }
    }
}

/// Applies annotation to pod specifying the target IP for external-dns.
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

/// Describes an AWS EC2 instance with the supplied instance_id.
async fn describe_instance(
    ec2_client: &Ec2Client,
    instance_id: String,
) -> Result<DescribeInstancesOutput, SdkError<DescribeInstancesError>> {
    ec2_client
        .describe_instances()
        .instance_ids(instance_id)
        .send()
        .await
}

/// Creates or updates EIP associations when creating or updating a pod.
async fn apply(
    ec2_client: &Ec2Client,
    node_api: &Api<Node>,
    pod_api: &Api<Pod>,
    pod: Pod,
    cluster_name: &str,
    default_tags: &HashMap<String, String>,
) -> Result<ReconcilerAction, Error> {
    info!("Associating...");
    let pod_uid = pod.metadata.uid.as_ref().ok_or(Error::MissingPodUid)?;
    let addresses =
        eip::describe_addresses_with_tag_value(&ec2_client, eip::POD_UID_TAG, pod_uid.to_owned())
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?;
    let (allocation_id, public_ip) = match addresses.len() {
        0 => {
            let response = eip::allocate_address(
                ec2_client,
                pod_uid.to_owned(),
                cluster_name.to_owned(),
                default_tags,
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

    let pod_ip = pod
        .status
        .as_ref()
        .ok_or(Error::MissingPodIp)?
        .pod_ip
        .as_ref()
        .ok_or(Error::MissingPodIp)?;

    let node_name = pod
        .spec
        .as_ref()
        .ok_or(Error::MissingNodeName)?
        .node_name
        .as_ref()
        .ok_or(Error::MissingNodeName)?;

    let node = node_api.get(node_name).await?;

    let provider_id = node
        .spec
        .as_ref()
        .ok_or(Error::MissingProviderId)?
        .provider_id
        .as_ref()
        .ok_or(Error::MissingProviderId)?;

    let instance_id = provider_id
        .rsplit_once('/')
        .ok_or(Error::MalformedProviderId)?
        .1;

    let instance_description = describe_instance(&ec2_client, instance_id.to_owned()).await?;

    let eni_id = instance_description
        .reservations
        .as_ref()
        .ok_or(Error::MissingReservations)?[0]
        .instances
        .as_ref()
        .ok_or(Error::MissingInstances)?[0]
        .network_interfaces
        .as_ref()
        .ok_or(Error::MissingNetworkInterfaces)?
        .iter()
        .find_map(|nic| {
            nic.private_ip_addresses.as_ref()?.iter().find_map(|ip| {
                match ip.private_ip_address.as_ref()? {
                    x if x == pod_ip => {
                        debug!(
                            "Found matching NIC: {} {} {}",
                            nic.network_interface_id.as_ref()?,
                            pod_ip,
                            ip.private_ip_address.as_ref()?,
                        );
                        Some(nic.network_interface_id.as_ref()?.to_owned())
                    }
                    _ => None,
                }
            })
        })
        .ok_or(Error::NoInterfaceWithThatIp)?;
    let eip_description = eip::describe_address(&ec2_client, allocation_id.to_owned())
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?
        .swap_remove(0);
    if eip_description.network_interface_id != Some(eni_id.to_owned())
        || eip_description.private_ip_address != Some(pod_ip.to_owned())
    {
        eip::associate_eip_with_pod_eni(
            &ec2_client,
            allocation_id.to_owned(),
            eni_id,
            pod_ip.to_owned(),
        )
        .await?;
    }
    let pod_name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
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

/// Deletes AWS Elastic IP associated with a pod being destroyed.
async fn cleanup_pod_eip(ec2_client: &Ec2Client, pod: Pod) -> Result<ReconcilerAction, Error> {
    info!("Cleaning up...");
    let pod_uid = pod.metadata.uid.as_ref().ok_or(Error::MissingPodUid)?;
    let addresses =
        &eip::describe_addresses_with_tag_value(&ec2_client, eip::POD_UID_TAG, pod_uid.to_owned())
            .await?
            .addresses
            .ok_or(Error::MissingAddresses)?;
    for address in addresses {
        eip::disassociate_and_release_address(ec2_client, address).await?;
    }
    Ok(ReconcilerAction {
        requeue_after: None,
    })
}

/// Finds all EIPs tagged for this cluster, then compares them to the pod UIDs. If the EIP is not
/// tagged with a pod UID, or the UID does not exist in this cluster, it deletes the EIP.
async fn cleanup_orphan_eips(
    ec2_client: &Ec2Client,
    pod_api: &Api<Pod>,
    cluster_name: &str,
) -> Result<(), Error> {
    let addresses = eip::describe_addresses_with_tag_value(
        &ec2_client,
        eip::CLUSTER_NAME_TAG,
        cluster_name.to_owned(),
    )
    .await?
    .addresses
    .ok_or(Error::MissingAddresses)?;

    let lp = ListParams::default().labels(MANAGE_EIP_LABEL);
    let pod_uids: HashSet<String> = pod_api
        .list(&lp)
        .await?
        .into_iter()
        .filter_map(|pod| pod.metadata.uid)
        .collect();

    for address in addresses {
        let pod_uid = eip::get_tag_from_address(&address, eip::POD_UID_TAG);
        if pod_uid.is_none() || !pod_uids.contains(pod_uid.unwrap()) {
            info!(
                "Cleaning up orphaned EIP {:?} with pod uid {:?}",
                address.allocation_id, pod_uid
            );
            eip::disassociate_and_release_address(ec2_client, &address).await?;
        }
    }
    Ok(())
}

/// Takes actions to create/associate an EIP with the pod or clean up if the pod is being deleted.
/// Wraps these operations with a finalizer to ensure the pod is not deleted without cleaning up
/// the Elastic IP associated with it.
async fn reconcile(
    pod: Pod,
    context: Context<ContextData>,
) -> Result<ReconcilerAction, kube_runtime::finalizer::Error<Error>> {
    let namespace = &context.get_ref().namespace;
    let cluster_name = &context.get_ref().cluster_name;
    let default_tags = &context.get_ref().default_tags;
    let k8s_client = context.get_ref().k8s_client.clone();
    let pod_api: Api<Pod> = Api::namespaced(k8s_client.clone(), namespace);
    let node_api: Api<Node> = Api::all(k8s_client.clone());
    finalizer(&pod_api, FINALIZER_NAME, pod, |event| async {
        let ec2_client = context.get_ref().ec2_client.clone();
        match event {
            Event::Apply(pod) => {
                apply(
                    &ec2_client,
                    &node_api,
                    &pod_api,
                    pod,
                    cluster_name,
                    default_tags,
                )
                .await
            }
            Event::Cleanup(pod) => cleanup_pod_eip(&ec2_client, pod).await,
        }
    })
    .await
}

/// Requeues the operation if there is an error.
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
    Io {
        #[from]
        source: std::io::Error,
    },
    #[error("Kubernetes error: {source}")]
    Kube {
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
    AllocateAddress {
        #[from]
        source: SdkError<AllocateAddressError>,
    },
    #[error("Rusoto describe_instances reported error: {source}")]
    RusotoDescribeInstances {
        #[from]
        source: SdkError<DescribeInstancesError>,
    },
    #[error("Rusoto describe_addresses reported error: {source}")]
    RusotoDescribeAddresses {
        #[from]
        source: SdkError<DescribeAddressesError>,
    },
    #[error("Rusoto associate_address reported error: {source}")]
    RusotoAssociateAddress {
        #[from]
        source: SdkError<AssociateAddressError>,
    },
    #[error("Rusoto disassociate_address reported error: {source}")]
    RusotoDisassociateAddress {
        #[from]
        source: SdkError<DisassociateAddressError>,
    },
    #[error("Rusoto release_address reported error: {source}")]
    RusotoReleaseAddress {
        #[from]
        source: SdkError<ReleaseAddressError>,
    },
    #[error("serde_json error: {source}")]
    SerdeJson {
        #[from]
        source: serde_json::Error,
    },
}

fn main() -> Result<(), Error> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run())?;
    Ok(())
}

async fn run() -> Result<(), Error> {
    debug!("Getting k8s_client...");
    let k8s_client = Client::try_default().await?;

    debug!("Getting ec2_client...");
    let aws_auth_provider = aws_auth_providers::default_provider();
    let aws_config = Config::builder()
        .credentials_provider(aws_auth_provider)
        .build();
    let ec2_client = Ec2Client::from_conf(aws_config);

    debug!("Getting namespace from env...");
    let namespace = std::env::var("NAMESPACE").unwrap_or_else(|_| "default".into());

    debug!("Getting cluster name from env...");
    let cluster_name =
        std::env::var("CLUSTER_NAME").expect("Environment variable CLUSTER_NAME is required.");

    debug!("Getting default tags from env...");
    let default_tags: HashMap<String, String> =
        serde_json::from_str(&std::env::var("DEFAULT_TAGS").unwrap_or_else(|_| "{}".to_owned()))?;

    debug!("Getting pod api");
    let pod_api = Api::<Pod>::namespaced(k8s_client.clone(), &namespace);

    debug!("Cleaning up any orphaned EIPs");
    cleanup_orphan_eips(&ec2_client, &pod_api, &cluster_name).await?;

    info!("Watching for events...");
    let context: Context<ContextData> = Context::new(ContextData::new(
        namespace,
        cluster_name,
        default_tags,
        k8s_client.clone(),
        ec2_client,
    ));
    Controller::new(pod_api, ListParams::default().labels(MANAGE_EIP_LABEL))
        .run(reconcile, on_error, context)
        .for_each(|reconciliation_result| async move {
            match reconciliation_result {
                Ok(resource) => info!("Reconciliation successful. Resource: {:?}", resource),
                Err(err) => error!("Reconciliation error: {:?}", err),
            }
        })
        .await;
    debug!("exiting");
    Ok(())
}
