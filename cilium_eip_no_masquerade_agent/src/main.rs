use std::collections::HashSet;
use std::net::Ipv4Addr;

use futures::{future, TryStream, TryStreamExt};
use iptables::IPTables;
use json_patch::{PatchOperation, RemoveOperation, TestOperation};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{ListParams, Patch, PatchParams};
use kube::{Api, Client as KubeClient, ResourceExt};
use netlink_packet_route::rule::Nla;
use netlink_packet_route::RuleMessage;
use rtnetlink::{new_connection, IpVersion, RuleHandle};
use tokio::time::{sleep, Duration};
use tracing::{event, info, trace, Level};

use eip_operator_shared::{run_with_tracing, Error, MANAGE_EIP_LABEL};

const FINALIZER_NAME: &str = "eip.materialize.cloud/cilium-no-masquerade-rule";
const TABLE: &str = "mangle";
const CHAIN: &str = "CILIUM_PRE_mangle";
const FW_MASK: u32 = 0xf;
// eth1
const FIRST_SECONDARY_ENI_INDEX: u32 = 1;
// eth15, No AWS instance type supports more than 15 ENIs
const LAST_SECONDARY_ENI_INDEX: u32 = 15;

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
        .try_collect()
        .await
}

struct RuleManager {
    iptables: IPTables,
    ip_rule_handle: RuleHandle,
    kube_client: KubeClient,
    global_pod_api: Api<Pod>,
    node_name: String,
}

impl RuleManager {
    async fn new() -> RuleManager {
        let iptables = iptables::new(false).unwrap();

        let (connection, rtnetlink_handle, _) = new_connection().unwrap();
        let ip_rule_handle = rtnetlink_handle.rule();
        tokio::spawn(connection);

        let kube_client = KubeClient::try_default().await.unwrap();
        let global_pod_api: Api<Pod> = Api::all(kube_client.clone());

        let node_name = std::env::var("NODE_NAME")
            .expect("NODE_NAME env var must be set to the name of the kubernetes node this agent is running on.");

        RuleManager {
            iptables,
            ip_rule_handle,
            kube_client,
            global_pod_api,
            node_name,
        }
    }

    async fn cleanup_legacy_per_pod_rules(&self, pod: &Pod) -> Result<(), Error> {
        let pod_name = pod.name_unchecked();

        // Assuming that if it doesn't have an IP during cleanup, that it never had one.
        if let Some(pod_ip_str) = &pod
            .status
            .as_ref()
            .and_then(|status| status.pod_ip.as_ref())
        {
            let pod_ip: Ipv4Addr = pod_ip_str.parse()?;
            let rules = self
                .ip_rule_handle
                .get(IpVersion::V4)
                .execute()
                .into_stream();
            let pod_rules = filter_pod_rules(rules, pod_ip).await?;
            if let Some(rule) = pod_rules
                .into_iter()
                .find(|rule| rule.header.dst_len == 0 && rule.header.action == 1)
            {
                event!(Level::INFO, pod_name = %pod_name, pod_ip = %pod_ip, rule = ?rule, "Deleting rule.");
                self.ip_rule_handle.del(rule).execute().await?;
            }
        }
        self.remove_finalizer(pod, &pod_name).await?;
        Ok(())
    }

    async fn remove_finalizer(&self, pod: &Pod, pod_name: &str) -> Result<(), Error> {
        // https://docs.rs/kube-runtime/latest/src/kube_runtime/finalizer.rs.html
        let finalizer_index = pod
            .finalizers()
            .iter()
            .enumerate()
            .find(|(_, finalizer)| *finalizer == FINALIZER_NAME)
            .map(|(i, _)| i);
        if let Some(finalizer_index) = finalizer_index {
            let pod_api: Api<Pod> =
                Api::namespaced(self.kube_client.clone(), &pod.namespace().unwrap());
            let finalizer_path = format!("/metadata/finalizers/{finalizer_index}");
            pod_api
                .patch::<Pod>(
                    pod_name,
                    &PatchParams::default(),
                    &Patch::Json(json_patch::Patch(vec![
                        // All finalizers run concurrently and we use an integer index
                        // `Test` ensures that we fail instead of deleting someone else's finalizer
                        PatchOperation::Test(TestOperation {
                            path: finalizer_path.clone(),
                            value: FINALIZER_NAME.into(),
                        }),
                        PatchOperation::Remove(RemoveOperation {
                            path: finalizer_path,
                        }),
                    ])),
                )
                .await?;
        }
        Ok(())
    }

