use std::collections::HashMap;
use std::fmt::Debug;
use std::net::AddrParseError;
use std::str::FromStr;
use std::time::Duration;

use aws_sdk_ec2::error::SdkError;
use aws_sdk_ec2::operation::allocate_address::AllocateAddressError;
use aws_sdk_ec2::operation::associate_address::AssociateAddressError;
use aws_sdk_ec2::operation::describe_addresses::DescribeAddressesError;
use aws_sdk_ec2::operation::describe_instances::DescribeInstancesError;
use aws_sdk_ec2::operation::disassociate_address::DisassociateAddressError;
use aws_sdk_ec2::operation::release_address::ReleaseAddressError;
use aws_sdk_servicequotas::error::SdkError as ServiceQuotaSdkError;
use aws_sdk_servicequotas::operation::get_service_quota::GetServiceQuotaError;
use futures::Future;
use hyper_tls::HttpsConnector;
use hyper_util::client::legacy::connect::HttpConnector;
use opentelemetry::trace::TracerProvider;
use opentelemetry::KeyValue;
use opentelemetry_sdk::trace::{Config, Sampler};
use opentelemetry_sdk::Resource as OtelResource;
use tokio::time::error::Elapsed;
use tonic::metadata::{MetadataKey, MetadataMap};
use tonic::transport::Endpoint;
use tracing::{Metadata, Subscriber};
use tracing_subscriber::filter::{EnvFilter, Targets};
use tracing_subscriber::fmt;
use tracing_subscriber::layer::{Context as LayerContext, Filter as LayerFilter, SubscriberExt};
use tracing_subscriber::prelude::*;

pub const MANAGE_EIP_LABEL: &str = "eip.materialize.cloud/manage";

#[derive(Debug, thiserror::Error)]
pub enum Error {
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
    #[error("Kubernetes error: {source}")]
    KubeRuntimeWaitError {
        #[from]
        source: kube_runtime::wait::Error,
    },
    #[error("No EIP found with that podName `{0}`.")]
    NoEipResourceWithThatPodName(String),
    #[error("No EIP found with that node selector.")]
    NoEipResourceWithThatNodeSelector,
    #[error("EIP does not have a status.")]
    MissingEipStatus,
    #[error("EIP does not have a UID in its metadata.")]
    MissingEipUid,
    #[error("Pod does not have an IP address.")]
    MissingPodIp,
    #[error("Node does not have an IP address.")]
    MissingNodeIp,
    #[error("Pod does not have a node name in its spec.")]
    MissingNodeName,
    #[error("Node does not have labels.")]
    MissingNodeLabels,
    #[error("Node does not have a status.")]
    MissingNodeStatus,
    #[error("Node does not have a ready condition.")]
    MissingNodeReadyCondition,
    #[error("Node does not have a provider_id in its spec.")]
    MissingProviderId,
    #[error("Node provider_id is not in expected format.")]
    MalformedProviderId,
    #[error("Multiple elastic IPs are tagged with this pod's UID.")]
    MultipleEipsTaggedForPod,
    #[error("allocation_id was None.")]
    MissingAllocationId,
    #[error("aassociation_id was None.")]
    MissingAssociationId,
    #[error("public_ip was None.")]
    MissingPublicIp,
    #[error("DescribeInstancesResult.reservations was None.")]
    MissingReservations,
    #[error("DescribeInstancesResult.reservations[0].instances was None.")]
    MissingInstances,
    #[error("DescribeInstancesResult.reservations[0].instances[0].network_interfaces was None.")]
    MissingNetworkInterfaces,
    #[error("No interface found with IP matching pod.")]
    MissingAddresses,
    #[error("DescribeAddressesResult.addresses was None.")]
    NoInterfaceWithThatIp,
    #[error("AWS allocate_address reported error: {source}")]
    AllocateAddress {
        #[from]
        source: SdkError<AllocateAddressError>,
    },
    #[error("AWS describe_instances reported error: {source}")]
    AwsDescribeInstances {
        #[from]
        source: SdkError<DescribeInstancesError>,
    },
    #[error("AWS describe_addresses reported error: {source}")]
    AwsDescribeAddresses {
        #[from]
        source: SdkError<DescribeAddressesError>,
    },
    #[error("AWS associate_address reported error: {source}")]
    AwsAssociateAddress {
        #[from]
        source: SdkError<AssociateAddressError>,
    },
    #[error("AWS disassociate_address reported error: {source}")]
    AwsDisassociateAddress {
        #[from]
        source: DisassociateAddressError,
    },
    #[error("AWS release_address reported error: {source}")]
    AwsReleaseAddress {
        #[from]
        source: SdkError<ReleaseAddressError>,
    },
    #[error("AWS get service quota reported error: {source}")]
    AwsGetServiceQuota {
        #[from]
        source: ServiceQuotaSdkError<GetServiceQuotaError>,
    },

    #[error("serde_json error: {source}")]
    SerdeJson {
        #[from]
        source: serde_json::Error,
    },
    #[error("tracing_subscriber error: {source}")]
    TracingSubscriberParse {
        #[from]
        source: tracing_subscriber::filter::ParseError,
    },

