//! The `s3` backend's startup check that conditional writes are enforced (D70).
//!
//! `NotedThat` forwards `If-Match` and `If-None-Match` to the object store verbatim and
//! adds no lock of its own, so optimistic concurrency is only as good as the backend's
//! enforcement of them. Some S3-compatible backends parse both and store the object
//! anyway (`SPECIFICATIONS.md` §8.1): a conditional write that should be `412` answers
//! `200`, and one of two concurrent writers is lost without anyone being told. Nothing
//! in a normal request can tell the difference, so each bucket is asked once, here,
//! before provisioning touches anything else.
//!
//! A bucket that does not enforce them refuses startup (D39) unless
//! `NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES` accepts the risk, in which case
//! the finding is logged (`S3_CONDITIONAL_WRITES_NOT_ENFORCED`), reported `degraded` by
//! `/readyz` for the life of the process, and set as
//! `notedthat_storage_conditional_writes_enforced{kb} 0`.
//!
//! One writer cannot provoke a failure that only appears under contention, so a
//! backend that passes here can still lose writes under load; §8.1 remains the
//! reference for those.

use notedthat_api_http::readiness::{Check, Unready};
use notedthat_core::metrics::{label, name};
use notedthat_core::{Error, KbSlug, Storage, TenantSlug, setting, validate_bucket_name};
use notedthat_storage_s3::{
    ConditionalWrites, S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV, S3Storage,
};
use tracing::{info, warn};

/// What `/readyz` reports for the `s3` backend's conditional writes.
const BACKEND: &str = "s3";

/// Ask every knowledge base's bucket whether it enforces conditional writes, and
/// refuse startup on one that does not unless `allow` says otherwise.
///
/// Returns the check `/readyz` carries: `ok` when every bucket enforced both
/// preconditions, `degraded` (`preconditions_not_enforced`) when at least one did not
/// and `allow` accepted it.
///
/// # Errors
///
/// Returns [`Error::Config`] naming the knowledge base and the setting when a bucket
/// does not enforce them and `allow` is `false`, and the storage error when a bucket
/// cannot be created or the check itself fails.
pub(super) async fn check(
    s3: &S3Storage,
    tenant: &TenantSlug,
    kbs: &[KbSlug],
    allow: bool,
) -> Result<Check, Error> {
    let mut all_enforced = true;
    for kb in kbs {
        // Provisioning does both again; here they are what makes the bucket there to
        // ask, and an unusable name is reported as that rather than as an S3 error.
        validate_bucket_name(tenant, kb)?;
        s3.ensure_bucket(kb).await?;
        let found = s3.check_conditional_writes(kb).await?;
        let enforced = judge(kb, found, allow)?;
        metrics::gauge!(
            name::STORAGE_CONDITIONAL_WRITES_ENFORCED,
            label::KB => kb.as_str().to_string(),
        )
        .set(if enforced { 1.0 } else { 0.0 });
        all_enforced &= enforced;
    }
    Ok(if all_enforced {
        Check::ok(BACKEND)
    } else {
        Check::unready(BACKEND, Unready::PreconditionsNotEnforced)
    })
}

