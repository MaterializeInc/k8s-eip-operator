use std::net::Ipv4Addr;

use futures::{future, StreamExt, TryStream, TryStreamExt};
use ipnetwork::Ipv4Network;
use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams};
use kube::Client;
use rtnetlink::packet::rule::Nla;
use rtnetlink::packet::RuleMessage;
use rtnetlink::{new_connection, Handle, IpVersion};
use tracing::{debug, event, info, instrument, Level};

use eip_operator_shared::controller::Controller;
use eip_operator_shared::{run_with_tracing, Error, MANAGE_EIP_LABEL};

struct Context {
    handle: Handle,
    vpc_cidr: Ipv4Network,
}

impl Context {
    fn new(handle: Handle, vpc_cidr: Ipv4Network) -> Self {
        Self { handle, vpc_cidr }
    }
}

#[async_trait::async_trait]
impl eip_operator_shared::controller::Context for Context {
    type Resource = Pod;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/cilium-no-masquerade-rule";

    #[instrument(skip(self, _client, _api, pod), err)]
    async fn apply(
        &self,
        _client: Client,
        _api: Api<Self::Resource>,
        pod: &Self::Resource,
    ) -> Result<(), Self::Error> {
        let name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
        event!(Level::INFO, name = %name, "Applying pod.");
        let pod_ip: Ipv4Addr = pod
            .status
            .as_ref()
            .ok_or(Error::MissingPodIp)?
            .pod_ip
            .as_ref()
            .ok_or(Error::MissingPodIp)?
            .parse()?;
        let rule_handle = self.handle.rule();
        let rules = rule_handle.get(IpVersion::V4).execute();
        let pod_rules = filter_pod_rules(rules, pod_ip).await?;

        // Find table from Cilium's installed rule
        let table = pod_rules
            .iter()
            .find_map(|rule| {
                if rule.header.dst_len == self.vpc_cidr.prefix()
                    && rule
                        .nlas
                        .contains(&Nla::Destination(self.vpc_cidr.network().octets().to_vec()))
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

        Ok(())
    }

    #[instrument(skip(self, _client, _api, pod), err)]
    async fn cleanup(
        &self,
        _client: Client,
        _api: Api<Self::Resource>,
        pod: &Self::Resource,
    ) -> Result<(), Self::Error> {
        let name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
        event!(Level::INFO, name = %name, "Cleaning up pod.");

        // Assuming that if it doesn't have an IP during cleanup, that it never had one.
        if let Some(pod_ip_str) = &pod
            .status
            .as_ref()
            .and_then(|status| status.pod_ip.as_ref())
        {
            let pod_ip: Ipv4Addr = pod_ip_str.parse()?;
            let rule_handle = self.handle.rule();
            let rules = rule_handle.get(IpVersion::V4).execute().into_stream();
            let pod_rules = filter_pod_rules(rules, pod_ip).await?;
            for rule in pod_rules {
                if rule.header.dst_len == 0 && rule.header.action == 1 {
                    event!(Level::INFO, pod_name = %name, pod_ip = %pod_ip, rule = ?rule, "Deleting rule.");
                    rule_handle.del(rule).execute().await?;
                    break;
                }
            }
        }
        Ok(())
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

    debug!("Getting rtnetlink handle");
    let (connection, handle, _) = new_connection()?;
    tokio::spawn(connection);

    info!("Watching for events...");
    let context = Context::new(handle, vpc_cidr);
    let lp = ListParams::default()
        .labels(MANAGE_EIP_LABEL)
        .fields(&format!("spec.nodeName={}", node_name));
    let controller = match namespace {
        Some(ref namespace) => Controller::namespaced(namespace, k8s_client, lp, context),
        None => Controller::namespaced_all(k8s_client, lp, context),
    };
    controller.run().await;

    debug!("exiting");
    Ok(())
}
