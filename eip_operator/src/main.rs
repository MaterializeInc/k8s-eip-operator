use std::collections::{HashMap, HashSet};
use std::time::Duration;

use aws_sdk_ec2::types::Filter;
use aws_sdk_ec2::Client as Ec2Client;
use aws_sdk_servicequotas::types::ServiceQuota;
use aws_sdk_servicequotas::Client as ServiceQuotaClient;
use futures::future::join_all;
use json_patch::{PatchOperation, RemoveOperation, TestOperation};
use k8s_controller::Controller;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, Resource, ResourceExt};
use tokio::task;
use tracing::{debug, event, info, instrument, Level};

use eip_operator_shared::{run_with_tracing, Error, MANAGE_EIP_LABEL};

use eip::v2::Eip;

mod aws;
mod controller;
mod eip;
mod kube_ext;

const LEGACY_MANAGE_EIP_LABEL: &str = "eip.aws.materialize.com/manage";
const LEGACY_POD_FINALIZER_NAME: &str = "eip.aws.materialize.com/disassociate";

const FIELD_MANAGER: &str = "eip.materialize.cloud";
const AUTOCREATE_EIP_LABEL: &str = "eip.materialize.cloud/autocreate_eip";
const EIP_ALLOCATION_ID_ANNOTATION: &str = "eip.materialize.cloud/allocation_id";
const EXTERNAL_DNS_TARGET_ANNOTATION: &str = "external-dns.alpha.kubernetes.io/target";

// See https://us-east-1.console.aws.amazon.com/servicequotas/home/services/ec2/quotas
// and filter in the UI for EC2 quotas like this, or use the CLI:
//   aws --profile=mz-cloud-staging-admin service-quotas list-service-quotas --service-code=ec2
const EIP_QUOTA_CODE: &str = "L-0263D0A3";

// Watch our EIP quota status on a fixed interval
const EIP_QUOTA_INTERVAL: tokio::time::Duration = Duration::from_secs(60);

fn main() -> Result<(), Error> {
    set_abort_on_panic();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_with_tracing("eip_operator", run))?;
    Ok(())
}

async fn run() -> Result<(), Error> {
    debug!("Getting k8s_client...");
    let k8s_client = Client::try_default().await?;

    debug!("Getting ec2_client...");
    let mut config_loader = eip_operator_shared::aws_config_loader_default();

    if let Ok(endpoint) = std::env::var("AWS_ENDPOINT_URL") {
        config_loader = config_loader.endpoint_url(endpoint);
    }
    let aws_config = config_loader.load().await;
    let ec2_client = Ec2Client::new(&aws_config);

    debug!("Getting quota_client...");
    let quota_client = ServiceQuotaClient::new(&aws_config);

    debug!("Getting namespace from env...");
    let namespace = std::env::var("NAMESPACE").ok();

    debug!("Getting cluster name from env...");
    let cluster_name =
        std::env::var("CLUSTER_NAME").expect("Environment variable CLUSTER_NAME is required.");

    debug!("Getting default tags from env...");
    let default_tags: HashMap<String, String> =
        serde_json::from_str(&std::env::var("DEFAULT_TAGS").unwrap_or_else(|_| "{}".to_owned()))?;

    eip::register_custom_resource(k8s_client.clone(), namespace.as_deref()).await?;

    debug!("Getting pod api");
    let pod_api = match namespace {
        Some(ref namespace) => Api::<Pod>::namespaced(k8s_client.clone(), namespace),
        None => Api::<Pod>::all(k8s_client.clone()),
    };

    debug!("Getting eip api");
    let eip_api = match namespace {
        Some(ref namespace) => Api::<Eip>::namespaced(k8s_client.clone(), namespace),
        None => Api::<Eip>::all(k8s_client.clone()),
    };

    debug!("Cleaning up any orphaned EIPs");
    cleanup_orphan_eips(
        &ec2_client,
        &eip_api,
        &pod_api,
        &cluster_name,
        namespace.as_deref(),
    )
    .await?;

    info!("Starting tasks");
    let mut tasks = vec![];
    tasks.push({
        let ec2_client = ec2_client.clone();
        task::spawn(async move {
            let mut interval = tokio::time::interval(EIP_QUOTA_INTERVAL);
            // It's better to miss the occasional measurement than to hammer the endpoint
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            loop {
                interval.tick().await;
                // Note: the Err that might occur here will be handled by tracing
                // instrumentation, rather than directly here.
                if let Err(err) = report_eip_quota_status(&ec2_client, &quota_client).await {
                    event!(Level::ERROR, err = %err, "Quota reporting error");
                }
            }
        })
    });

    tasks.push({
        let context = controller::pod::Context::new(ec2_client.clone());
        let watch_config = kube_runtime::watcher::Config::default().labels(MANAGE_EIP_LABEL);

        let pod_controller = match &namespace {
            Some(namespace) => {
                Controller::namespaced(k8s_client.clone(), context, namespace, watch_config)
            }
            None => Controller::namespaced_all(k8s_client.clone(), context, watch_config),
        };
        task::spawn(pod_controller.run())
    });

    tasks.push({
        let context = controller::node::Context::new(ec2_client.clone(), namespace.clone());
        let watch_config = kube_runtime::watcher::Config::default().labels(MANAGE_EIP_LABEL);
        let node_controller = Controller::cluster(k8s_client.clone(), context, watch_config);
        task::spawn(node_controller.run())
    });

    tasks.push({
        let context = controller::eip::Context::new(ec2_client, cluster_name, default_tags);
        let watch_config: kube_runtime::watcher::Config = Default::default();
        let eip_controller = match &namespace {
            Some(namespace) => Controller::namespaced(k8s_client, context, namespace, watch_config),
            None => Controller::namespaced_all(k8s_client, context, watch_config),
        };
        task::spawn(eip_controller.run())
    });

    join_all(tasks).await;

    debug!("exiting");
    Ok(())
}

