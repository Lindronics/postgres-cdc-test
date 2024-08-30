use cdc_framework::db::Entity;
pub use cdc_framework::{
    db::{DbClient, DbConfig},
    EventHandler,
};
use client::OutboxClient;
use model::EventRecord;
use subscriber::OutboxSubscriber;

pub mod client;
pub mod handlers;
pub mod model;
pub mod subscriber;

pub async fn new<T>(
    db_config: &DbConfig,
    amqp_connection: &::amqp::Connection,
) -> anyhow::Result<(
    OutboxClient,
    OutboxSubscriber<impl EventHandler<EventRecord>>,
)>
where
    T: model::Message + ::amqp::Publish + Send + Sync + 'static,
{
    let amqp_publisher = handlers::AmqpPublisher::<T>::new(amqp_connection).await?;
    let retry_handler = handlers::EagerRetryHandler::new(db_config, amqp_publisher).await?;

    let sub = OutboxSubscriber::new(&db_config, retry_handler).await?;
    let client = OutboxClient::new(db_config).await?;
    Ok((client, sub))
}

pub async fn setup<const REPLICATION: bool>(client: &DbClient<REPLICATION>) -> anyhow::Result<()> {
    client
        .simple_query(&format!(
            r#"
            CREATE TABLE IF NOT EXISTS {table} (
                id UUID PRIMARY KEY,
                agg_id UUID NOT NULL,
                event_type TEXT NOT NULL,
                data BYTEA NOT NULL,
                ttl smallint NOT NULL,
                created_at TIMESTAMPTZ DEFAULT NOW()
            );
            "#,
            table = model::EventRecord::TABLE
        ))
        .await?;

    Ok(())
}