    async fn wait_for_chain_to_exist(&self) -> Result<(), Box<dyn std::error::Error>> {
        info!("Waiting for {CHAIN} chain to exist");
        while !self.iptables.chain_exists(TABLE, CHAIN)? {
            trace!("{CHAIN} does not exist, sleeping");
            sleep(Duration::from_secs(1)).await;
        }
        Ok(())
    }

    async fn get_eni_indexes_for_existing_ip_rules(
        &self,
    ) -> Result<HashSet<u32>, rtnetlink::Error> {
        let rules = self.ip_rule_handle.get(IpVersion::V4).execute();
        rules
            .try_filter(|rule| future::ready(rule.nlas.contains(&Nla::FwMask(FW_MASK))))
            .map_ok(|rule| {
                rule.nlas
                    .into_iter()
                    .find_map(|nla| match nla {
                        Nla::FwMark(i) => Some(i),
                        _ => None,
                    })
                    .expect("There should always be an associated mark if there is a mask.")
            })
            .try_collect()
            .await
    }

    fn ensure_iptables_rule(&self, rule: &str) -> Result<(), Box<dyn std::error::Error>> {
        if !self.iptables.exists(TABLE, CHAIN, rule)? {
            event!(Level::INFO, rule = ?rule, "Appending iptables rule");
            self.iptables.append(TABLE, CHAIN, rule)?;
        }
        Ok(())
    }

    fn ensure_restore_mark_iptables_rule(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let rule = format!("-i lxc+ -m comment --comment \"cilium: secondary interfaces\" -j CONNMARK --restore-mark --nfmask {FW_MASK} --ctmask {FW_MASK}");
        self.ensure_iptables_rule(&rule)?;
        Ok(())
    }

    fn ensure_mark_iptables_rule(
        &mut self,
        interface_index: u32,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let rule = format!("-i eth{interface_index} -m comment --comment \"cilium: eth{interface_index}\" -m addrtype --dst-type UNICAST --limit-iface-in -j CONNMARK --set-xmark 0x{interface_index:x}/{FW_MASK}");
        self.ensure_iptables_rule(&rule)?;
        Ok(())
    }

    async fn insert_ip_rule(&mut self, interface_index: u32) -> Result<(), rtnetlink::Error> {
        info!("Inserting ip rule for eth{interface_index}");
        // ip rule add pref 200 from all fwmark 0x{i}/{FW_MASK} lookup {10 + i}
        //
        // The 200 priority is somewhat arbitrary, but it is after the rules Cilium injects
        // for the eth0 marks and for in-VPC traffic, and after the local and pod inbound rules.
        let mut rule_add_request = self
            .ip_rule_handle
            .add()
            .v4()
            // Cilium starts numbering tables for secondary ENI local traffic at 11.
            .table_id(10 + interface_index)
            .action(1) // Not sure what this action is, but it seems to work
            .priority(200);
        let message = rule_add_request.message_mut();
        message.nlas.push(Nla::FwMark(interface_index));
        message.nlas.push(Nla::FwMask(FW_MASK));
        rule_add_request.execute().await
    }
}

fn main() -> Result<(), Error> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(run_with_tracing("cilium-eip-no-masquerade-agent", run))?;
    Ok(())
}

async fn run() -> Result<(), Error> {
    let mut manager = RuleManager::new().await;

    loop {
        manager.wait_for_chain_to_exist().await.unwrap();

        manager.ensure_restore_mark_iptables_rule().unwrap();

        let existing_ip_rules = manager.get_eni_indexes_for_existing_ip_rules().await?;
        for i in FIRST_SECONDARY_ENI_INDEX..=LAST_SECONDARY_ENI_INDEX {
            manager.ensure_mark_iptables_rule(i).unwrap();

            if !existing_ip_rules.contains(&i) {
                manager.insert_ip_rule(i).await.unwrap();
            }
        }

        let pods = manager
            .global_pod_api
            .list(
                &ListParams::default()
                    .labels(MANAGE_EIP_LABEL)
                    .fields(&format!("spec.nodeName={}", manager.node_name)),
            )
            .await?;
        for pod in pods {
            manager.cleanup_legacy_per_pod_rules(&pod).await?;
        }

        let delay_secs = 1;
        info!("Done! Will recheck in {delay_secs} seconds");
        sleep(Duration::from_secs(delay_secs)).await;
    }
}
