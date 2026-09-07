//! Walking a bundle and reporting its OKF conformance.

use super::{FETCH_CONCURRENCY, MAX_DOC_BYTES, MAX_FINDINGS, MAX_LINK_CHECKS, VALIDATE_BUDGET};
use futures::stream::{self, StreamExt};
use notedthat_core::{ConditionalHeaders, KbSlug, ObjectPath, Storage, StorageError};
use notedthat_okf::{LinkTarget, Severity, conformance};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Instant;

/// One conformance observation, as returned over the wire.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ReportFinding {
    /// Object the finding is about.
    pub path: String,
    /// Stable machine-readable rule identifier.
    pub rule: &'static str,
    /// `"error"` or `"warning"`.
    pub severity: &'static str,
    /// 1-based line number, when the finding is anchored to one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// Human-readable explanation.
    pub message: String,
}

/// Counts of findings by severity.
#[derive(Debug, Clone, Default, Serialize, PartialEq, Eq)]
pub struct FindingCounts {
    /// Findings that break an OKF §11 conformance clause.
    pub error: usize,
    /// Tolerated departures.
    pub warning: usize,
}

/// The result of validating a bundle or a single object.
///
/// The flags are separate booleans because each one answers a different question
/// the caller has to act on, and the JSON shape is the published contract.
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ValidationReport {
    /// The bundle's declared version, when its root `index.md` states one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub okf_version: Option<String>,
    /// Whether the inspected objects carry no conformance errors.
    pub conformant: bool,
    /// Number of Markdown objects inspected.
    pub scanned: usize,
    /// Number of objects skipped because they are not Markdown.
    pub skipped_non_markdown: usize,
    /// Whether more objects remain beyond this page or the time budget.
    pub truncated: bool,
    /// Cursor to resume from, when `truncated`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// Findings by severity.
    pub counts: FindingCounts,
    /// The findings themselves.
    pub findings: Vec<ReportFinding>,
    /// Whether the findings list hit its cap.
    pub findings_truncated: bool,
    /// Whether link checking hit its cap.
    pub link_checks_truncated: bool,
}

impl ValidationReport {
    fn finish(mut self, findings: Vec<ReportFinding>) -> Self {
        self.counts = FindingCounts {
            error: findings
                .iter()
                .filter(|f| f.severity == Severity::Error.as_str())
                .count(),
            warning: findings
                .iter()
                .filter(|f| f.severity == Severity::Warning.as_str())
                .count(),
        };
        // OKF §11 lists broken links, unknown types and unknown keys as things a
        // consumer must tolerate, so only errors can make a bundle non-conformant.
        self.conformant = self.counts.error == 0;
        self.findings_truncated = findings.len() > MAX_FINDINGS;
        self.findings = findings.into_iter().take(MAX_FINDINGS).collect();
        self
    }
}

/// Parameters for one bundle walk.
#[derive(Debug, Clone)]
pub struct WalkParams {
    /// Restrict the walk to this key prefix.
    pub prefix: Option<String>,
    /// Maximum objects to list in this page.
    pub limit: u32,
    /// Continuation cursor from a previous report.
    pub cursor: Option<String>,
    /// Whether to check that in-body links resolve to objects that exist.
    pub check_links: bool,
}

