//! Handler for `GET /v1/okf/{kb_slug}/computation?path=…`.
//!
//! # Non-execution invariant
//!
//! This route returns a **contract**. `NotedThat` never executes a computation, and
//! never issues an outbound HTTP request on an agent's behalf: an absolute URL in
//! `computation`, `executor.resource` or `attester.resource` is returned as a
//! string and nothing more. The server holds S3 credentials and sits inside the
//! network, so dereferencing a user-authored URL here would be an SSRF primitive.
//! See SPECIFICATIONS.md D48.

use axum::{
    Json,
    extract::{Path, Query, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use notedthat_core::{ConditionalHeaders, Error as CoreError, KbSlug, ObjectPath, StorageError};
use notedthat_okf::{LinkTarget, OkfConcept};
use serde::{Deserialize, Serialize};

use crate::{
    error::{ApiError, ApiErrorResponse},
    state::AppState,
};

/// Largest computation body this route will fetch (1 MiB).
pub const MAX_COMPUTATION_BYTES: u64 = 1024 * 1024;

/// Query parameters for a contract call.
#[derive(Debug, Deserialize)]
pub struct ContractQuery {
    /// The concept to read the contract from.
    pub path: String,
}

/// Where a computation body came from.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ComputationBody {
    /// `"inline"` for a fenced block in the body, `"file"` for a referenced file.
    pub source: &'static str,
    /// The fence info string, or the referenced file's extension.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// The computation source, verbatim.
    pub code: String,
    /// The resolved key the code was read from, for `source: "file"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// A resource referenced by the contract but deliberately not fetched.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContractResource {
    /// The reference exactly as written in the frontmatter.
    pub resource: String,
    /// The reference resolved to a knowledge base key, when it resolves to one.
    ///
    /// `null` for an absolute URL or a target outside the bundle. The document is
    /// never read: the agent has `read` and now has the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_path: Option<String>,
    /// Fields a run must return for attestation.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub receipt: Vec<String>,
}

/// One typed hole an agent may fill.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContractParameter {
    /// Parameter name.
    pub name: String,
    /// Declared type, verbatim.
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub param_type: Option<String>,
    /// Whether the parameter must be supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
}

/// A resolved Attested Computation contract.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ContractResponse {
    /// The concept this contract came from.
    pub path: String,
    /// The concept's OKF `type`.
    #[serde(rename = "type")]
    pub concept_type: String,
    /// Human-readable display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// How parameters bind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runtime: Option<String>,
    /// Typed, named holes. The computation itself is not editable by an agent.
    pub parameters: Vec<ContractParameter>,
    /// The computation body.
    pub computation: ComputationBody,
    /// How to run it. Referenced, never fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executor: Option<ContractResource>,
    /// How to check the receipt. Referenced, never fetched.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub attester: Option<ContractResource>,
    /// Notes that do not prevent the contract from being served.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Restates the invariant for any client that only reads the response.
    pub execution: &'static str,
}

