//! `search`: hybrid search over one or more knowledge bases.
//!
//! The HTTP route is single-slug by construction, so a request naming several
//! knowledge bases fans out to one `POST /api/v1/knowledgebases/{kb}/search`
//! per slug, concurrently, and the answers are returned grouped per knowledge
//! base in request order. No cross-KB ranking is published: a hit's `score` is
//! Qdrant's reciprocal-rank fusion value, a function of the hit's position in
//! its own collection, so every knowledge base's top hit scores the same
//! whatever its relevance and a merged sort would be an arbitrary interleave.

use crate::client::NotedThatClient;
use crate::error::{McpToolError, map_response};
use crate::path::encode_kb_slug;
use futures::StreamExt;
use notedthat_core::search::{SearchHit, SearchResponse};
use rmcp::{
    ErrorData as McpError,
    model::{CallToolResult, ContentBlock},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// Searches in flight at once. Every knowledge base costs a Qdrant query and
/// an embedding call on the server, so a wide fan-out is paced rather than
/// fired all at once; request order is kept regardless.
const CONCURRENCY: usize = 8;

/// The tool's arguments. Unknown keys are refused, as the API refuses them
/// (#125): `filter` for `filters`, or a misspelt filter field, is an
/// `invalid_request` naming the key rather than a search that quietly ran
/// without it.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchArgs {
    /// Knowledge bases to search, by slug (discover them with
    /// `list_knowledgebases`). Omit, or pass `[]`, to search every knowledge
    /// base the caller may search.
    #[serde(default)]
    pub kb: Vec<String>,
    /// The natural-language query, 1–8192 bytes.
    pub query: String,
    /// Optional filters, AND-composed; the same fields the HTTP search body's
    /// `filter` takes.
    pub filters: Option<SearchFilter>,
    /// Maximum hits per knowledge base, not across the request. Server
    /// default 10, maximum 50.
    pub limit: Option<u32>,
}

/// The HTTP search body's `filter`, field for field, with the descriptions an
/// agent reads from the tool schema. Kept as its own type rather than
/// re-exporting `notedthat_core::search::SearchFilter` because core does not
/// depend on `schemars`; `schema_matches_the_api_filter` below keeps the two
/// in step.
#[derive(Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchFilter {
    /// Only hits whose object key starts with this prefix, e.g. `docs/rfc/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub object_key_prefix: Option<String>,
    /// Only hits from objects of exactly this MIME type, e.g. `text/markdown`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
    /// Only Open Knowledge Format concept hits of exactly this type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub concept_type: Option<String>,
    /// Only hits whose heading path starts with these segments, in order,
    /// e.g. `["Installation"]` for every chunk under that top heading.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub heading_path_prefix: Vec<String>,
    /// Only hits from objects modified at or after this Unix time (seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_after: Option<i64>,
    /// Only hits from objects modified at or before this Unix time (seconds).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_before: Option<i64>,
    /// Only Open Knowledge Format concept hits carrying at least one of these
    /// tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// The body sent to every knowledge base: one request, serialised per slug.
#[derive(Debug, Serialize)]
struct SearchBody<'a> {
    query: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    filter: Option<&'a SearchFilter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    limit: Option<u32>,
}

/// The tool's answer: one group per knowledge base searched, in request
/// order, each keeping its own ranking.
#[derive(Debug, Serialize)]
struct SearchOutput {
    results: Vec<KbResults>,
    /// Knowledge bases the caller may see but not search, dropped from an
    /// implicit (omitted `kb`) fan-out. Always empty for an explicit list,
    /// which fails instead.
    skipped: Vec<String>,
}

#[derive(Debug, Serialize)]
struct KbResults {
    kb: String,
    hits: Vec<KbHit>,
}

/// A hit that names its knowledge base, so it stays unambiguous once a client
/// flattens the groups.
#[derive(Debug, Serialize)]
struct KbHit {
    kb: String,
    #[serde(flatten)]
    hit: SearchHit,
}

