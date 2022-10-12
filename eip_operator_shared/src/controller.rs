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
        res: &Self::Resource,
    ) -> Result<(), Self::Error>;
    async fn cleanup(
        &self,
        client: Client,
        api: Api<Self::Resource>,
        res: &Self::Resource,
    ) -> Result<(), Self::Error>;

    async fn reconcile(
        self: Arc<Self>,
        client: Client,
        api: Api<Self::Resource>,
        res: Arc<Self::Resource>,
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
            Arc::clone(&res),
            |event| async {
                match event {
                    Event::Apply(res) => self.apply(client, api.clone(), &res).await?,
                    Event::Cleanup(res) => self.cleanup(client, api.clone(), &res).await?,
                }
                Ok(self.on_success(&res))
            },
        )
        .await
    }

    fn on_success(&self, _res: &Self::Resource) -> Action {
        Action::requeue(Duration::from_secs(thread_rng().gen_range(2400..3600)))
    }
    fn on_error(
        self: Arc<Self>,
        _res: Arc<Self::Resource>,
        _err: &kube_runtime::finalizer::Error<Self::Error>,
    ) -> Action {
        Action::requeue(Duration::from_millis(thread_rng().gen_range(4000..8000)))
    }
}

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
    make_api: Box<dyn Fn(&Ctx::Resource) -> Api<Ctx::Resource> + Sync + Send + 'static>,
    controller: kube_runtime::controller::Controller<Ctx::Resource>,
    ctx: Ctx,
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
    pub fn namespaced(namespace: &str, client: Client, lp: ListParams, ctx: Ctx) -> Self
    where
        Ctx::Resource: Resource<Scope = NamespaceResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |res: &Ctx::Resource| {
                Api::<Ctx::Resource>::namespaced(client.clone(), &res.namespace().unwrap())
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
            ctx,
        }
    }

    pub fn namespaced_all(client: Client, lp: ListParams, ctx: Ctx) -> Self
    where
        Ctx::Resource: Resource<Scope = NamespaceResourceScope>,
    {
        let make_api = {
            let client = client.clone();
            Box::new(move |res: &Ctx::Resource| {
                Api::<Ctx::Resource>::namespaced(client.clone(), &res.namespace().unwrap())
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
            ctx,
        }
    }

    pub fn cluster(client: Client, lp: ListParams, ctx: Ctx) -> Self
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
            ctx,
        }
    }

    pub async fn run(self) {
        let Self {
            client,
            make_api,
            controller,
            ctx,
        } = self;
        controller
            .run(
                |res, ctx| ctx.reconcile(client.clone(), make_api(&res), res),
                |res, err, ctx| ctx.on_error(res, err),
                Arc::new(ctx),
            )
            .for_each(|reconciliation_result| async move {
                let dt = Default::default();
                let kind = Ctx::Resource::kind(&dt).to_owned();
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
