use std::collections::HashSet;

use futures::{future, TryStreamExt};
use iptables::IPTables;
use netlink_packet_route::rule::Nla;
use rtnetlink::{new_connection, IpVersion, RuleHandle};
use tokio::time::{sleep, Duration};
use tracing::{event, info, trace, Level};

use eip_operator_shared::{run_with_tracing, Error};

const TABLE: &str = "mangle";
const CHAIN: &str = "CILIUM_PRE_mangle";
const FW_MASK: u32 = 0xf;
// eth1
const FIRST_SECONDARY_ENI_INDEX: u32 = 1;
// eth15, No AWS instance type supports more than 15 ENIs
const LAST_SECONDARY_ENI_INDEX: u32 = 15;

struct RuleManager {
    iptables: IPTables,
    ip_rule_handle: RuleHandle,
}

impl RuleManager {
    async fn new() -> RuleManager {
        let iptables = iptables::new(false).unwrap();

        let (connection, rtnetlink_handle, _) = new_connection().unwrap();
        let ip_rule_handle = rtnetlink_handle.rule();
        tokio::spawn(connection);

        RuleManager {
            iptables,
            ip_rule_handle,
        }
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

        let delay_secs = 1;
        info!("Done! Will recheck in {delay_secs} seconds");
        sleep(Duration::from_secs(delay_secs)).await;
    }
}