/// Which knowledge bases a call named, and so how a refusal is handled.
enum Selection {
    /// The caller listed them: a refusal anywhere fails the call.
    Explicit(Vec<String>),
    /// Discovered from `GET /api/v1/knowledgebases`, which shows every
    /// knowledge base the caller holds any grant in — not necessarily
    /// `search` — so a refusal drops that one rather than failing the call.
    All(Vec<String>),
}

impl Selection {
    fn slugs(&self) -> &[String] {
        match self {
            Self::Explicit(slugs) | Self::All(slugs) => slugs,
        }
    }
}

pub(super) async fn run(
    client: &NotedThatClient,
    args: SearchArgs,
) -> Result<CallToolResult, McpError> {
    let selection = select(client, args.kb).await?;
    let body = SearchBody {
        query: &args.query,
        filter: args.filters.as_ref(),
        limit: args.limit,
    };

    // Built up front (a future does nothing until polled) so the stream holds
    // plain futures rather than a closure over `body`'s borrow.
    let searches: Vec<_> = selection
        .slugs()
        .iter()
        .map(|slug| search_one(client, slug, &body))
        .collect();
    let answers: Vec<_> = futures::stream::iter(searches)
        .buffered(CONCURRENCY)
        .collect()
        .await;

    let mut results = Vec::with_capacity(answers.len());
    let mut skipped = Vec::new();
    for (slug, answer) in selection.slugs().iter().zip(answers) {
        match (answer, &selection) {
            (Ok(resp), _) => results.push(KbResults {
                kb: slug.clone(),
                hits: resp
                    .hits
                    .into_iter()
                    .map(|hit| KbHit {
                        kb: slug.clone(),
                        hit,
                    })
                    .collect(),
            }),
            (Err(McpToolError::Forbidden | McpToolError::NotFound(_)), Selection::All(_)) => {
                skipped.push(slug.clone());
            }
            (Err(error), _) => return Err(kb_error(slug, error)),
        }
    }

    Ok(CallToolResult::success(vec![ContentBlock::json(
        SearchOutput { results, skipped },
    )?]))
}

/// Resolve the `kb` argument: the caller's list, or every knowledge base the
/// caller can see when the list is empty. Duplicates are refused rather than
/// deduplicated so the answer has exactly one group per requested slug.
async fn select(client: &NotedThatClient, kb: Vec<String>) -> Result<Selection, McpError> {
    if kb.is_empty() {
        return Ok(Selection::All(client.list_kbs().await?));
    }
    let mut seen = HashSet::with_capacity(kb.len());
    if let Some(duplicate) = kb.iter().find(|slug| !seen.insert(slug.as_str())) {
        return Err(McpToolError::InvalidRequest(format!(
            "kb lists knowledge base {duplicate:?} more than once"
        ))
        .into());
    }
    Ok(Selection::Explicit(kb))
}

/// One knowledge base's search: the single-slug HTTP route as it stands.
async fn search_one(
    client: &NotedThatClient,
    slug: &str,
    body: &SearchBody<'_>,
) -> Result<SearchResponse, McpToolError> {
    let kb_enc = encode_kb_slug(slug);
    let url = client.api_v1_url(&["knowledgebases", &kb_enc, "search"]);
    let resp = client
        .authorized(client.http.post(url).json(body))
        .send()
        .await?;
    let resp = map_response(resp).await?;
    Ok(resp.json().await?)
}