/// Validate one page of a bundle.
///
/// Lists a single page, fetches only the Markdown objects that are within the
/// size cap, and checks each one. Never fetches an object it can reject from its
/// listed size alone.
///
/// # Errors
///
/// Returns the underlying [`StorageError`] when listing fails.
pub async fn validate_bundle(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    params: &WalkParams,
) -> Result<ValidationReport, StorageError> {
    let started = Instant::now();
    let listing = storage
        .list_objects(
            kb,
            params.prefix.as_deref(),
            params.limit,
            params.cursor.as_deref(),
        )
        .await?;

    let mut report = ValidationReport {
        okf_version: None,
        conformant: true,
        scanned: 0,
        skipped_non_markdown: 0,
        truncated: listing.truncated,
        next_cursor: listing.next_cursor.clone(),
        counts: FindingCounts::default(),
        findings: Vec::new(),
        findings_truncated: false,
        link_checks_truncated: false,
    };

    let present: BTreeSet<String> = listing
        .objects
        .iter()
        .map(|meta| meta.key.clone())
        .collect();

    let mut to_fetch: Vec<String> = Vec::new();
    let mut findings: Vec<ReportFinding> = Vec::new();

    for meta in &listing.objects {
        if !is_markdown(&meta.key) {
            report.skipped_non_markdown += 1;
            continue;
        }
        if meta.size > MAX_DOC_BYTES {
            findings.push(to_report(
                &meta.key,
                &conformance::object_too_large(meta.size, MAX_DOC_BYTES),
            ));
            continue;
        }
        to_fetch.push(meta.key.clone());
    }

    let fetched: Vec<(String, Result<String, FetchProblem>)> = stream::iter(to_fetch)
        .map(|key| async move {
            let text = fetch_text(storage, kb, &key).await;
            (key, text)
        })
        .buffer_unordered(FETCH_CONCURRENCY)
        .collect()
        .await;

    let mut link_candidates: Vec<(String, conformance::FoundLink, LinkTarget)> = Vec::new();

    for (key, result) in fetched {
        if started.elapsed() > VALIDATE_BUDGET {
            report.truncated = true;
            break;
        }
        let text = match result {
            Ok(text) => text,
            Err(FetchProblem::NonUtf8) => {
                findings.push(to_report(&key, &conformance::non_utf8()));
                report.scanned += 1;
                continue;
            }
            Err(FetchProblem::Missing) => continue,
            Err(FetchProblem::Storage(err)) => return Err(err),
        };
        report.scanned += 1;

        if report.okf_version.is_none() && key == notedthat_okf::INDEX_FILE {
            report.okf_version = notedthat_okf::parse_index(&text).okf_version;
        }

        for finding in conformance::check_object(&key, &text) {
            findings.push(to_report(&key, &finding));
        }

        if params.check_links {
            let base_dir = notedthat_okf::dir_of(&key).to_string();
            for link in notedthat_okf::collect_links(&text) {
                let target = notedthat_okf::resolve_link(&base_dir, &link.target);
                link_candidates.push((key.clone(), link, target));
            }
        }
    }

    if params.check_links {
        let (link_findings, truncated) =
            check_links(storage, kb, &present, link_candidates).await?;
        findings.extend(link_findings);
        report.link_checks_truncated = truncated;
    }

    Ok(report.finish(findings))
}

/// Validate a single object.
///
/// The call an agent makes right after a write, without paying for a bundle walk.
///
/// # Errors
///
/// Returns the underlying [`StorageError`] when the read fails for a reason other
/// than the object being absent.
pub async fn validate_one(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    path: &ObjectPath,
    with_link_checks: bool,
) -> Result<ValidationReport, StorageError> {
    let key = path.as_str().to_string();
    let mut report = ValidationReport {
        okf_version: None,
        conformant: true,
        scanned: 0,
        skipped_non_markdown: 0,
        truncated: false,
        next_cursor: None,
        counts: FindingCounts::default(),
        findings: Vec::new(),
        findings_truncated: false,
        link_checks_truncated: false,
    };

    if !is_markdown(&key) {
        report.skipped_non_markdown = 1;
        return Ok(report.finish(Vec::new()));
    }

    let text = match fetch_text(storage, kb, &key).await {
        Ok(text) => text,
        Err(FetchProblem::NonUtf8) => {
            return Ok(report.finish(vec![to_report(&key, &conformance::non_utf8())]));
        }
        Err(FetchProblem::Missing) => {
            return Err(StorageError::NotFound { key: key.clone() });
        }
        Err(FetchProblem::Storage(err)) => return Err(err),
    };
    report.scanned = 1;

    let mut findings: Vec<ReportFinding> = conformance::check_object(&key, &text)
        .iter()
        .map(|finding| to_report(&key, finding))
        .collect();

    if with_link_checks {
        let base_dir = notedthat_okf::dir_of(&key).to_string();
        let candidates: Vec<_> = notedthat_okf::collect_links(&text)
            .into_iter()
            .map(|link| {
                let target = notedthat_okf::resolve_link(&base_dir, &link.target);
                (key.clone(), link, target)
            })
            .collect();
        let (link_findings, truncated) =
            check_links(storage, kb, &BTreeSet::new(), candidates).await?;
        findings.extend(link_findings);
        report.link_checks_truncated = truncated;
    }

    Ok(report.finish(findings))
}

/// Resolve each candidate link to an existence answer.
///
/// Targets already seen in the listing are answered for free; only the remainder
/// costs a round trip, and each distinct target is checked at most once.
async fn check_links(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    present: &BTreeSet<String>,
    candidates: Vec<(String, conformance::FoundLink, LinkTarget)>,
) -> Result<(Vec<ReportFinding>, bool), StorageError> {
    let mut memo: HashMap<String, bool> = HashMap::new();
    let mut findings = Vec::new();
    let mut checks = 0usize;
    let mut truncated = false;

    for (key, link, target) in candidates {
        // An absolute URL is never resolved and never counted as broken.
        let lookup = match &target {
            LinkTarget::External(_) => continue,
            LinkTarget::Unresolvable { .. } => {
                findings.push(to_report(&key, &conformance::broken_link(&link)));
                continue;
            }
            LinkTarget::Internal(path) => path.as_str().to_string(),
            LinkTarget::Directory(prefix) => prefix.clone(),
        };

        let exists = if present.contains(&lookup) {
            true
        } else if let Some(known) = memo.get(&lookup) {
            *known
        } else {
            if checks >= MAX_LINK_CHECKS {
                truncated = true;
                break;
            }
            checks += 1;
            let exists = target_exists(storage, kb, &target).await?;
            memo.insert(lookup.clone(), exists);
            exists
        };

        if !exists {
            findings.push(to_report(&key, &conformance::broken_link(&link)));
        }
    }

    Ok((findings, truncated))
}

