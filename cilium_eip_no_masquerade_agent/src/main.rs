use std::net::Ipv4Addr;
use std::sync::Arc;
use std::time::Duration;

use futures::{future, StreamExt, TryStream, TryStreamExt};
use ipnetwork::Ipv4Network;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use kube::{Client, ResourceExt};
use kube_runtime::controller::{Action, Controller};
use kube_runtime::finalizer::{finalizer, Event};
use rand::{thread_rng, Rng};
use rtnetlink::packet::rule::Nla;
use rtnetlink::packet::RuleMessage;
use rtnetlink::{new_connection, Handle, IpVersion, RuleHandle};
use tracing::{debug, event, info, instrument, Level};

use eip_operator_shared::{run_with_tracing, Error, MANAGE_EIP_LABEL};

const POD_FINALIZER_NAME: &str = "eip.materialize.cloud/cilium-no-masquerade-rule";

struct ContextData {
    k8s_client: Client,
    handle: Handle,
    vpc_cidr: Ipv4Network,
}

impl ContextData {
    fn new(k8s_client: Client, handle: Handle, vpc_cidr: Ipv4Network) -> ContextData {
        ContextData {
            k8s_client,
            handle,
            vpc_cidr,
        }
    }
}

async fn filter_pod_rules(
    rules: impl TryStream<Ok = RuleMessage, Error = rtnetlink::Error>,
    pod_ip: Ipv4Addr,
) -> Result<Vec<RuleMessage>, rtnetlink::Error> {
    rules
        .try_filter(|rule| {
            future::ready(
                rule.header.src_len == 32
                    && rule.nlas.contains(&Nla::Source(pod_ip.octets().to_vec())),
            )
        })
        .collect::<Vec<Result<RuleMessage, rtnetlink::Error>>>()
        .await
        .into_iter()
        .collect()
}

#[instrument(skip(rule_handle, pod), err)]
async fn apply_pod(
    rule_handle: RuleHandle,
    vpc_cidr: Ipv4Network,
    pod: Arc<Pod>,
) -> Result<Action, Error> {
    let pod_name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
    event!(Level::INFO, pod_name = %pod_name, "Applying pod.");
    let pod_ip: Ipv4Addr = pod
        .status
        .as_ref()
        .ok_or(Error::MissingPodIp)?
        .pod_ip
        .as_ref()
        .ok_or(Error::MissingPodIp)?
        .parse()?;
    let rules = rule_handle.get(IpVersion::V4).execute();
    let pod_rules = filter_pod_rules(rules, pod_ip).await?;

    // Find table from Cilium's installed rule
    let table = pod_rules
        .iter()
        .find_map(|rule| {
            if rule.header.dst_len == vpc_cidr.prefix()
                && rule
                    .nlas
                    .contains(&Nla::Destination(vpc_cidr.network().octets().to_vec()))
            {
                Some(rule.header.table)
            } else {
                None
            }
        })
        .ok_or(Error::CiliumRuleNotFound)?;

    // If our rule doesn't already exist, install it.
    if !pod_rules.iter().any(|rule| {
        rule.header.dst_len == 0 && rule.header.table == table && rule.header.action == 1
    }) {
        rule_handle
            .add()
            .v4()
            .source_prefix(pod_ip, 32)
            .table(table)
            .action(1) // Not sure what this action is, but it matches the one Cilium is using.
            .priority(100)
            .execute()
            .await?;
    }

    Ok(Action::requeue(Duration::from_secs(
        thread_rng().gen_range(240..360),
    )))
}

#[instrument(skip(rule_handle, pod), err)]
async fn cleanup_pod(rule_handle: RuleHandle, pod: Arc<Pod>) -> Result<Action, Error> {
    let pod_name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
    event!(Level::INFO, pod_name = %pod_name, "Cleaning up pod.");
    let pod_ip: Ipv4Addr = pod
        .status
        .as_ref()
        .ok_or(Error::MissingPodIp)?
        .pod_ip
        .as_ref()
        .ok_or(Error::MissingPodIp)?
        .parse()?;
    let rules = rule_handle.get(IpVersion::V4).execute().into_stream();
    let pod_rules = filter_pod_rules(rules, pod_ip).await?;
    for rule in pod_rules {
        if rule.header.dst_len == 0 && rule.header.action == 1 {
            event!(Level::INFO, pod_name = %pod_name, pod_ip = %pod_ip, rule = ?rule, "Deleting rule.");
            rule_handle.del(rule).execute().await?;
            break;
        }
    }
    Ok(Action::await_change())
}

#[instrument(skip(pod, context), err)]
async fn reconcile_pod(
    pod: Arc<Pod>,
    context: Arc<ContextData>,
) -> Result<Action, kube_runtime::finalizer::Error<Error>> {
    let namespace = pod.namespace().unwrap();
    let k8s_client = context.k8s_client.clone();
    let pod_api: Api<Pod> = Api::namespaced(k8s_client.clone(), &namespace);
    let rule_handle = context.handle.rule();
    let vpc_cidr = context.vpc_cidr;

    finalizer(&pod_api, POD_FINALIZER_NAME, pod, |event| async {
        match event {
            Event::Apply(pod) => apply_pod(rule_handle, vpc_cidr, pod).await,
            Event::Cleanup(pod) => cleanup_pod(rule_handle, pod).await,
        }
    })
    .await
}

/// Requeues the operation if there is an error.
fn on_error(_error: &kube_runtime::finalizer::Error<Error>, _context: Arc<ContextData>) -> Action {
    Action::requeue(Duration::from_millis(thread_rng().gen_range(4000..8000)))
}

fn main() -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_with_tracing("cilium-eip-no-masquerade-agent", run))?;
    Ok(())
}

async fn run() -> Result<(), Error> {
    debug!("Getting k8s_client...");
    let k8s_client = Client::try_default().await?;

    debug!("Getting VPC_CIDR from env...");
    let vpc_cidr: Ipv4Network = std::env::var("VPC_CIDR")
        .expect("VPC_CIDR env var must be set to the Cilium native routing CIDR.")
        .parse()
        .expect("Invalid VPC_CIDR.");

    debug!("Getting NODE_NAME from env...");
    let node_name = std::env::var("NODE_NAME")
        .expect("NODE_NAME env var must be set to the name of the kubernetes node this agent is running on.");

    debug!("Getting namespace from env...");
    let namespace = std::env::var("NAMESPACE").ok();

    debug!("Getting pod api");
    let pod_api = match namespace {
        Some(ref namespace) => Api::<Pod>::namespaced(k8s_client.clone(), namespace),
        None => Api::<Pod>::all(k8s_client.clone()),
    };

    debug!("Getting rtnetlink handle");
    let (connection, handle, _) = new_connection()?;
    tokio::spawn(connection);

    info!("Watching for events...");
    let context = Arc::new(ContextData::new(k8s_client, handle, vpc_cidr));
    Controller::new(
        pod_api,
        ListParams::default()
            .labels(MANAGE_EIP_LABEL)
            .fields(&format!("spec.nodeName={}", node_name)),
    )
    .run(reconcile_pod, on_error, context.clone())
    .for_each(|reconciliation_result| async move {
        match reconciliation_result {
            Ok(resource) => {
                event!(Level::INFO, pod_name = %resource.0.name, "Pod reconciliation successful.");
            }
            Err(err) => event!(Level::ERROR, err = %err, "Pod reconciliation error."),
        }
    })
    .await;

    debug!("exiting");
    Ok(())
}
