//! A worker on the `NotedThat` event stream, read straight from NATS (`docs/NATS.md`).
//!
//! Run as many copies as you like: they share one durable pull consumer, so
//! `JetStream` hands each event to one of them and redelivers any event a copy
//! took but never acknowledged — the thing an SSE subscriber cannot do.
//!
//! ```sh
//! docker compose -f docker-compose.yml -f docker-compose.events.yml up --build -d
//! NATS_URL=nats://127.0.0.1:4222 FILTER='notedthat.events.notes.written' \
//!     cargo run -p notedthat-events --example nats_consumer
//! ```
//!
//! Settings: `NATS_URL`, `STREAM` (default `notedthat-events`), `CONSUMER`
//! (default `example-worker`) and `FILTER` (default `notedthat.events.>`).
//!
//! Delivery is at least once, so `handle` must be idempotent — here it only
//! prints. A real worker compares the event's `etag` with what it last
//! processed, or checks for the output it would produce, before doing work.

use std::time::Duration;

use async_nats::jetstream::{self, AckKind, consumer::pull};
use futures::StreamExt;
use notedthat_core::{ObjectEvent, ObjectEventKind};

fn env(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Do the work for one event. An `Err` asks for redelivery.
#[allow(clippy::unnecessary_wraps)] // A real handler fails; the stub keeps its shape.
fn handle(event: &ObjectEvent) -> Result<(), String> {
    match &event.kind {
        ObjectEventKind::Written { etag, mime, .. } => {
            println!("written  {}/{}  {mime}  {etag}", event.kb, event.object_key);
        }
        ObjectEventKind::Deleted => println!("deleted  {}/{}", event.kb, event.object_key),
        ObjectEventKind::Indexed { chunks, .. } => {
            println!(
                "indexed  {}/{}  {chunks} chunks",
                event.kb, event.object_key
            );
        }
        ObjectEventKind::IndexFailed { summary, .. } => println!(
            "failed   {}/{}  {}",
            event.kb,
            event.object_key,
            summary.as_deref().unwrap_or("-")
        ),
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), async_nats::Error> {
    let url = env("NATS_URL", "nats://127.0.0.1:4222");
    let stream_name = env("STREAM", "notedthat-events");
    let consumer_name = env("CONSUMER", "example-worker");
    let filter = env("FILTER", "notedthat.events.>");

    let client = async_nats::connect(&url).await?;
    let js = jetstream::new(client);
    // The stream is NotedThat's; only the consumer is ours. In production an
    // administrator creates it and the worker's NATS user may only read from it.
    let stream = js.get_stream(&stream_name).await?;
    let consumer: jetstream::consumer::PullConsumer = stream
        .get_or_create_consumer(
            &consumer_name,
            pull::Config {
                durable_name: Some(consumer_name.clone()),
                filter_subject: filter.clone(),
                ack_policy: jetstream::consumer::AckPolicy::Explicit,
                ack_wait: Duration::from_secs(60),
                max_deliver: 5,
                ..pull::Config::default()
            },
        )
        .await?;
    println!("consuming {filter} from {stream_name} as {consumer_name}");

    let mut messages = consumer.messages().await?;
    while let Some(message) = messages.next().await {
        let message = message?;
        let schema = message
            .headers
            .as_ref()
            .and_then(|headers| headers.get("NotedThat-Schema"))
            .map(|value| value.as_str().to_string());
        // Ignore what this worker was not written for, rather than guess at it.
        if schema.as_deref() != Some("object-event/1") {
            eprintln!("skip: schema {schema:?} on {}", message.subject);
            message.ack().await?;
            continue;
        }
        match serde_json::from_slice::<ObjectEvent>(&message.payload) {
            Ok(event) => match handle(&event) {
                Ok(()) => message.ack().await?,
                Err(error) => {
                    eprintln!("retry later: {error}");
                    message
                        .ack_with(AckKind::Nak(Some(Duration::from_secs(5))))
                        .await?;
                }
            },
            // A payload this version cannot read will not get better on redelivery.
            Err(error) => {
                eprintln!("undecodable event on {}: {error}", message.subject);
                message.ack_with(AckKind::Term).await?;
            }
        }
    }
    Ok(())
}