/// Handle `GET /v1/okf/{kb_slug}/computation`.
///
/// # Errors
///
/// 400 for a malformed slug or path, an absolute-URL `computation`, or a
/// `computation` path escaping the knowledge base; 404 for an undeclared KB, a
/// missing concept, or a missing computation file; 413 for an oversized
/// computation file; 422 when the concept declares no computation.
pub async fn get_contract(
    State(state): State<AppState>,
    Path(kb_slug_raw): Path<String>,
    Query(query): Query<ContractQuery>,
    req: Request,
) -> Result<Response, ApiErrorResponse> {
    let request_id = crate::middleware::extract_request_id(&req);
    let err = |error: ApiError| ApiErrorResponse {
        error,
        request_id: request_id.clone(),
    };
    let invalid = |message: &str| {
        err(ApiError::Core(CoreError::InvalidInput {
            message: message.to_string(),
        }))
    };

    let kb_slug = KbSlug::try_new(kb_slug_raw).map_err(|e| err(ApiError::Core(e)))?;
    let kb = crate::router::lookup_kb(&state, kb_slug.as_str()).map_err(err)?;
    let concept_path = ObjectPath::try_from_str(&query.path).map_err(|e| err(ApiError::Core(e)))?;

    let read = state
        .storage
        .get_object(&kb, &concept_path, None, ConditionalHeaders::default())
        .await
        .map_err(|e| err(ApiError::from(e)))?;
    let text = String::from_utf8(read.bytes.to_vec())
        .map_err(|_| invalid("concept is not valid UTF-8"))?;

    let (concept, split) =
        notedthat_okf::parse_document(&text).map_err(|e| invalid(&format!("{e}")))?;

    let Some(computation) = concept.computation.as_ref() else {
        return Err(err(ApiError::Core(CoreError::InvalidInput {
            message: format!(
                "`{}` declares no computation (type `{}`)",
                concept_path.as_str(),
                concept.concept_type
            ),
        })));
    };

    let mut warnings: Vec<String> = Vec::new();
    let base_dir = notedthat_okf::dir_of(concept_path.as_str()).to_string();

    let body = if let Some(reference) = computation.computation.as_deref() {
        resolve_computation_file(&state, &kb, &base_dir, reference, &err, &invalid).await?
    } else {
        {
            let inline = notedthat_okf::extract_inline_computation(&text[split.body_start..])
                .ok_or_else(|| {
                    invalid(
                        "concept declares no `computation:` path and has no `# Computation` section",
                    )
                })?;
            if inline.unfenced {
                warnings.push(
                    "the `# Computation` section has no fenced code block; returning its raw text"
                        .to_string(),
                );
            }
            ComputationBody {
                source: "inline",
                language: inline.language,
                code: inline.code,
                origin: None,
            }
        }
    };

    Ok((
        StatusCode::OK,
        Json(build_response(
            concept_path.as_str(),
            &concept,
            &base_dir,
            body,
            warnings,
        )),
    )
        .into_response())
}

async fn resolve_computation_file(
    state: &AppState,
    kb: &KbSlug,
    base_dir: &str,
    reference: &str,
    err: &impl Fn(ApiError) -> ApiErrorResponse,
    invalid: &impl Fn(&str) -> ApiErrorResponse,
) -> Result<ComputationBody, ApiErrorResponse> {
    let path = match notedthat_okf::resolve_link(base_dir, reference) {
        LinkTarget::Internal(path) => path,
        LinkTarget::External(_) => {
            return Err(invalid(
                "`computation` must reference a path inside the bundle; absolute URLs are never fetched",
            ));
        }
        LinkTarget::Directory(_) => {
            return Err(invalid("`computation` references a directory, not a file"));
        }
        LinkTarget::Unresolvable { reason, .. } => {
            return Err(invalid(&format!("`computation` is unresolvable: {reason}")));
        }
    };

    // Size-check before reading, so an oversized file costs one HEAD.
    let meta = state
        .storage
        .head_object(kb, &path, ConditionalHeaders::default())
        .await
        .map_err(|e| err(ApiError::from(e)))?;
    if meta.size > MAX_COMPUTATION_BYTES {
        return Err(err(ApiError::Core(CoreError::PayloadTooLarge {
            size: meta.size,
            limit: MAX_COMPUTATION_BYTES,
        })));
    }

    let read = state
        .storage
        .get_object(kb, &path, None, ConditionalHeaders::default())
        .await
        .map_err(|e| match e {
            StorageError::NotFound { .. } => err(ApiError::Core(CoreError::NotFound {
                resource: format!("computation file '{}'", path.as_str()),
            })),
            other => err(ApiError::from(other)),
        })?;
    let code = String::from_utf8(read.bytes.to_vec())
        .map_err(|_| invalid("computation file is not valid UTF-8"))?;

    Ok(ComputationBody {
        source: "file",
        language: path
            .as_str()
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase()),
        code,
        origin: Some(path.as_str().to_string()),
    })
}

