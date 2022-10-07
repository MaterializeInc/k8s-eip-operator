use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Node, Pod};

pub(crate) trait NodeExt {
    fn ip(&self) -> Option<&str>;
    fn labels(&self) -> Option<&BTreeMap<String, String>>;
    fn provider_id(&self) -> Option<&str>;
}

impl NodeExt for Node {
    fn ip(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|status| status.addresses.as_ref())
            .and_then(|addresses| {
                addresses
                    .iter()
                    .find(|addr| addr.type_ == "InternalIP")
                    .map(|addr| addr.address.as_str())
            })
    }

    fn labels(&self) -> Option<&BTreeMap<String, String>> {
        self.metadata.labels.as_ref()
    }

    fn provider_id(&self) -> Option<&str> {
        self.spec
            .as_ref()
            .and_then(|spec| spec.provider_id.as_deref())
    }
}

pub(crate) trait PodExt {
    fn ip(&self) -> Option<&str>;
    fn node_name(&self) -> Option<&str>;
}

impl PodExt for Pod {
    fn ip(&self) -> Option<&str> {
        self.status
            .as_ref()
            .and_then(|status| status.pod_ip.as_deref())
    }

    fn node_name(&self) -> Option<&str> {
        self.spec
            .as_ref()
            .and_then(|spec| spec.node_name.as_deref())
    }
}