async fn target_exists(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    target: &LinkTarget,
) -> Result<bool, StorageError> {
    match target {
        LinkTarget::Internal(path) => {
            match storage
                .head_object(kb, path, ConditionalHeaders::default())
                .await
            {
                Ok(_) => Ok(true),
                Err(StorageError::NotFound { .. }) => Ok(false),
                Err(err) => Err(err),
            }
        }
        // A directory is a virtual prefix (D40); HEAD on `subdir/` would always
        // 404, so existence means "at least one object lives under it".
        LinkTarget::Directory(prefix) => {
            let listing = storage.list_objects(kb, Some(prefix), 1, None).await?;
            Ok(!listing.objects.is_empty())
        }
        LinkTarget::External(_) | LinkTarget::Unresolvable { .. } => Ok(false),
    }
}

enum FetchProblem {
    NonUtf8,
    Missing,
    Storage(StorageError),
}

async fn fetch_text(
    storage: &Arc<dyn Storage>,
    kb: &KbSlug,
    key: &str,
) -> Result<String, FetchProblem> {
    let path = ObjectPath::try_from_str(key).map_err(|_| FetchProblem::Missing)?;
    let read = storage
        .get_object(kb, &path, None, ConditionalHeaders::default())
        .await
        .map_err(|err| match err {
            StorageError::NotFound { .. } => FetchProblem::Missing,
            other => FetchProblem::Storage(other),
        })?;
    String::from_utf8(read.bytes.to_vec()).map_err(|_| FetchProblem::NonUtf8)
}

fn to_report(path: &str, finding: &notedthat_okf::Finding) -> ReportFinding {
    ReportFinding {
        path: path.to_string(),
        rule: finding.rule,
        severity: finding.severity.as_str(),
        line: finding.line,
        message: finding.message.clone(),
    }
}

/// Whether an object key names a Markdown document, case-insensitively.
pub(crate) fn is_markdown(key: &str) -> bool {
    matches!(
        key.rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .as_deref(),
        Some("md" | "markdown")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_detection_is_extension_based_and_case_insensitive() {
        assert!(is_markdown("a.md"));
        assert!(is_markdown("A.MD"));
        assert!(is_markdown("dir/b.markdown"));
        assert!(!is_markdown("a.txt"));
        assert!(!is_markdown("mdfile"));
    }

    #[test]
    fn conformance_is_decided_by_errors_alone() {
        let report = ValidationReport {
            okf_version: None,
            conformant: true,
            scanned: 1,
            skipped_non_markdown: 0,
            truncated: false,
            next_cursor: None,
            counts: FindingCounts::default(),
            findings: Vec::new(),
            findings_truncated: false,
            link_checks_truncated: false,
        };
        let warned = report.clone().finish(vec![ReportFinding {
            path: "a.md".into(),
            rule: "broken_link",
            severity: Severity::Warning.as_str(),
            line: Some(1),
            message: "x".into(),
        }]);
        assert!(warned.conformant, "a warning must not break conformance");
        assert_eq!(warned.counts.warning, 1);

        let errored = report.finish(vec![ReportFinding {
            path: "a.md".into(),
            rule: "type_missing",
            severity: Severity::Error.as_str(),
            line: None,
            message: "x".into(),
        }]);
        assert!(!errored.conformant);
        assert_eq!(errored.counts.error, 1);
    }

    #[test]
    fn findings_are_capped() {
        let report = ValidationReport {
            okf_version: None,
            conformant: true,
            scanned: 0,
            skipped_non_markdown: 0,
            truncated: false,
            next_cursor: None,
            counts: FindingCounts::default(),
            findings: Vec::new(),
            findings_truncated: false,
            link_checks_truncated: false,
        };
        let many: Vec<ReportFinding> = (0..MAX_FINDINGS + 10)
            .map(|i| ReportFinding {
                path: format!("{i}.md"),
                rule: "type_missing",
                severity: Severity::Error.as_str(),
                line: None,
                message: "x".into(),
            })
            .collect();
        let finished = report.finish(many);
        assert_eq!(finished.findings.len(), MAX_FINDINGS);
        assert!(finished.findings_truncated);
        // Counts reflect everything found, not just what fit.
        assert_eq!(finished.counts.error, MAX_FINDINGS + 10);
    }
}