/// Finds all EIPs tagged for this cluster, then compares them to the pod UIDs. If the EIP is not
/// tagged with a pod UID, or the UID does not exist in this cluster, it deletes the EIP.
#[instrument(skip(ec2_client, eip_api, pod_api), err)]
async fn cleanup_orphan_eips(
    ec2_client: &Ec2Client,
    eip_api: &Api<Eip>,
    pod_api: &Api<Pod>,
    cluster_name: &str,
    namespace: Option<&str>,
) -> Result<(), Error> {
    let mut describe_addresses = ec2_client.describe_addresses().filters(
        Filter::builder()
            .name(format!("tag:{}", aws::CLUSTER_NAME_TAG))
            .values(cluster_name.to_owned())
            .build(),
    );
    if let Some(namespace) = namespace {
        describe_addresses = describe_addresses.filters(
            Filter::builder()
                .name(format!("tag:{}", aws::NAMESPACE_TAG))
                .values(namespace.to_owned())
                .build(),
        )
    }
    let mut addresses = describe_addresses
        .send()
        .await?
        .addresses
        .ok_or(Error::MissingAddresses)?;

    let mut legacy_addresses = aws::describe_addresses_with_tag_value(
        ec2_client,
        aws::LEGACY_CLUSTER_NAME_TAG,
        cluster_name,
    )
    .await?
    .addresses
    .ok_or(Error::MissingAddresses)?;

    addresses.append(&mut legacy_addresses);

    let eip_uids: HashSet<String> = eip_api
        .list(&ListParams::default())
        .await?
        .into_iter()
        .filter_map(|eip| eip.metadata.uid)
        .collect();

    for address in addresses {
        let eip_uid = aws::get_tag_from_address(&address, aws::EIP_UID_TAG);
        if eip_uid.is_none() || !eip_uids.contains(eip_uid.unwrap()) {
            event!(Level::WARN,
                allocation_id = %address.allocation_id.as_deref().unwrap_or("None"),
                eip_uid = %eip_uid.unwrap_or("None"),
                "Cleaning up orphaned EIP",
            );
            aws::disassociate_and_release_address(ec2_client, &address).await?;
        }
    }

    // Manually remove the old finalizer, since we just removed the EIPs.
    // https://docs.rs/kube-runtime/0.65.0/src/kube_runtime/finalizer.rs.html#133
    let legacy_pods = pod_api
        .list(&ListParams::default().labels(LEGACY_MANAGE_EIP_LABEL))
        .await?;
    for pod in legacy_pods {
        if let Some(position) = pod
            .finalizers()
            .iter()
            .position(|s| s == LEGACY_POD_FINALIZER_NAME)
        {
            let pod_name = pod.meta().name.as_ref().ok_or(Error::MissingPodName)?;
            let finalizer_path = format!("/metadata/finalizers/{}", position);
            pod_api
                .patch::<Pod>(
                    pod_name,
                    &PatchParams::default(),
                    &Patch::Json(json_patch::Patch(vec![
                        PatchOperation::Test(TestOperation {
                            path: finalizer_path.clone(),
                            value: LEGACY_POD_FINALIZER_NAME.into(),
                        }),
                        PatchOperation::Remove(RemoveOperation {
                            path: finalizer_path,
                        }),
                    ])),
                )
                .await?;
        }
    }
    Ok(())
}

#[instrument(skip(ec2_client, quota_client), err)]
async fn report_eip_quota_status(
    ec2_client: &Ec2Client,
    quota_client: &ServiceQuotaClient,
) -> Result<(), Error> {
    let addresses_result = ec2_client.describe_addresses().send().await?;
    let allocated = addresses_result.addresses().len();
    let quota_result = quota_client
        .get_service_quota()
        .service_code("ec2")
        .quota_code(EIP_QUOTA_CODE)
        .send()
        .await?;
    let quota = quota_result
        .quota()
        .and_then(|q: &ServiceQuota| q.value)
        .unwrap_or(0f64);
    event!(Level::INFO, eips_allocated = %allocated, eip_quota = %quota, "eip_quota_checked");
    Ok(())
}

fn set_abort_on_panic() {
    let old_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        old_hook(panic_info);
        std::process::abort();
    }));
}