/// The tool error for one knowledge base's failure, naming the knowledge base
/// after the mapped message so the code string stays where D43 puts it.
fn kb_error(slug: &str, error: McpToolError) -> McpError {
    let mut data = McpError::from(error);
    data.message = format!("{} (knowledge base {slug:?})", data.message).into();
    data
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::NotedThatClient;
    use rmcp::model::ErrorCode;
    use std::time::Duration;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_partial_json, body_string_contains, header, method, path},
    };

    fn client(url: &str) -> NotedThatClient {
        NotedThatClient::new(url, "tok").unwrap()
    }

    fn args(kb: &[&str], query: &str, limit: Option<u32>) -> SearchArgs {
        SearchArgs {
            kb: kb.iter().map(|s| (*s).to_string()).collect(),
            query: query.into(),
            filters: None,
            limit,
        }
    }

    fn hit(object_key: &str, score: f32) -> serde_json::Value {
        serde_json::json!({
            "object_key": object_key, "byte_start": 0, "byte_end": 10,
            "score": score, "preview": "hi"
        })
    }

    /// A search mock for `kb` answering `status` with `hits`.
    fn search_mock(kb: &str, status: u16, hits: &serde_json::Value) -> Mock {
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/knowledgebases/{kb}/search")))
            .respond_with(
                ResponseTemplate::new(status).set_body_json(serde_json::json!({"hits": hits})),
            )
    }

    fn error_mock(kb: &str, status: u16, code: &str) -> Mock {
        Mock::given(method("POST"))
            .and(path(format!("/api/v1/knowledgebases/{kb}/search")))
            .respond_with(ResponseTemplate::new(status).set_body_json(
                serde_json::json!({"error": code, "message": format!("{code} for {kb}")}),
            ))
    }

    fn kbs_mock(slugs: &[&str]) -> Mock {
        Mock::given(method("GET"))
            .and(path("/api/v1/knowledgebases"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "knowledgebases": slugs
            })))
    }

    /// The JSON the tool answered with.
    fn output(result: CallToolResult) -> serde_json::Value {
        let content = serde_json::to_value(result).unwrap();
        let text = content["content"][0]["text"].as_str().unwrap();
        serde_json::from_str(text).unwrap()
    }

    fn kb_order(output: &serde_json::Value) -> Vec<&str> {
        output["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|group| group["kb"].as_str().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn two_knowledge_bases_answer_as_two_groups_in_request_order() {
        // Given: two knowledge bases, each ranking its own hits
        let server = MockServer::start().await;
        search_mock(
            "whatwg",
            200,
            &serde_json::json!([hit("dom.md", 0.5), hit("html.md", 0.33)]),
        )
        .expect(1)
        .mount(&server)
        .await;
        search_mock("odf", 200, &serde_json::json!([hit("packages.md", 0.5)]))
            .expect(1)
            .mount(&server)
            .await;

        // When: both are searched in one call, odf named first
        let result = run(
            &client(&server.uri()),
            args(&["odf", "whatwg"], "document", None),
        )
        .await
        .unwrap();
        let out = output(result);

        // Then: one group per slug in request order, each keeping its own
        // ranking, every hit naming its knowledge base, nothing skipped
        assert_eq!(kb_order(&out), ["odf", "whatwg"]);
        let whatwg = &out["results"][1]["hits"];
        assert_eq!(whatwg[0]["object_key"], "dom.md");
        assert_eq!(whatwg[1]["object_key"], "html.md");
        assert_eq!(whatwg[0]["kb"], "whatwg");
        assert_eq!(whatwg[0]["score"], 0.5);
        assert_eq!(out["results"][0]["hits"][0]["kb"], "odf");
        assert_eq!(out["skipped"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn limit_filters_and_bearer_reach_every_knowledge_base() {
        // Given: two knowledge bases that each insist on the caller's bearer,
        // the per-KB limit and the filter
        let server = MockServer::start().await;
        for kb in ["a", "b"] {
            Mock::given(method("POST"))
                .and(path(format!("/api/v1/knowledgebases/{kb}/search")))
                .and(header("authorization", "Bearer tok"))
                .and(body_partial_json(serde_json::json!({
                    "query": "q", "limit": 7,
                    "filter": {"concept_type": "Metric", "tags": ["finance"]}
                })))
                .respond_with(
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": []})),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        let mut args = args(&["a", "b"], "q", Some(7));
        args.filters = Some(SearchFilter {
            concept_type: Some("Metric".into()),
            tags: vec!["finance".into()],
            ..SearchFilter::default()
        });

        // When: the call fans out
        let result = run(&client(&server.uri()), args).await.unwrap();

        // Then: both were asked (the mocks' expectations hold on drop), and
        // both groups are present even though empty
        assert_eq!(kb_order(&output(result)), ["a", "b"]);
    }

    #[tokio::test]
    async fn explicit_list_with_an_unknown_slug_fails_and_names_it() {
        // Given: one knowledge base that exists and one the server has never
        // heard of
        let server = MockServer::start().await;
        search_mock("known", 200, &serde_json::json!([hit("a.md", 0.5)]))
            .mount(&server)
            .await;
        error_mock("nope", 404, "not_found").mount(&server).await;

        // When: both are named explicitly
        let error = run(&client(&server.uri()), args(&["known", "nope"], "q", None))
            .await
            .unwrap_err();

        // Then: the whole call fails as not_found, naming the slug
        assert_eq!(error.code, ErrorCode::RESOURCE_NOT_FOUND);
        assert!(error.message.starts_with("not_found"), "{}", error.message);
        assert!(error.message.contains("\"nope\""), "{}", error.message);
    }

    #[tokio::test]
    async fn explicit_list_with_a_forbidden_slug_fails_and_names_it() {
        // Given: a knowledge base the caller may not search
        let server = MockServer::start().await;
        search_mock("open", 200, &serde_json::json!([]))
            .mount(&server)
            .await;
        error_mock("hr", 403, "forbidden").mount(&server).await;

        // When: it is named explicitly beside an accessible one
        let error = run(&client(&server.uri()), args(&["open", "hr"], "q", None))
            .await
            .unwrap_err();

        // Then: the whole call fails as forbidden, naming the slug
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.starts_with("forbidden"), "{}", error.message);
        assert!(error.message.contains("\"hr\""), "{}", error.message);
    }

    #[tokio::test]
    async fn the_first_failure_in_request_order_is_reported() {
        // Given: two failing knowledge bases, the first-named one slower to
        // answer than the second
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/knowledgebases/slow/search"))
            .respond_with(
                ResponseTemplate::new(403)
                    .set_body_json(serde_json::json!({"error": "forbidden"}))
                    .set_delay(Duration::from_millis(200)),
            )
            .mount(&server)
            .await;
        error_mock("fast", 404, "not_found").mount(&server).await;

        // When: both are named, slow first
        let error = run(&client(&server.uri()), args(&["slow", "fast"], "q", None))
            .await
            .unwrap_err();

        // Then: the error is slow's, however the answers arrived
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("\"slow\""), "{}", error.message);
    }

    #[tokio::test]
    async fn omitted_kb_searches_every_visible_knowledge_base() {
        // Given: three knowledge bases visible to the caller
        let server = MockServer::start().await;
        kbs_mock(&["a", "b", "c"]).expect(1).mount(&server).await;
        for kb in ["a", "b", "c"] {
            search_mock(kb, 200, &serde_json::json!([hit("x.md", 0.5)]))
                .expect(1)
                .mount(&server)
                .await;
        }

        // When: the call names none
        let result = run(&client(&server.uri()), args(&[], "q", None))
            .await
            .unwrap();
        let out = output(result);

        // Then: all three are searched, in the order the API listed them
        assert_eq!(kb_order(&out), ["a", "b", "c"]);
        assert_eq!(out["results"][2]["hits"][0]["kb"], "c");
        assert_eq!(out["skipped"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn omitted_kb_skips_and_reports_knowledge_bases_that_refuse_search() {
        // Given: a visible knowledge base that refuses search, one whose
        // denial is the concealed 404, and one that answers
        let server = MockServer::start().await;
        kbs_mock(&["hr", "open", "hidden"]).mount(&server).await;
        error_mock("hr", 403, "forbidden").mount(&server).await;
        search_mock("open", 200, &serde_json::json!([hit("x.md", 0.5)]))
            .mount(&server)
            .await;
        error_mock("hidden", 404, "not_found").mount(&server).await;

        // When: the call names none
        let result = run(&client(&server.uri()), args(&[], "q", None))
            .await
            .unwrap();
        let out = output(result);

        // Then: the refusals are dropped and named, the answer is kept
        assert_eq!(kb_order(&out), ["open"]);
        assert_eq!(out["skipped"], serde_json::json!(["hr", "hidden"]));
    }

    #[tokio::test]
    async fn omitted_kb_still_fails_on_any_other_error() {
        // Given: a visible knowledge base whose backend is down
        let server = MockServer::start().await;
        kbs_mock(&["open", "down"]).mount(&server).await;
        search_mock("open", 200, &serde_json::json!([]))
            .mount(&server)
            .await;
        error_mock("down", 503, "backend_unavailable")
            .mount(&server)
            .await;

        // When: the call names none
        let error = run(&client(&server.uri()), args(&[], "q", None))
            .await
            .unwrap_err();

        // Then: a 503 is not a refusal to skip; the call fails, naming it
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("\"down\""), "{}", error.message);
    }

    #[tokio::test]
    async fn duplicate_slugs_are_refused_before_any_request() {
        // Given: a server that must not be asked anything
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": []})))
            .expect(0)
            .mount(&server)
            .await;

        // When: one slug is listed twice
        let error = run(&client(&server.uri()), args(&["a", "b", "a"], "q", None))
            .await
            .unwrap_err();

        // Then: invalid_request naming the duplicate, no HTTP call made
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("\"a\""), "{}", error.message);
    }

    #[tokio::test]
    async fn a_wide_fan_out_is_paced_and_still_answers_in_request_order() {
        // Given: more knowledge bases than run at once, each answering after
        // a short delay so that pacing is observable in the total time
        let server = MockServer::start().await;
        let n = CONCURRENCY * 2;
        let slugs: Vec<String> = (0..n).map(|i| format!("kb{i}")).collect();
        for slug in &slugs {
            Mock::given(method("POST"))
                .and(path(format!("/api/v1/knowledgebases/{slug}/search")))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"hits": [hit("x.md", 0.5)]}))
                        .set_delay(Duration::from_millis(100)),
                )
                .expect(1)
                .mount(&server)
                .await;
        }
        let refs: Vec<&str> = slugs.iter().map(String::as_str).collect();

        // When: all of them are named in one call
        let started = std::time::Instant::now();
        let result = run(&client(&server.uri()), args(&refs, "q", None))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        // Then: two waves ran (not one, not n), and the groups come back in
        // request order with every hit naming its knowledge base
        assert!(
            elapsed >= Duration::from_millis(200),
            "two waves of {CONCURRENCY} should take two delays: {elapsed:?}"
        );
        let out = output(result);
        assert_eq!(kb_order(&out), refs);
        assert_eq!(out["results"][n - 1]["hits"][0]["kb"], slugs[n - 1]);
    }

    #[test]
    fn schema_takes_kb_as_an_optional_list_of_slugs() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchArgs)).unwrap();
        let kb = &schema["properties"]["kb"];
        assert_eq!(kb["type"], "array");
        assert_eq!(kb["items"]["type"], "string");
        assert!(kb["description"].as_str().unwrap().contains("Omit"), "{kb}");
        let required: Vec<&str> = schema["required"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(required, ["query"]);
        assert!(
            schema["properties"]["limit"]["description"]
                .as_str()
                .unwrap()
                .contains("per knowledge base"),
            "{schema}"
        );
    }

    #[tokio::test]
    async fn okf_filters_and_hit_metadata_are_forwarded() {
        let server = MockServer::start().await;
        let metadata = serde_json::json!({"concept_id": "revenue", "type": "business-glossary", "tags": ["finance"]});
        Mock::given(method("POST"))
            .and(path("/api/v1/knowledgebases/notes/search"))
            .and(body_partial_json(serde_json::json!({"filter": {"concept_type": "business-glossary", "tags": ["finance"]}})))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": [{
                "object_key": "revenue.md", "byte_start": 0, "byte_end": 10, "score": 0.9, "preview": "Revenue", "okf": metadata
            }]})))
            .expect(1)
            .mount(&server).await;
        let args = serde_json::from_value(serde_json::json!({"kb": ["notes"], "query": "revenue", "filters": {"concept_type": "business-glossary", "tags": ["finance"]}})).unwrap();
        let result = run(&client(&server.uri()), args).await.unwrap();
        assert_eq!(output(result)["results"][0]["hits"][0]["okf"], metadata);
    }

    #[test]
    fn schema_exposes_okf_filter_fields() {
        let schema = serde_json::to_value(schemars::schema_for!(SearchFilter)).unwrap();
        assert!(schema["properties"]["concept_type"].is_object());
        assert!(schema["properties"]["tags"].is_object());
    }

    /// The tool's filter is the API's filter: every field the HTTP body
    /// accepts is in the published schema, with a description, and nothing
    /// else is. A field added to one without the other fails here.
    #[test]
    fn schema_matches_the_api_filter() {
        let api = serde_json::to_value(notedthat_core::search::SearchFilter {
            object_key_prefix: Some("p".into()),
            mime: Some("m".into()),
            concept_type: Some("c".into()),
            heading_path_prefix: vec!["h".into()],
            updated_after: Some(1),
            updated_before: Some(2),
            tags: vec!["t".into()],
        })
        .unwrap();
        let api_fields: std::collections::BTreeSet<&str> = api
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            api_fields.len(),
            7,
            "a fully-populated filter names every field"
        );

        let schema = serde_json::to_value(schemars::schema_for!(SearchFilter)).unwrap();
        let properties = schema["properties"].as_object().unwrap();
        let tool_fields: std::collections::BTreeSet<&str> =
            properties.keys().map(String::as_str).collect();
        assert_eq!(tool_fields, api_fields);
        for (name, property) in properties {
            assert!(
                property["description"]
                    .as_str()
                    .is_some_and(|d| !d.is_empty()),
                "{name} has a description for the agent to read"
            );
        }
    }

    #[tokio::test]
    async fn every_filter_field_is_forwarded_under_filter() {
        // Given: an API that insists on the whole filter, under the API's key
        let server = MockServer::start().await;
        let filter = serde_json::json!({
            "object_key_prefix": "docs/", "mime": "text/markdown",
            "concept_type": "Metric", "heading_path_prefix": ["Intro", "Scope"],
            "updated_after": 1_700_000_000, "updated_before": 1_800_000_000,
            "tags": ["finance", "q3"]
        });
        Mock::given(method("POST"))
            .and(path("/api/v1/knowledgebases/notes/search"))
            .and(body_partial_json(
                serde_json::json!({ "query": "q", "filter": filter }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits": []})))
            .expect(1)
            .mount(&server)
            .await;
        let args: SearchArgs = serde_json::from_value(
            serde_json::json!({ "kb": ["notes"], "query": "q", "filters": filter }),
        )
        .unwrap();

        // When / Then: the mock's expectation is the assertion
        run(&client(&server.uri()), args).await.unwrap();
    }

    #[test]
    fn unknown_argument_keys_are_refused_and_named() {
        // `filter` (singular) is the API's spelling and the likeliest slip.
        let err = serde_json::from_value::<SearchArgs>(
            serde_json::json!({ "kb": ["notes"], "query": "q", "filter": { "mime": "a/b" } }),
        )
        .unwrap_err()
        .to_string();
        // Backticked, so the accepted `filters` is not found inside the
        // unknown `filter` — or the other way round.
        assert!(
            err.contains("unknown field `filter`") && err.contains("`filters`"),
            "{err}"
        );

        let err = serde_json::from_value::<SearchArgs>(
            serde_json::json!({ "query": "q", "filters": { "mimetype": "a/b" } }),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("mimetype"), "{err}");
    }

    #[tokio::test]
    async fn happy_returns_hits() {
        let server = MockServer::start().await;
        search_mock("notes", 200, &serde_json::json!([hit("a.md", 0.9)]))
            .mount(&server)
            .await;
        let result = run(&client(&server.uri()), args(&["notes"], "hello", None))
            .await
            .unwrap();
        assert_eq!(
            output(result)["results"][0]["hits"][0]["object_key"],
            "a.md"
        );
    }

    #[tokio::test]
    async fn filter_field_renamed_to_singular() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/knowledgebases/notes/search"))
            .and(body_string_contains("\"filter\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"hits":[]})))
            .mount(&server)
            .await;
        let mut args = args(&["notes"], "q", None);
        args.filters = Some(SearchFilter {
            mime: Some("text/markdown".into()),
            ..SearchFilter::default()
        });
        let result = run(&client(&server.uri()), args).await.unwrap();
        assert!(!result.content.is_empty());
    }

    #[tokio::test]
    async fn a_bare_string_kb_is_rejected() {
        let parsed =
            serde_json::from_value::<SearchArgs>(serde_json::json!({"kb": "notes", "query": "q"}));
        assert!(parsed.is_err(), "kb is a list, never a bare slug");
    }
}