/// Whether `kb`'s bucket enforced conditional writes, as an error when it did not and
/// that is not allowed. Logs the accepted case.
fn judge(kb: &KbSlug, found: ConditionalWrites, allow: bool) -> Result<bool, Error> {
    let problem = match found {
        ConditionalWrites::Enforced => {
            info!(
                target: "notedthat::storage",
                kb = %kb.as_str(),
                "conditional writes are enforced"
            );
            return Ok(true);
        }
        ConditionalWrites::NotEnforced {
            if_match,
            if_none_match,
        } => {
            let ignored = match (if_match, if_none_match) {
                (true, true) => {
                    "ignored both If-Match and If-None-Match: it stored a PUT whose If-Match \
                     did not hold, and one with If-None-Match: * over an existing object, \
                     instead of refusing them with 412"
                }
                (true, false) => {
                    "ignored If-Match: it stored a PUT whose If-Match did not hold instead of \
                     refusing it with 412"
                }
                _ => {
                    "ignored If-None-Match: it stored a PUT with If-None-Match: * over an \
                     existing object instead of refusing it with 412"
                }
            };
            format!(
                "{ignored}, so a concurrent write to this knowledge base would be silently lost"
            )
        }
        ConditionalWrites::Unsupported => "answered 501 Not Implemented to a conditional PUT, \
             so every write carrying If-Match or If-None-Match would fail"
            .to_string(),
    };
    let allow_setting = setting(S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES_ENV);
    if allow {
        warn!(
            target: "notedthat::storage",
            kb = %kb.as_str(),
            "S3_CONDITIONAL_WRITES_NOT_ENFORCED: the bucket {problem}; starting anyway because \
             {allow_setting} is true (SPECIFICATIONS.md §8.1)"
        );
        return Ok(false);
    }
    Err(Error::Config {
        message: format!(
            "the s3 bucket for knowledge base '{}' {problem} (SPECIFICATIONS.md §8.1). Use a \
             backend that enforces conditional writes, or set {allow_setting} to true to start \
             anyway and accept the risk",
            kb.as_str(),
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::config::retry::RetryConfig;
    use aws_sdk_s3::config::{BehaviorVersion, Credentials, Region};
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use wiremock::matchers::{header_exists, method};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn kb() -> KbSlug {
        KbSlug::try_new("notes").expect("kb slug")
    }

    fn refusal(found: ConditionalWrites) -> String {
        judge(&kb(), found, false)
            .expect_err("refused without the opt-out")
            .to_string()
    }

    #[test]
    fn an_enforcing_bucket_passes_either_way() {
        assert!(judge(&kb(), ConditionalWrites::Enforced, false).unwrap());
        assert!(judge(&kb(), ConditionalWrites::Enforced, true).unwrap());
    }

    #[test]
    fn a_bucket_ignoring_preconditions_refuses_startup_and_names_the_setting() {
        let message = refusal(ConditionalWrites::NotEnforced {
            if_match: true,
            if_none_match: true,
        });
        for needle in [
            "'notes'",
            "If-Match",
            "If-None-Match",
            "silently lost",
            "§8.1",
            "NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES",
            "--s3-allow-unenforced-conditional-writes",
        ] {
            assert!(message.contains(needle), "{message:?} should name {needle}");
        }
    }

    #[test]
    fn the_refusal_names_only_the_header_that_was_ignored() {
        let message = refusal(ConditionalWrites::NotEnforced {
            if_match: false,
            if_none_match: true,
        });
        assert!(message.contains("ignored If-None-Match"), "{message}");
        assert!(!message.contains("If-Match"), "{message}");

        let message = refusal(ConditionalWrites::NotEnforced {
            if_match: true,
            if_none_match: false,
        });
        assert!(message.contains("ignored If-Match"), "{message}");
        assert!(!message.contains("If-None-Match"), "{message}");
    }

    #[test]
    fn a_bucket_refusing_the_headers_outright_refuses_startup() {
        let message = refusal(ConditionalWrites::Unsupported);
        assert!(message.contains("501"), "{message}");
        assert!(
            message.contains("NOTEDTHAT_S3_ALLOW_UNENFORCED_CONDITIONAL_WRITES"),
            "{message}"
        );
    }

    #[test]
    fn the_opt_out_starts_and_reports_the_bucket_unenforced() {
        for found in [
            ConditionalWrites::NotEnforced {
                if_match: true,
                if_none_match: true,
            },
            ConditionalWrites::Unsupported,
        ] {
            assert!(!judge(&kb(), found, true).expect("allowed"));
        }
    }

    /// A fake S3 endpoint that creates buckets, deletes objects and stores every `PUT`,
    /// refusing a conditional one `412` only when `enforcing`.
    async fn fake_s3(enforcing: bool) -> MockServer {
        let server = MockServer::start().await;
        if enforcing {
            for header in ["if-match", "if-none-match"] {
                Mock::given(method("PUT"))
                    .and(header_exists(header))
                    .respond_with(ResponseTemplate::new(412))
                    .with_priority(1)
                    .mount(&server)
                    .await;
            }
        }
        Mock::given(method("PUT"))
            .respond_with(ResponseTemplate::new(200))
            .with_priority(5)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;
        server
    }

    fn s3_for(server: &MockServer) -> S3Storage {
        let sdk_config = aws_sdk_s3::config::Builder::new()
            .behavior_version(BehaviorVersion::latest())
            .endpoint_url(server.uri())
            .force_path_style(true)
            .region(Region::new("us-east-1"))
            .credentials_provider(Credentials::new("key", "secret", None, None, "test"))
            .retry_config(RetryConfig::disabled())
            .build();
        S3Storage::new(
            aws_sdk_s3::Client::from_conf(sdk_config),
            TenantSlug::default(),
        )
    }

    /// Run [`check`] against a fake backend under a recorder local to this thread, and
    /// return what it answered with the value it set on the gauge for `notes`.
    fn checked(enforcing: bool, allow: bool) -> (Result<Check, Error>, Option<f64>) {
        let recorder = DebuggingRecorder::new();
        let snapshotter = recorder.snapshotter();
        let result = metrics::with_local_recorder(&recorder, || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("runtime")
                .block_on(async {
                    let server = fake_s3(enforcing).await;
                    check(&s3_for(&server), &TenantSlug::default(), &[kb()], allow).await
                })
        });
        let gauge = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .find_map(|(key, _, _, value)| {
                let key = key.key();
                let for_notes = key.name() == name::STORAGE_CONDITIONAL_WRITES_ENFORCED
                    && key
                        .labels()
                        .any(|l| l.key() == label::KB && l.value() == "notes");
                match value {
                    DebugValue::Gauge(v) if for_notes => Some(v.into_inner()),
                    _ => None,
                }
            });
        (result, gauge)
    }

    #[test]
    fn an_enforcing_backend_is_ok_and_sets_the_gauge_to_one() {
        let (result, gauge) = checked(true, false);
        assert_eq!(result.expect("starts"), Check::ok("s3"));
        assert_eq!(gauge, Some(1.0));
    }

    #[test]
    fn an_accepted_unenforcing_backend_is_degraded_and_sets_the_gauge_to_zero() {
        let (result, gauge) = checked(false, true);
        assert_eq!(
            result.expect("the opt-out starts"),
            Check::unready("s3", Unready::PreconditionsNotEnforced)
        );
        assert_eq!(gauge, Some(0.0));
    }

    #[test]
    fn an_unenforcing_backend_refuses_startup() {
        let (result, _) = checked(false, false);
        let message = result.expect_err("refused").to_string();
        assert!(message.contains("'notes'"), "{message}");
    }
}
