use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};
use kube_runtime::controller::Action;
use tracing::{event, info, instrument, Level};

use eip_operator_shared::Error;

use crate::eip::v2::Eip;

pub(crate) struct Context {
    ec2_client: aws_sdk_ec2::Client,
}

impl Context {
    pub(crate) fn new(ec2_client: aws_sdk_ec2::Client) -> Self {
        Self { ec2_client }
    }
}

#[async_trait::async_trait]
impl k8s_controller::Context for Context {
    type Resource = Pod;
    type Error = Error;

    const FINALIZER_NAME: &'static str = "eip.materialize.cloud/disassociate";

    #[instrument(skip(self, client, pod), err)]
    async fn apply(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        // find matching eip and claim, create it if should autocreate
        let name = pod.metadata.name.as_ref().ok_or(Error::MissingPodName)?;
        let eip_api = Api::<Eip>::namespaced(client.clone(), &pod.namespace().unwrap());
        let pod_api = Api::<Pod>::namespaced(client.clone(), &pod.namespace().unwrap());

        // create eip if should auto-create
        if should_autocreate_eip(pod) {
            event!(Level::INFO, should_autocreate_eip = true);
            crate::eip::create_for_pod(&eip_api, name).await?;
        }
        let eips = eip_api.list(&ListParams::default()).await?.items;
        // Cases
        // there are some eips
        // - we are associated with one
        //   - it is correct
        //     - done - register dns labels - ok
        //   - it is incorrect
        //     - ask eip to reoncile, reschedule
        // - we are not associated with any
        //   - we want to be
        //     - an eip is not associated (potentially free to grab)
        //       - we ask to claim it
        //     - it is associated (someone else is using it)
        //       - done ok
        //   - we do not want to be
        //     - done ok
        // no eips
        // - done ok
        if !eips.is_empty() {
            // there are eips
            if let Some(eip) = eips.iter().find(|eip| eip.associated_with_pod(pod)) {
                // we are associated with an eip
                if eip.matches_pod_name(&pod.name_unchecked()) {
                    // we are associated, it is correct
                    info!(
                        "Pod {} is already attached to EIP {}, setting up DNS target annotation",
                        pod.name_unchecked(),
                        eip.name_unchecked()
                    );
                    let allocation_id = eip.allocation_id().ok_or(Error::MissingAllocationId)?;
                    let eip_description =
                        crate::aws::describe_address(&self.ec2_client, allocation_id)
                            .await?
                            .addresses
                            .ok_or(Error::MissingAddresses)?
                            .swap_remove(0);
                    let public_ip = eip_description.public_ip.ok_or(Error::MissingPublicIp)?;
                    add_dns_target_annotation(&pod_api, name, &public_ip, allocation_id).await?;
                    return Ok(None);
                } else {
                    // we are associated, it is incorrect
                    info!("Pod {} is attached to EIP {}, but should not be, asking eip to reconcile, and rescheduling reconciliation", pod.name_unchecked(), eip.name_unchecked());
                    crate::eip::trigger_reconciliation(&eip_api, &eip.name_unchecked()).await?;
                    return Ok(Some(Action::requeue(Duration::from_secs(2))));
                }
            } else {
                // we are not associated
                if let Some(eip) = eips
                    .iter()
                    .find(|eip| eip.matches_pod_name(&pod.name_unchecked()) && !eip.attached())
                {
                    // we have an eip we want to associate with
                    info!("Pod {} found a matching unattached EIP {}, asking it to reconcile, and rescheduling reconciliation", eip.name_unchecked(), eip.name_unchecked());
                    crate::eip::trigger_reconciliation(&eip_api, &eip.name_unchecked()).await?;
                    return Ok(Some(Action::requeue(Duration::from_secs(2))));
                } else {
                    // There are no eips we want to associate with or none are free
                    info!(
                        "No matching unattached EIPs for Pod {}",
                        pod.name_unchecked()
                    );
                    return Ok(None);
                }
            }
        } else {
            // there are no eips
            info!(
                "No matching unattached EIPs for Pod {}",
                pod.name_unchecked()
            );
            return Ok(None);
        }
    }

    #[instrument(skip(self, client, pod), err)]
    async fn cleanup(
        &self,
        client: Client,
        pod: &Self::Resource,
    ) -> Result<Option<Action>, Self::Error> {
        // remove claim, delete if autocreate
        let eip_api = Api::<Eip>::namespaced(client.clone(), &pod.namespace().unwrap());
        let eip = eip_api
            .list(&ListParams::default())
            .await?
            .items
            .into_iter()
            .find(|eip| eip.matches_pod_name(&pod.name_unchecked()));

        if let Some(eip) = eip {
            info!(
                "Pod{} still attached to EIP {}, asking eip to re-reconcile",
                pod.name_unchecked(),
                eip.name_unchecked()
            );
            // If this is pod auto-creates an eip we should
            // just delete the eip
            if should_autocreate_eip(pod) {
                event!(Level::INFO, should_autocreate_eip = true);
                crate::eip::delete(&eip_api, &eip.name_unchecked()).await?;
                return Ok(None);
            }
            // We're going to try to disassociate here by asking the eip
            // to reconile and waiting for some attempts for it to
            // disassociate with the pod
            crate::eip::trigger_reconciliation(&eip_api, &eip.name_unchecked()).await?;
            for _ in 0..5 {
                let eip = eip_api.get_opt(&eip.name_unchecked()).await?;
                if let Some(eip) = eip {
                    if !eip.associated_with_pod(pod) {
                        info!(
                            "Pod {} has been disassociated with EIP {}",
                            pod.name_unchecked(),
                            eip.name_unchecked()
                        );
                        return Ok(None);
                    }
                } else {
                    info!("Pod {} no longer has an attached eip", pod.name_unchecked());
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
            return Err(Error::PodFailedToRemoveEip);
        } else {
            info!(
                "Pod {} is not attached to any EIP, continuing with cleanup",
                pod.name_unchecked()
            );
            Ok(None)
        }
    }
}

/// Checks if the autocreate label is set to true on a pod.
fn should_autocreate_eip(pod: &Pod) -> bool {
    pod.metadata
        .labels
        .as_ref()
        .and_then(|label| {
            label
                .get(crate::AUTOCREATE_EIP_LABEL)
                .map(|s| (*s).as_ref())
        })
        .unwrap_or("false")
        .to_lowercase()
        == "true"
}

/// Applies annotation to pod specifying the target IP for external-dns.
#[instrument(skip(api), err)]
async fn add_dns_target_annotation(
    api: &Api<Pod>,
    name: &str,
    eip_address: &str,
    allocation_id: &str,
) -> Result<Pod, kube::Error> {
    let patch = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "annotations": {
                crate::EIP_ALLOCATION_ID_ANNOTATION: allocation_id,
                crate::EXTERNAL_DNS_TARGET_ANNOTATION: eip_address
            }
        }
    });
    let patch = Patch::Apply(&patch);
    let params = PatchParams::apply(crate::FIELD_MANAGER);
    api.patch(name, &params, &patch).await
}
