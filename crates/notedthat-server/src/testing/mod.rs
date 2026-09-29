//! In-process servers and stream readers for the E2E suites, here rather than
//! in `tests/support` so every suite links the same library code instead of
//! compiling its own copy of a harness it only partly uses (#304).

use std::time::Duration;

pub mod backends;
pub mod metrics_server;
/// A server for the write-path suites: REST writes, patches, replaces, MCP.
pub mod patch_server;
pub mod sse;

/// How long to wait for the server to bind after startup begins.
///
/// Provisioning is in-process, so this is generous by a wide margin; it
/// exists to fail with a clear message rather than hang if startup breaks.
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(30);

async fn wait_for_http(url: &str, timeout: Duration) {
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Ok(response) = client.get(url).send().await
            && response.status().is_success()
        {
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "server did not become ready at {url} within {timeout:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
