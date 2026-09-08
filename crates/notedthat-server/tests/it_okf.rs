//! OKF storage, indexing, and MCP behavior through the real server.

#[path = "support/patch_env.rs"]
mod patch_env;

use patch_env::{API_TOKEN, PatchServer, mcp_call_tool, mcp_request};
use reqwest::StatusCode;
use serde_json::{Value, json};
use std::time::Duration;

const PATH: &str = "metrics%2Frevenue.md";
const BODY: &str = "# Revenue\nRevenue measures café sales in €.\n";
const FRONTMATTER: &str = "---\ntype: Metric\ntitle: Revenue\ndescription: Café revenue\nresource: bigquery://project.dataset.sales\ntags: [finance, approved]\ncustom:\n  owner: analyst\n---\n";

async fn search(server: &PatchServer, filter: Value) -> Vec<Value> {
    let response = server
        .client
        .post(format!(
            "{}/api/v1/knowledgebases/{}/search",
            server.base_url, server.kb
        ))
        .bearer_auth(API_TOKEN)
        .json(&json!({"query": "revenue", "filter": filter, "limit": 50}))
        .send()
        .await
        .expect("search should return");
    assert_eq!(response.status(), StatusCode::OK);
    response.json::<Value>().await.expect("search JSON")["hits"]
        .as_array()
        .expect("search hits array")
        .clone()
}

async fn wait_for_hits(
    server: &PatchServer,
    filter: Value,
    ready: impl Fn(&[Value]) -> bool,
) -> Vec<Value> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let hits = search(server, filter.clone()).await;
        if ready(&hits) {
            return hits;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "index not ready: {hits:?}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test]
async fn okf_upload_preserves_source_and_exposes_metadata_through_http_and_mcp() {
    // Given: an OKF concept with UTF-8 content, unknown metadata, and a nested path.
    let server = PatchServer::start(1_048_576).await;
    let source = format!("{FRONTMATTER}{BODY}");

    // When: the original Markdown is uploaded and asynchronously indexed.
    server.put_text(PATH, &source).await;
    let hits = wait_for_hits(&server, json!({}), |hits| !hits.is_empty()).await;

    // Then: storage preserves all bytes, while search describes only the body.
    assert_eq!(server.get_text(PATH).await, source);
    assert_eq!(
        hits.len(),
        1,
        "frontmatter must not become a separate chunk"
    );
    let hit = &hits[0];
    assert_eq!(hit["object_key"], "metrics/revenue.md");
    let metadata = json!({
        "concept_id": "metrics/revenue", "type": "Metric", "title": "Revenue",
        "description": "Café revenue", "resource": "bigquery://project.dataset.sales",
        "tags": ["finance", "approved"]
    });
    assert_eq!(hit["okf"], metadata);
    let start = usize::try_from(hit["byte_start"].as_u64().expect("start offset")).expect("usize");
    let end = usize::try_from(hit["byte_end"].as_u64().expect("end offset")).expect("usize");
    assert!(start >= FRONTMATTER.len());
    assert_eq!(&source[start..end], BODY);
    assert!(
        !hit["preview"]
            .as_str()
            .expect("preview")
            .contains("custom:")
    );
    let range = server
        .client
        .get(server.object_url(PATH))
        .bearer_auth(API_TOKEN)
        .header("Range", format!("bytes={start}-{}", end - 1))
        .send()
        .await
        .expect("range GET");
    assert_eq!(range.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(range.text().await.expect("range body"), &source[start..end]);

    let filter =
        json!({"concept_type": "Metric", "tags": ["finance", "approved"], "mime": "text/markdown"});
    assert_eq!(search(&server, filter.clone()).await.len(), 1);
    for excluded in [
        json!({"concept_type": "Rule", "tags": ["finance"]}),
        json!({"concept_type": "Metric", "tags": ["missing"]}),
        json!({"concept_type": "Metric", "mime": "text/plain"}),
    ] {
        assert!(search(&server, excluded).await.is_empty());
    }

    let initialized = mcp_request(
        &server.client,
        &server.mcp_url,
        0,
        "initialize",
        json!({
            "protocolVersion": "2024-11-05", "capabilities": {},
            "clientInfo": {"name": "notedthat-okf-e2e", "version": "0"}
        }),
    )
    .await;
    assert!(initialized.get("result").is_some());
    for (id, filters, expected) in [(1, filter, 1), (2, json!({"concept_type": "Rule"}), 0)] {
        let result = mcp_call_tool(
            &server.client,
            &server.mcp_url,
            id,
            "search",
            json!({
                "kb": server.kb, "query": "revenue", "filters": filters
            }),
        )
        .await;
        let text = result["result"]["content"][0]["text"]
            .as_str()
            .expect("MCP search content");
        let result: Value = serde_json::from_str(text).expect("MCP search JSON");
        let hits = result["hits"].as_array().expect("MCP hits");
        assert_eq!(hits.len(), expected);
        if let Some(hit) = hits.first() {
            assert_eq!(hit["okf"], metadata);
        }
    }
    server
        .put_text("plain.md", "# Revenue\nPlain revenue notes.\n")
        .await;
    let plain = wait_for_hits(&server, json!({"object_key_prefix": "plain.md"}), |hits| {
        !hits.is_empty()
    })
    .await;
    assert_eq!(plain.len(), 1);
    assert!(plain[0].get("okf").is_none_or(Value::is_null));
    assert_eq!(plain[0]["byte_start"], 0);
}

#[tokio::test]
async fn replacing_okf_with_fewer_chunks_and_metadata_only_removes_stale_search_hits() {
    // Given: an indexed OKF concept spanning several headings.
    let server = PatchServer::start(1_048_576).await;
    server.put_text(PATH, &format!("{FRONTMATTER}# First\nRevenue one.\n# Second\nRevenue two.\n# Third\nRevenue three.\n")).await;
    let filter = json!({"object_key_prefix": "metrics/revenue.md"});
    wait_for_hits(&server, filter.clone(), |hits| hits.len() == 3).await;

    // When: the concept becomes one chunk, then metadata without a body.
    let changed = FRONTMATTER.replace("title: Revenue", "title: Updated");
    server.put_text(PATH, &format!("{changed}{BODY}")).await;
    let hits = wait_for_hits(&server, filter.clone(), |hits| {
        hits.len() == 1 && hits[0]["okf"]["title"] == "Updated"
    })
    .await;
    assert_eq!(hits.len(), 1, "old heading chunks must be removed");
    server.put_text(PATH, &changed).await;
    server
        .put_text("barrier.md", "# Revenue\nIndexing barrier.\n")
        .await;
    wait_for_hits(
        &server,
        json!({"object_key_prefix": "barrier.md"}),
        |hits| !hits.is_empty(),
    )
    .await;

    // Then: processing a later event proves metadata-only indexing finished and removed all hits.
    assert!(search(&server, filter).await.is_empty());
    assert_eq!(server.get_text(PATH).await, changed);
}
