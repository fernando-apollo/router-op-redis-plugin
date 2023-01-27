

use anyhow::Result;
use apollo_router::plugin::Plugin;
use apollo_router::plugin::PluginInit;
use apollo_router::register_plugin;

use apollo_router::services::supergraph;
use deadpool_redis::redis::cmd;
use redis::AsyncCommands;
pub use redis::Client;

use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use tower::BoxError;
use tower::ServiceBuilder;
use tower::ServiceExt;

use apollo_router::services::*;

#[derive(Debug)]
struct DiorPlugin {
    #[allow(dead_code)]
    configuration: Conf,
    sender: mpsc::Sender<String>,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
struct Conf {
    message: String,
    redis_scheme: String,
    redis_host: String,
    redis_pass: String,
    channel_size: usize,
}

// This is a bare bones plugin that can be duplicated when creating your own.
#[async_trait::async_trait]
impl Plugin for DiorPlugin {
    type Config = Conf;

    async fn new(init: PluginInit<Self::Config>) -> Result<Self, BoxError> {
        tracing::info!("{}", init.config.message);

        let cfg = deadpool_redis::Config::from_url(format_redis_url(&init.config));
        let (sender, mut receiver) = mpsc::channel::<String>(init.config.channel_size);

        let pool: deadpool_redis::Pool = cfg.create_pool(Some(deadpool_redis::Runtime::Tokio1))?;

        test_connection(&pool).await?;

        // start the receiver thread
        tokio::spawn(async move {
            println!("started receiver thread");

            while let Some(query) = receiver.recv().await {
                let mut conn = pool.get().await.unwrap();

                // spawn a green thread to set the value in redis
                tokio::spawn(async move {
                    let hash = format!("hash:{:X}", Sha256::digest(&query));

                    if let Err(err) = cmd("SET")
                        .arg(&[&hash, &query])
                        .query_async::<_, ()>(&mut conn)
                        .await
                    {
                        tracing::warn!("error setting value: {}", err);
                    }

                    tracing::debug!("SET {} = {}", &hash, &query);
                });
            }
        });

        Ok(DiorPlugin {
            configuration: init.config,
            sender: sender.clone(),
        })
    }

    fn supergraph_service(&self, service: supergraph::BoxService) -> supergraph::BoxService {
        println!("supergraph_service -> {:?}", service);

        let _config_copy = &self.configuration;
        let sender = self.sender.clone();

        ServiceBuilder::new()
            .map_request(move |req: supergraph::Request| {
                let body = req.supergraph_request.body().to_owned();

                let is_introspection_query =
                    &body.operation_name.unwrap_or_default() == "IntrospectionQuery";

                if is_introspection_query {
                    return req;
                }

                let variables = body.variables.to_owned();
                let variables_json = serde_json::to_string(&variables).unwrap();

                if let Some(query) = &body.query {
                    let full_query = format!(
                        "{{ \"query\": \"{}\", \"variables\": {} }}",
                        query.as_str().trim(),
                        &variables_json.as_str()
                    );

                    if !sender.is_closed() {
                        // if the channel is full the message will be dropped with an error
                        if sender.try_send(full_query).is_err() {
                            tracing::warn!("channel is full, dropping message");
                        }
                    }
                }
                req
            })
            .service(service)
            .boxed()
    }

    fn router_service(&self, service: router::BoxService) -> router::BoxService {
        println!("router_service");
        service
    }

    fn execution_service(&self, service: execution::BoxService) -> execution::BoxService {
        println!("execution_service");
        service
    }

    // Unlike other hooks, this hook also passes the name of the subgraph
    // being invoked. That's because this service might invoke *multiple*
    // subgraphs for a single request, and this is called once for each.
    fn subgraph_service(&self, name: &str, service: subgraph::BoxService) -> subgraph::BoxService {
        println!("subgraph_service {}", name);
        service
    }
}

async fn test_connection(
    pool: &deadpool_redis::Pool,
) -> std::result::Result<(), redis::RedisError> {
    let mut connection = pool.get().await.unwrap();
    connection.get("some_key").await
}

fn format_redis_url(config: &Conf) -> String {
    format!(
        "{}://:{}@{}",
        config.redis_scheme, config.redis_pass, config.redis_host
    )
}

// This macro allows us to use it in our plugin registry!
// register_plugin takes a group name, and a plugin name.
register_plugin!("router_plugin", "op_redis_plugin", DiorPlugin);

#[cfg(test)]
mod tests {
    use apollo_router::graphql;
    use apollo_router::services::supergraph;
    use apollo_router::TestHarness;
    use tower::BoxError;
    use tower::ServiceExt;

    #[tokio::test]
    async fn basic_test() -> Result<(), BoxError> {
        let test_harness = TestHarness::builder()
            .configuration_json(serde_json::json!({
                "plugins": {
                    "router_plugin.op_redis_plugin": {
                        "message" : "Starting my plugin"
                    }
                }
            }))
            .unwrap()
            .build_router()
            .await
            .unwrap();
        let request = supergraph::Request::canned_builder().build().unwrap();
        let mut streamed_response = test_harness.oneshot(request.try_into()?).await?;

        let first_response: graphql::Response = serde_json::from_slice(
            streamed_response
                .next_response()
                .await
                .expect("couldn't get primary response")?
                .to_vec()
                .as_slice(),
        )
        .unwrap();

        assert!(first_response.data.is_some());

        println!("first response: {:?}", first_response);
        let next = streamed_response.next_response().await;
        println!("next response: {:?}", next);

        // You could keep calling .next_response() until it yields None if you're expexting more parts.
        assert!(next.is_none());
        Ok(())
    }
}
