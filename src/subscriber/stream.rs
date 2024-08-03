use std::{
    pin::Pin,
    time::{SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use postgres_protocol::message::backend::{LogicalReplicationMessage, ReplicationMessage};
use tokio_postgres::{types::PgLsn, NoTls, SimpleQueryMessage};

use super::message::{MessageHandler, MessageRecord};

pub struct Subscriber<T: MessageHandler> {
    stream: Pin<Box<tokio_postgres::CopyBothDuplex<bytes::Bytes>>>,
    message_handler: T,
}

impl<T: MessageHandler> Subscriber<T> {
    pub async fn new(message_handler: T) -> anyhow::Result<Self> {
        let (client, connection) = tokio_postgres::connect(
            "user=postgres password=password host=localhost port=5432 dbname=postgres replication=database",
            NoTls,
        )
        .await
        .unwrap();
        tokio::spawn(connection);

        let lsn = get_lsn(&client).await?;

        let options = [("proto_version", "1"), ("publication_names", "events_pub")];
        let query = format!(
            "START_REPLICATION SLOT events_slot LOGICAL {lsn} ({});",
            options
                .iter()
                .map(|(k, v)| format!("\"{}\" '{}'", k, v))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let stream = Box::pin(
            client
                .copy_both_simple::<bytes::Bytes>(&query)
                .await
                .unwrap(),
        );
        Ok(Self {
            stream,
            message_handler,
        })
    }

    pub async fn handle_stream(&mut self) -> anyhow::Result<()> {
        while let Some(msg) = self.stream.as_mut().next().await {
            let msg = msg.context("could not get next message in stream")?;

            let ReplicationMessage::XLogData(data) = ReplicationMessage::parse(&msg)? else {
                continue;
            };

            match LogicalReplicationMessage::parse(data.data())? {
                LogicalReplicationMessage::Insert(msg) => {
                    let record = MessageRecord::try_from(msg.tuple())?;
                    self.message_handler.handle(record).await?;
                }
                LogicalReplicationMessage::Commit(msg) => {
                    let ssu = prepare_ssu(PgLsn::from(msg.end_lsn()));
                    self.stream.as_mut().send(ssu).await?;
                    println!("- ACKED")
                }
                _ => {
                    continue;
                }
            };
        }
        Ok(())
    }
}

async fn get_lsn(client: &tokio_postgres::Client) -> anyhow::Result<PgLsn> {
    let result = client
        .simple_query(
            "SELECT confirmed_flush_lsn FROM pg_replication_slots WHERE slot_name = 'events_slot'",
        )
        .await?;

    let row = result
        .into_iter()
        .find_map(|msg| match msg {
            SimpleQueryMessage::Row(row) => Some(row),
            _ => None,
        })
        .context("empty rows")?;

    let lsn = row
        .get("confirmed_flush_lsn")
        .context("missing confirmed_flush_lsn")?
        .to_string()
        .parse()
        .map_err(|_| anyhow::anyhow!("failed to parse LSN"))?;

    Ok(lsn)
}

fn prepare_ssu(write_lsn: PgLsn) -> Bytes {
    const SECONDS_FROM_UNIX_EPOCH_TO_2000: u128 = 946684800;

    let write_lsn_bytes = u64::from(write_lsn).to_be_bytes();
    let time_since_2000: u64 = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_micros()
        - (SECONDS_FROM_UNIX_EPOCH_TO_2000 * 1000 * 1000))
        .try_into()
        .unwrap();

    // see here for format details: https://www.postgresql.org/docs/10/protocol-replication.html
    let mut data_to_send: Vec<u8> = vec![];
    // Byte1('r'); Identifies the message as a receiver status update.
    data_to_send.extend_from_slice(&[114]); // "r" in ascii

    // The location of the last WAL byte + 1 received and written to disk in the standby.
    data_to_send.extend_from_slice(write_lsn_bytes.as_ref());

    // The location of the last WAL byte + 1 flushed to disk in the standby.
    data_to_send.extend_from_slice(write_lsn_bytes.as_ref());

    // The location of the last WAL byte + 1 applied in the standby.
    data_to_send.extend_from_slice(write_lsn_bytes.as_ref());

    // The client's system clock at the time of transmission, as microseconds since midnight on 2000-01-01.
    //0, 0, 0, 0, 0, 0, 0, 0,
    data_to_send.extend_from_slice(&time_since_2000.to_be_bytes());
    // Byte1; If 1, the client requests the server to reply to this message immediately. This can be used to ping the server, to test if the connection is still healthy.
    data_to_send.extend_from_slice(&[1]);

    Bytes::from(data_to_send)
}
