use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::Client;
use kube::Resource;
use kube_runtime::controller::{Context, Controller, ReconcilerAction};
use rusoto_core::region::Region;
use rusoto_core::RusotoError;
use rusoto_ec2::{
    AssociateAddressError, AssociateAddressRequest, AssociateAddressResult, DescribeAddressesError,
    DescribeAddressesRequest, DescribeAddressesResult, DescribeInstancesError,
    DescribeInstancesRequest, DescribeInstancesResult, DisassociateAddressError,
    DisassociateAddressRequest, Ec2, Ec2Client, Filter,
};
use serde_json::{json, Value};
use std::fmt::Debug;
use std::time::Duration;

struct ContextData {
    k8s_client: Client,
    ec2_client: Ec2Client,
}

impl ContextData {
    fn new(k8s_client: Client, ec2_client: Ec2Client) -> ContextData {
        ContextData {
            k8s_client,
            ec2_client,
        }
    }
}

enum Action {
    Associate,
    Disassociate,
    NoOp,
}

fn determine_action(pod: &Pod) -> Action {
    return if pod.meta().deletion_timestamp.is_some() {
        Action::Disassociate
    } else if pod.meta().finalizers.is_empty() {
        // TODO actually check if it's attached?
        Action::Associate
    } else {
        Action::NoOp
    };
}

async fn add_finalizer(
    k8s_client: Client,
    name: &str,
    namespace: &str,
) -> Result<Pod, kube::Error> {
    let api: Api<Pod> = Api::namespaced(k8s_client, namespace);
    let finalizer: Value = json!({
        "metadata": {
            "finalizers": ["eip.aws.materialize.com/disassociate"]
        }
    });

    let patch: Patch<&Value> = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}

async fn remove_finalizer(
    k8s_client: Client,
    name: &str,
    namespace: &str,
) -> Result<Pod, kube::Error> {
    let api: Api<Pod> = Api::namespaced(k8s_client, namespace);
    // TODO only remove our finalizer
    let finalizer: Value = json!({
        "metadata": {
            "finalizers": null
        }
    });

    let patch: Patch<&Value> = Patch::Merge(&finalizer);
    Ok(api.patch(name, &PatchParams::default(), &patch).await?)
}

async fn describe_instance_with_ip(
    ec2_client: &Ec2Client,
    pod_ip: String,
) -> Result<DescribeInstancesResult, RusotoError<DescribeInstancesError>> {
    ec2_client
        .describe_instances(DescribeInstancesRequest {
            dry_run: None,
            // TODO filter by VPC also?
            filters: Some(vec![Filter {
                name: Some("network-interface.addresses.private-ip-address".to_owned()),
                values: Some(vec![pod_ip]),
            }]),
            instance_ids: None,
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
    eip_id: String,
) -> Result<DescribeAddressesResult, RusotoError<DescribeAddressesError>> {
    ec2_client
        .describe_addresses(DescribeAddressesRequest {
            allocation_ids: Some(vec![eip_id]),
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
    // TODO get association ID
    ec2_client
        .disassociate_address(DisassociateAddressRequest {
            association_id: Some(association_id),
            dry_run: None,
            public_ip: None,
        })
        .await
}

// https://dzone.com/articles/oxidizing-the-kubernetes-operator
async fn reconcile(pod: Pod, context: Context<ContextData>) -> Result<ReconcilerAction, Error> {
    let eip_id = &pod.meta().labels["eip.aws.materialize.com/id"];
    let ec2_client = context.get_ref().ec2_client.clone();
    let k8s_client = context.get_ref().k8s_client.clone();
    let pod_name = pod
        .meta()
        .name
        .as_ref()
        .ok_or_else(|| Error::MissingPodName)?;
    let pod_namespace = pod
        .meta()
        .namespace
        .as_ref()
        .ok_or_else(|| Error::MissingPodNamespace)?;

    if let Some(PodStatus {
        pod_ip: Some(pod_ip),
        ..
    }) = &pod.status
    {
        return match determine_action(&pod) {
            Action::Associate => {
                println!("Associating...");
                let instance_description =
                    describe_instance_with_ip(&ec2_client, pod_ip.to_owned()).await?;
                let instance = &instance_description
                    .reservations
                    .as_ref()
                    .ok_or_else(|| Error::MissingReservations)?[0]
                    .instances
                    .as_ref()
                    .ok_or_else(|| Error::MissingInstances)?[0];
                let eni_id = instance
                    .network_interfaces
                    .as_ref()
                    .ok_or_else(|| Error::MissingNetworkInterfaces)?
                    .iter()
                    .find_map(|nic| {
                        nic.private_ip_addresses.as_ref()?.iter().find_map(|ip| {
                            match ip.private_ip_address.as_ref()? {
                                x if x == pod_ip => {
                                    Some(nic.network_interface_id.as_ref()?.to_owned())
                                }
                                _ => None,
                            }
                        })
                    })
                    .ok_or_else(|| Error::NoInterfaceWithThatIp)?;
                add_finalizer(k8s_client, &pod_name, &pod_namespace).await?;
                associate_eip_with_pod_eni(
                    &ec2_client,
                    eip_id.to_owned(),
                    eni_id,
                    pod_ip.to_owned(),
                )
                .await?;
                Ok(ReconcilerAction {
                    requeue_after: Some(Duration::from_secs(30)),
                })
            }
            Action::Disassociate => {
                println!("Disassociating...");
                if let Some(association_id) = &describe_addresses(&ec2_client, eip_id.to_owned())
                    .await?
                    .addresses
                    .ok_or_else(|| Error::MissingAddresses)?[0]
                    .association_id
                {
                    disassociate_eip(&ec2_client, association_id.to_owned()).await?;
                }
                remove_finalizer(k8s_client, &pod_name, &pod_namespace).await?;
                Ok(ReconcilerAction {
                    requeue_after: None,
                })
            }
            Action::NoOp => {
                println!("NoOp...");
                Ok(ReconcilerAction {
                    requeue_after: Some(Duration::from_secs(30)),
                })
            }
        };
    }
    Ok(ReconcilerAction {
        requeue_after: Some(Duration::from_secs(30)),
    })
}

fn on_error(error: &Error, _context: Context<ContextData>) -> ReconcilerAction {
    eprintln!("Reconciliation error:\n{:?}", error);
    ReconcilerAction {
        requeue_after: Some(Duration::from_secs(10)),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let k8s_client = Client::try_default().await?;
    let ec2_client = Ec2Client::new(Region::UsEast1);
    let namespace = std::env::var("NAMESPACE").unwrap_or_else(|_| "default".into());

    let api = Api::<Pod>::namespaced(k8s_client.clone(), &namespace);
    let context: Context<ContextData> =
        Context::new(ContextData::new(k8s_client.clone(), ec2_client));
    Controller::new(
        api,
        ListParams::default().labels("eip.aws.materialize.com/id"),
    )
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

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("Kubernetes reported error: {source}")]
    KubeError {
        #[from]
        source: kube::Error,
    },
    #[error("Pod does not have a name.")]
    MissingPodName,
    #[error("Pod does not have a namespace.")]
    MissingPodNamespace,
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
