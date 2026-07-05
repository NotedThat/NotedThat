use super::*;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path, query_param, query_param_is_missing},
};

fn client(url: &str) -> NotedThatClient {
    NotedThatClient::new(url, "test-token").unwrap()
}

async fn mount_kbs(server: &MockServer, kbs: &[&str]) {
    Mock::given(method("GET"))
        .and(path("/v1/knowledgebases"))
        .and(header("authorization", "Bearer test-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"knowledgebases": kbs})),
        )
        .mount(server)
        .await;
}

async fn mount_list_page(
    server: &MockServer,
    kb: &str,
    cursor: Option<&str>,
    keys: &[String],
    next_cursor: Option<&str>,
) {
    let objects: Vec<serde_json::Value> = keys
        .iter()
        .map(|key| serde_json::json!({"key": key, "size": 0}))
        .collect();
    let response = ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "objects": objects,
        "next_cursor": next_cursor,
    }));

    let mock = Mock::given(method("GET")).and(path(format!("/v1/knowledgebases/{kb}")));
    let mock = match cursor {
        Some(value) => mock.and(query_param("cursor", value)),
        None => mock.and(query_param_is_missing("cursor")),
    };
    mock.respond_with(response).mount(server).await;
}

fn resource_names(resources: &[Resource]) -> Vec<String> {
    resources
        .iter()
        .map(|resource| resource.name.clone())
        .collect()
}

async fn collect_all(client: &NotedThatClient) -> Vec<Resource> {
    let mut cursor = None;
    let mut resources = Vec::new();
    loop {
        let page = list_resources(client, cursor).await.unwrap();
        resources.extend(page.resources);
        cursor = page.next_cursor;
        if cursor.is_none() {
            break;
        }
    }
    resources
}

#[tokio::test]
async fn first_page_returns_cursor_for_backend_next_page() {
    let server = MockServer::start().await;
    mount_kbs(&server, &["notes"]).await;
    mount_list_page(
        &server,
        "notes",
        None,
        &["docs/one.md".to_string(), "space note.md".to_string()],
        Some("backend-2"),
    )
    .await;
    mount_list_page(
        &server,
        "notes",
        Some("backend-2"),
        &["docs/two.md".to_string()],
        None,
    )
    .await;
    let c = client(&server.uri());

    let first = list_resources(&c, None).await.unwrap();
    let second = list_resources(&c, first.next_cursor.clone()).await.unwrap();

    assert_eq!(
        resource_names(&first.resources),
        ["docs/one.md", "space note.md"]
    );
    assert_eq!(resource_names(&second.resources), ["docs/two.md"]);
    assert!(first.next_cursor.is_some());
    assert!(second.next_cursor.is_none());
    assert_eq!(
        decode_cursor(first.next_cursor.as_deref().unwrap())
            .unwrap()
            .backend_cursor,
        Some("backend-2".to_string())
    );
    assert_eq!(first.resources[0].uri, "notedthat://notes/docs%2Fone.md");
    assert_eq!(first.resources[1].uri, "notedthat://notes/space%20note.md");
    assert_eq!(first.resources[0].title, None);
    assert_eq!(first.resources[0].description, None);
    assert_eq!(first.resources[0].annotations, None);
    assert_eq!(first.resources[0].meta, None);
}

#[tokio::test]
async fn cursor_advances_across_kb_boundary() {
    let server = MockServer::start().await;
    mount_kbs(&server, &["notes", "scratch"]).await;
    mount_list_page(&server, "notes", None, &["a.md".to_string()], None).await;
    mount_list_page(&server, "scratch", None, &["b.md".to_string()], None).await;
    let c = client(&server.uri());

    let first = list_resources(&c, None).await.unwrap();
    let second = list_resources(&c, first.next_cursor.clone()).await.unwrap();

    assert_eq!(resource_names(&first.resources), ["a.md"]);
    assert_eq!(resource_names(&second.resources), ["b.md"]);
    assert!(first.next_cursor.is_some());
    assert!(second.next_cursor.is_none());
}

#[tokio::test]
async fn collecting_until_no_cursor_returns_all_resources() {
    let server = MockServer::start().await;
    mount_kbs(&server, &["notes", "scratch"]).await;
    mount_list_page(&server, "notes", None, &["a.md".to_string()], Some("n2")).await;
    mount_list_page(&server, "notes", Some("n2"), &["b.md".to_string()], None).await;
    mount_list_page(&server, "scratch", None, &["c.md".to_string()], None).await;
    let c = client(&server.uri());

    let resources = collect_all(&c).await;

    assert_eq!(resource_names(&resources), ["a.md", "b.md", "c.md"]);
}

#[tokio::test]
async fn collecting_three_kbs_of_fifty_has_no_drops_or_duplicates() {
    let server = MockServer::start().await;
    mount_kbs(&server, &["kb-a", "kb-b", "kb-c"]).await;
    let mut expected = Vec::new();
    for kb in ["kb-a", "kb-b", "kb-c"] {
        let first: Vec<String> = (0..25)
            .map(|index| format!("{kb}/object-{index:02}.md"))
            .collect();
        let second: Vec<String> = (25..50)
            .map(|index| format!("{kb}/object-{index:02}.md"))
            .collect();
        expected.extend(first.iter().cloned());
        expected.extend(second.iter().cloned());
        mount_list_page(&server, kb, None, &first, Some("page-2")).await;
        mount_list_page(&server, kb, Some("page-2"), &second, None).await;
    }
    let c = client(&server.uri());

    let resources = collect_all(&c).await;

    let actual = resource_names(&resources);
    assert_eq!(actual.len(), 150);
    assert_eq!(
        actual
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        150
    );
    assert_eq!(actual, expected);
}
