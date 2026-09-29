#![allow(missing_docs)]

//! Authentication against a real broker: the configured method is what the
//! connection presents, and a broker that wants one refuses a connection without it.

use notedthat_nats::{NatsAuth, NatsConnectConfig, NatsConnectError, connect};
use testcontainers::{
    ContainerAsync, GenericImage, ImageExt,
    core::{IntoContainerPort, WaitFor},
    runners::AsyncRunner,
};

const TOKEN: &str = "integration-token";

async fn token_broker() -> (ContainerAsync<GenericImage>, String) {
    let container = GenericImage::new("nats", "2.12-alpine")
        .with_exposed_port(4222_u16.tcp())
        .with_wait_for(WaitFor::message_on_stderr("Server is ready"))
        .with_cmd(["-js", "--auth", TOKEN])
        .start()
        .await
        .expect("start NATS container");
    let port = container
        .get_host_port_ipv4(4222_u16)
        .await
        .expect("NATS mapped port");
    (container, format!("nats://127.0.0.1:{port}"))
}

#[tokio::test]
#[ignore = "requires a NATS testcontainer"]
async fn a_token_is_presented_and_its_absence_is_refused() {
    let (_broker, url) = token_broker().await;

    let mut config = NatsConnectConfig::plain(url.clone());
    config.auth = NatsAuth::Token(TOKEN.to_string());
    let client = connect(&config, "notedthat-test")
        .await
        .expect("the configured token is accepted");
    assert_eq!(
        client.connection_state(),
        async_nats::connection::State::Connected
    );

    let refused = connect(&NatsConnectConfig::plain(url), "notedthat-test").await;
    assert!(
        matches!(refused, Err(NatsConnectError::Connect(_))),
        "a broker that wants a token refuses a connection without one"
    );
}

#[tokio::test]
async fn an_unreadable_seed_file_names_its_setting() {
    let mut config = NatsConnectConfig::plain("nats://127.0.0.1:1");
    config.auth = NatsAuth::NkeySeedFile("/nonexistent/notedthat.nk".into());
    let Err(error) = connect(&config, "notedthat-test").await else {
        panic!("a missing seed file must fail before connecting");
    };
    let message = error.to_string();
    assert!(
        message.contains("NOTEDTHAT_NATS_NKEY_SEED_FILE (--nats-nkey-seed-file)"),
        "{message}"
    );
}
