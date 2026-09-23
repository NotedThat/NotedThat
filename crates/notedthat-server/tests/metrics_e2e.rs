//! E2E: the metrics listener exists only when asked for, serves the Prometheus
//! text exposition, and answers nothing else (D68).
//!
//! ```sh
//! cargo test -p notedthat-server --locked --test metrics_e2e
//! ```

#[path = "support/metrics_env.rs"]
mod metrics_env;

use metrics_env::{MetricsServer, series_lines};
use reqwest::StatusCode;
use std::time::Duration;

/// Off by default means two things, and both matter: nothing is bound on the
/// metrics address, and the product listener has no such route — with or
/// without a credential, so its absence cannot be read as "exists but
/// forbidden".
#[tokio::test]
async fn with_metrics_off_nothing_is_bound_and_the_product_listener_has_no_metrics_route() {
    let server = MetricsServer::start_with(metrics_env::in_memory_backends(), false).await;

    let addr = server
        .metrics_base
        .trim_start_matches("http://")
        .to_string();
    let connected = tokio::time::timeout(
        Duration::from_secs(2),
        tokio::net::TcpStream::connect(addr.clone()),
    )
    .await
    .expect("connecting to an unbound port fails promptly");
    assert!(
        connected.is_err(),
        "nothing may listen on {addr} while metrics are off"
    );

    for credential in [Some(metrics_env::LEAK_TOKEN), None] {
        let mut request = server.client.get(format!("{}/metrics", server.base_url));
        if let Some(token) = credential {
            request = request.header("Authorization", format!("Bearer {token}"));
        }
        let response = request.send().await.expect("the product listener answers");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "the product listener must not serve /metrics"
        );
    }
}

#[tokio::test]
async fn the_metrics_listener_serves_the_prometheus_text_exposition() {
    let server = MetricsServer::start().await;
    server.put_text("metrics/probe.md", "# probe").await;

    let response = server
        .client
        .get(server.metrics_url())
        .send()
        .await
        .expect("the metrics listener answers");
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok()),
        Some("text/plain; version=0.0.4; charset=utf-8")
    );

    let body = response.text().await.expect("the exposition reads");
    assert!(
        body.contains("# TYPE "),
        "the exposition declares its metric types:\n{body}"
    );
    let series = series_lines(&body);
    assert!(
        !series.is_empty(),
        "a served exposition has series:\n{body}"
    );
    for line in series {
        assert!(
            line.starts_with("notedthat_"),
            "every series this server exports is ours: {line}"
        );
    }
}

/// The operator socket is not a second product surface.
#[tokio::test]
async fn every_other_path_on_the_metrics_listener_is_404() {
    let server = MetricsServer::start().await;
    for path in [
        "/",
        "/healthz",
        "/readyz",
        "/api/v1/knowledgebases",
        "/metrics/",
    ] {
        let response = server
            .client
            .get(format!("{}{path}", server.metrics_base))
            .send()
            .await
            .expect("the metrics listener answers");
        assert_eq!(
            response.status(),
            StatusCode::NOT_FOUND,
            "{path} must not be served by the metrics listener"
        );
    }
}

/// The recorder is global and installed once; a second server in the same
/// process must still start and serve.
#[tokio::test]
async fn two_servers_in_one_process_both_serve_metrics() {
    let first = MetricsServer::start().await;
    let second = MetricsServer::start().await;
    for server in [&first, &second] {
        assert!(
            server.scrape().await.contains("# TYPE "),
            "both servers serve an exposition"
        );
    }
}

/// The build's identity is on a constant gauge, and the labels are the build's,
/// never the deployment's data.
#[tokio::test]
async fn build_info_names_the_version_and_the_backends() {
    let server = MetricsServer::start().await;
    let body = server.scrape().await;
    let line = body
        .lines()
        .find(|line| line.starts_with("notedthat_build_info"))
        .unwrap_or_else(|| panic!("build info is exported:\n{body}"));
    for expected in [
        concat!("version=\"", env!("CARGO_PKG_VERSION"), "\""),
        "events_backend=\"none\"",
    ] {
        assert!(line.contains(expected), "{line} is missing {expected}");
    }
}
