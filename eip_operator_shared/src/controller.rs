use std::sync::Arc;
use std::time::Duration;

use futures::stream::StreamExt;
use kube::api::{Api, ListParams};
use kube::core::{ClusterResourceScope, NamespaceResourceScope};
use kube::{Client, Resource, ResourceExt};
use kube_runtime::controller::Action;
use kube_runtime::finalizer::{finalizer, Event};
use rand::{thread_rng, Rng};
use tracing::{event, Level};

#[async_trait::async_trait]
pub trait Context {
    type Resource: Resource;
    type Error: std::error::Error;

    const FINALIZER_NAME: &'static str;

    async fn apply(
        &self,
        client: Client,
        api: Api<Self::Resource>,
        resource: &Self::Resource,
    ) -> Result<(), Self::Error>;
    async fn cleanup(
        &self,
        client: Client,
        api: Api<Self::Resource>,
        resource: &Self::Resource,
    ) -> Result<(), Self::Error>;

    async fn reconcile(
        self: Arc<Self>,
        client: Client,
        api: Api<Self::Resource>,
        resource: Arc<Self::Resource>,
    ) -> Result<Action, kube_runtime::finalizer::Error<Self::Error>>
    where
        Self: Send + Sync + 'static,
        Self::Error: Send + Sync + 'static,
        Self::Resource: Send + Sync + 'static,
        Self::Resource: Clone + std::fmt::Debug + serde::Serialize,
        for<'de> Self::Resource: serde::Deserialize<'de>,
        <Self::Resource as Resource>::DynamicType: Eq
            + Clone
            + std::hash::Hash
            + std::default::Default
            + std::fmt::Debug
            + std::marker::Unpin,
    {
        finalizer(
            &api,
            Self::FINALIZER_NAME,
            Arc::clone(&resource),
            |event| async {
                match event {
                    Event::Apply(resource) => self.apply(client, api.clone(), &resource).await?,
                    Event::Cleanup(resource) => {
                        self.cleanup(client, api.clone(), &resource).await?
                    }
                }
                Ok(self.on_success(&resource))
            },
        )
        .await
    }

    fn on_success(&self, _resource: &Self::Resource) -> Action {
        Action::requeue(Duration::from_secs(thread_rng().gen_range(2400..3600)))
    }
    fn on_error(
        self: Arc<Self>,
        _resource: Arc<Self::Resource>,
        _err: &kube_runtime::finalizer::Error<Self::Error>,
    ) -> Action {
        Action::requeue(Duration::from_millis(thread_rng().gen_range(4000..8000)))
    }
}

type MakeApi<Ctx> = Box<
    dyn Fn(&<Ctx as Context>::Resource) -> Api<<Ctx as Context>::Resource> + Sync + Send + 'static,
>;
pub struct Controller<Ctx: Context>
where
    Ctx: Send + Sync + 'static,
    Ctx::Error: Send + Sync + 'static,
    Ctx::Resource: Send + Sync + 'static,
    Ctx::Resource: Clone + std::fmt::Debug + serde::Serialize,
    for<'de> Ctx::Resource: serde::Deserialize<'de>,
    <Ctx::Resource as Resource>::DynamicType:
        Eq + Clone + std::hash::Hash + std::default::Default + std::fmt::Debug + std::marker::Unpin,
{
    client: kube::Client,
    make_api: MakeApi<Ctx>,
    controller: kube_runtime::controller::Controller<Ctx::Resource>,
    context: Ctx,
}

impl<Ctx: Context> Controller<Ctx>
where
    Ctx: Send + Sync + 'static,
    Ctx::Error: Send + Sync + 'static,
    Ctx::Resource: Send + Sync + 'static,
    Ctx::Resource: Clone + std::fmt::Debug + serde::Serialize,
    for<'de> Ctx::Resource: serde::Deserialize<'de>,
    <Ctx::Resource as Resource>::DynamicType:
        Eq + Clone + std::hash::Hash + std::default::Default + std::fmt::Debug + std::marker::Unpin,
{
    pub fn namespaced(namespace: &str, client: Client, lp: ListParams, context: Ctx) -> Self
    where
        Ctx::Resource: Resource<Scope = NamespaceResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |resource: &Ctx::Resource| {
                Api::<Ctx::Resource>::namespaced(client.clone(), &resource.namespace().unwrap())
            })
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::namespaced(client.clone(), namespace),
            lp,
        );
        Self {
            client,
            make_api,
            controller,
            context,
        }
    }

    pub fn namespaced_all(client: Client, lp: ListParams, context: Ctx) -> Self
    where
        Ctx::Resource: Resource<Scope = NamespaceResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |resource: &Ctx::Resource| {
                Api::<Ctx::Resource>::namespaced(client.clone(), &resource.namespace().unwrap())
            })
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::all(client.clone()),
            lp,
        );
        Self {
            client,
            make_api,
            controller,
            context,
        }
    }

    pub fn cluster(client: Client, lp: ListParams, context: Ctx) -> Self
    where
        Ctx::Resource: Resource<Scope = ClusterResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |_: &Ctx::Resource| Api::<Ctx::Resource>::all(client.clone()))
        };
        let controller = kube_runtime::controller::Controller::new(
            Api::<Ctx::Resource>::all(client.clone()),
            lp,
        );
        Self {
            client,
            make_api,
            controller,
            context,
        }
    }

    pub async fn run(self) {
        let Self {
            client,
            make_api,
            controller,
            context,
        } = self;
        controller
            .run(
                |resource, context| {
                    context.reconcile(client.clone(), make_api(&resource), resource)
                },
                |resource, err, context| context.on_error(resource, err),
                Arc::new(context),
            )
            .for_each(|reconciliation_result| async move {
                let dynamic_type = Default::default();
                let kind = Ctx::Resource::kind(&dynamic_type);
                match reconciliation_result {
                    Ok(resource) => {
                        event!(
                            Level::INFO,
                            resource_name = %resource.0.name,
                            "{} reconciliation successful.",
                            kind
                        );
                    }
                    Err(err) => event!(
                        Level::ERROR,
                        err = %err,
                        "{} reconciliation error.",
                        kind
                    ),
                }
            })
            .await
    }
}