    #[error("Tokio Timeout Elapsed: {source}")]
    TokioTimeoutElapsed {
        #[from]
        source: Elapsed,
    },
    #[error("Hyper url error: {source}")]
    HyperUrl {
        #[from]
        source: hyper::http::uri::InvalidUri,
    },
    #[error("Tonic transport error: {source}")]
    TonicTransport {
        #[from]
        source: tonic::transport::Error,
    },
    #[error("Tonic metadata key error: {source}")]
    TonicInvalidMetadataKey {
        #[from]
        source: tonic::metadata::errors::InvalidMetadataKey,
    },
    #[error("Tonic metadata value error: {source}")]
    TonicInvalidMetadataValue {
        #[from]
        source: tonic::metadata::errors::InvalidMetadataValue,
    },
    #[error("AddrParse error: {source}")]
    AddrParse {
        #[from]
        source: AddrParseError,
    },
    #[error("RtNetlink error: {source}")]
    RtNetlink {
        #[from]
        source: rtnetlink::Error,
    },
    #[error("Could not find a rule for that pod installed by Cilium.")]
    CiliumRuleNotFound,
}

struct MyEnvFilter(EnvFilter);

impl<S> LayerFilter<S> for MyEnvFilter
where
    S: Subscriber,
{
    fn enabled(&self, meta: &Metadata<'_>, ctx: &LayerContext<S>) -> bool {
        self.0.enabled(meta, ctx.to_owned())
    }
}

pub async fn run_with_tracing<F, Fut>(service_name: &'static str, f: F) -> Result<(), Error>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<(), Error>>,
{
    match std::env::var("OPENTELEMETRY_ENDPOINT") {
        Ok(otel_endpoint) => {
            let otel_headers: HashMap<String, String> = serde_json::from_str(
                &std::env::var("OPENTELEMETRY_HEADERS").unwrap_or_else(|_| "{}".to_owned()),
            )?;
            // Arbitrary k:v fields to include in all traces, ex: region:us-east-1
            let otel_toplevel_fields: HashMap<String, String> = serde_json::from_str(
                &std::env::var("OPENTELEMETRY_TOPLEVEL_FIELDS").unwrap_or_else(|_| "{}".to_owned()),
            )?;
            let otel_sample_rate =
                &std::env::var("OPENTELEMETRY_SAMPLE_RATE").unwrap_or_else(|_| "0.05".to_owned());
            let otel_targets = std::env::var("OPENTELEMETRY_LEVEL_TARGETS")
                .unwrap_or_else(|_| "DEBUG".to_owned())
                .parse::<Targets>()?;

            // Build endpoint with the correct timeout as exposed here:
            // https://docs.rs/opentelemetry-otlp/latest/opentelemetry_otlp/struct.TonicExporterBuilder.html#method.with_channel
            let endpoint = Endpoint::from_shared(otel_endpoint)?.timeout(Duration::from_secs(
                opentelemetry_otlp::OTEL_EXPORTER_OTLP_TIMEOUT_DEFAULT,
            ));
            // TODO(guswynn): investigate if this should be non-lazy
            let mut http = HttpConnector::new();
            http.enforce_http(false);

            let connector = HttpsConnector::from((
                http,
                tokio_native_tls::TlsConnector::from(
                    native_tls::TlsConnector::builder()
                        .request_alpns(&["h2"])
                        .build()
                        .unwrap(),
                ),
            ));
            let channel = endpoint.connect_with_connector_lazy(connector);

            let mut mmap = MetadataMap::new();
            for (k, v) in otel_headers {
                mmap.insert(MetadataKey::from_str(&k)?, v.parse()?);
            }

            // Add the attributes that all spans should have applied
            let otr = OtelResource::new(
                otel_toplevel_fields
                    .into_iter()
                    .map(|(k, v)| KeyValue::new(k, v))
                    .chain([KeyValue::new("service.name", service_name)]),
            );

            let otlp_exporter = opentelemetry_otlp::new_exporter()
                .tonic()
                .with_channel(channel)
                .with_metadata(mmap);

            let tracer_provider = opentelemetry_otlp::new_pipeline()
                .tracing()
                .with_exporter(otlp_exporter)
                .with_trace_config(
                    Config::default()
                        .with_sampler(Sampler::TraceIdRatioBased(
                            otel_sample_rate.parse().unwrap(),
                        ))
                        .with_resource(otr),
                )
                .install_batch(opentelemetry_sdk::runtime::Tokio)
                .unwrap();
            let tracer = tracer_provider.tracer_builder(service_name).build();
            let otel_layer = tracing_opentelemetry::layer()
                .with_tracer(tracer)
                .with_filter(otel_targets);
            let stdout_layer = fmt::layer()
                .json()
                .with_filter(MyEnvFilter(EnvFilter::from_default_env()));
            tracing_subscriber::Registry::default()
                .with(otel_layer)
                .with(stdout_layer)
                .init();
        }
        Err(_) => {
            tracing_subscriber::fmt()
                .with_env_filter(EnvFilter::from_default_env())
                .json()
                .init();
        }
    };
    f().await
}