fn build_response(
    path: &str,
    concept: &OkfConcept,
    base_dir: &str,
    computation: ComputationBody,
    warnings: Vec<String>,
) -> ContractResponse {
    let family = concept.computation.as_ref();
    ContractResponse {
        path: path.to_string(),
        concept_type: concept.concept_type.clone(),
        title: concept.title.clone(),
        runtime: family.and_then(|c| c.runtime.clone()),
        parameters: family
            .map(|c| {
                c.parameters
                    .iter()
                    .map(|p| ContractParameter {
                        name: p.name.clone(),
                        param_type: p.param_type.clone(),
                        required: p.required,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        computation,
        executor: family.and_then(|c| c.executor.as_ref()).and_then(|e| {
            e.resource.as_ref().map(|resource| ContractResource {
                resource: resource.clone(),
                resource_path: resolved_key(base_dir, resource),
                receipt: e.receipt.clone(),
            })
        }),
        attester: family.and_then(|c| c.attester.as_ref()).and_then(|a| {
            a.resource.as_ref().map(|resource| ContractResource {
                resource: resource.clone(),
                resource_path: resolved_key(base_dir, resource),
                receipt: Vec::new(),
            })
        }),
        warnings,
        execution: "NotedThat never executes this computation. Run it with your own credentials.",
    }
}

/// Resolve a reference to a knowledge base key without reading anything.
fn resolved_key(base_dir: &str, reference: &str) -> Option<String> {
    match notedthat_okf::resolve_link(base_dir, reference) {
        LinkTarget::Internal(path) => Some(path.as_str().to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn concept_with_computation(yaml: &str) -> OkfConcept {
        notedthat_okf::parse_frontmatter_yaml(yaml).unwrap()
    }

    fn body() -> ComputationBody {
        ComputationBody {
            source: "inline",
            language: Some("sql".into()),
            code: "SELECT 1;".into(),
            origin: None,
        }
    }

    #[test]
    fn executor_and_attester_are_resolved_but_not_fetched() {
        let concept = concept_with_computation(
            "type: Attested Computation\nruntime: bigquery\n\
             executor: {resource: /executors/bq.md, receipt: [job_id]}\n\
             attester: {resource: ./fin.md}\n",
        );
        let response = build_response("a/b.md", &concept, "a/", body(), Vec::new());
        let executor = response.executor.unwrap();
        assert_eq!(executor.resource, "/executors/bq.md");
        assert_eq!(executor.resource_path.as_deref(), Some("executors/bq.md"));
        assert_eq!(executor.receipt, vec!["job_id"]);
        assert_eq!(
            response.attester.unwrap().resource_path.as_deref(),
            Some("a/fin.md")
        );
    }

    #[test]
    fn an_absolute_url_resource_has_no_resolved_key() {
        let concept = concept_with_computation(
            "type: Attested Computation\nexecutor: {resource: 'https://example.com/x.md'}\n",
        );
        let executor = build_response("a.md", &concept, "", body(), Vec::new())
            .executor
            .unwrap();
        assert_eq!(executor.resource, "https://example.com/x.md");
        assert!(executor.resource_path.is_none());
    }

    #[test]
    fn a_traversing_resource_has_no_resolved_key() {
        assert!(resolved_key("a/", "../../etc/passwd").is_none());
    }

    #[test]
    fn parameters_are_carried_through() {
        let concept = concept_with_computation(
            "type: Attested Computation\nparameters:\n  - {name: d, type: date, required: true}\n",
        );
        let response = build_response("a.md", &concept, "", body(), Vec::new());
        assert_eq!(response.parameters[0].name, "d");
        assert_eq!(response.parameters[0].required, Some(true));
    }

    #[test]
    fn the_response_states_the_non_execution_invariant() {
        let concept = concept_with_computation("type: Attested Computation\nruntime: dbt\n");
        let response = build_response("a.md", &concept, "", body(), Vec::new());
        assert!(response.execution.contains("never executes"));
    }
}
