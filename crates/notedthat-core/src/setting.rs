//! Naming one configuration setting across both of the ways it can be supplied.
//!
//! Every setting has an environment variable and a command-line flag, and the
//! flag wins. Which one an operator used is not knowable from inside a
//! validator — so a diagnostic names both, and the mapping between them is
//! mechanical rather than a table that can drift.
//!
//! The parsers here are the shared rules for one kind of setting each. They
//! take an already-read `&str`; each crate keeps its own front door ([`utf8`]
//! for an `OsStr`, or a trim) so a difference in blank handling is a visible
//! call, not a copy that drifted.

use std::ffi::OsStr;
use std::ops::RangeInclusive;

use crate::Error;

/// The long flag matching an environment variable name.
///
/// The `NOTEDTHAT_` prefix is the binary's own namespace and carries no meaning
/// on a flag, so it is dropped; anything else keeps its prefix. The remainder is
/// lowercased with underscores turned into dashes.
///
/// ```
/// use notedthat_core::flag_for;
/// assert_eq!(flag_for("NOTEDTHAT_S3_ENDPOINT_URL"), "--s3-endpoint-url");
/// assert_eq!(flag_for("EMBEDDING_MODEL"), "--embedding-model");
/// ```
#[must_use]
pub fn flag_for(env_var: &str) -> String {
    let stem = env_var.strip_prefix("NOTEDTHAT_").unwrap_or(env_var);
    let mut flag = String::with_capacity(stem.len() + 2);
    flag.push_str("--");
    for ch in stem.chars() {
        flag.push(if ch == '_' {
            '-'
        } else {
            ch.to_ascii_lowercase()
        });
    }
    flag
}

/// Name a setting the way an operator can act on it, whichever form they used.
///
/// ```
/// use notedthat_core::setting;
/// assert_eq!(setting("NOTEDTHAT_FS_ROOT"), "NOTEDTHAT_FS_ROOT (--fs-root)");
/// ```
#[must_use]
pub fn setting(env_var: &str) -> String {
    format!("{env_var} ({})", flag_for(env_var))
}

/// A configuration error carrying `message`.
#[must_use]
pub fn config_error(message: String) -> Error {
    Error::Config { message }
}

/// Read a setting as UTF-8, `None` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting when the value is not UTF-8 or is empty.
pub fn utf8<'a>(var: &str, supplied: Option<&'a OsStr>) -> Result<Option<&'a str>, Error> {
    let Some(value) = supplied else {
        return Ok(None);
    };
    let value = value
        .to_str()
        .ok_or_else(|| config_error(format!("{} must be valid UTF-8", setting(var))))?;
    if value.is_empty() {
        return Err(config_error(format!("{} must not be empty", setting(var))));
    }
    Ok(Some(value))
}

fn invalid(var: &str, accepted: &str, value: &str) -> Error {
    config_error(format!(
        "{} is invalid: expected {accepted}, got \"{value}\"",
        setting(var)
    ))
}

/// Parse one of a fixed set of values, `None` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting, what it accepts and the value when
/// `parse` refuses it.
pub fn parse_choice<T>(
    var: &str,
    supplied: Option<&str>,
    parse: impl Fn(&str) -> Option<T>,
    accepted: &str,
) -> Result<Option<T>, Error> {
    supplied
        .map(|value| parse(value).ok_or_else(|| invalid(var, accepted, value)))
        .transpose()
}

/// Parse a switch that is exactly `true` or `false`, `default` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting for any other value, the empty one included.
pub fn parse_bool(var: &str, supplied: Option<&str>, default: bool) -> Result<bool, Error> {
    let parsed = parse_choice(
        var,
        supplied,
        |value| match value {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        "\"true\" or \"false\"",
    )?;
    Ok(parsed.unwrap_or(default))
}

/// Parse a positive integer, `default` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting and `accepted` when the value is not a
/// number or is zero.
pub fn parse_positive(
    var: &str,
    supplied: Option<&str>,
    default: u64,
    accepted: &str,
) -> Result<u64, Error> {
    let parsed = parse_choice(
        var,
        supplied,
        |value| value.parse::<u64>().ok().filter(|n| *n > 0),
        accepted,
    )?;
    Ok(parsed.unwrap_or(default))
}

/// Parse an integer within `range`, `default` when absent.
///
/// # Errors
///
/// `Error::Config` naming the setting, the range and `unit` when the value is
/// not a number or falls outside the range.
pub fn parse_in_range(
    var: &str,
    supplied: Option<&str>,
    range: RangeInclusive<u64>,
    default: u64,
    unit: &str,
) -> Result<u64, Error> {
    let accepted = format!("{}-{} {unit}", range.start(), range.end());
    let parsed = parse_choice(
        var,
        supplied,
        |value| value.parse::<u64>().ok().filter(|n| range.contains(n)),
        &accepted,
    )?;
    Ok(parsed.unwrap_or(default))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message<T: std::fmt::Debug>(result: Result<T, Error>) -> String {
        match result {
            Err(Error::Config { message }) => message,
            other => panic!("expected a config error, got {other:?}"),
        }
    }

    #[test]
    fn utf8_refuses_empty_and_non_utf8_values() {
        assert_eq!(utf8("NOTEDTHAT_X", None).unwrap(), None);
        assert_eq!(
            utf8("NOTEDTHAT_X", Some(OsStr::new("a"))).unwrap(),
            Some("a")
        );
        assert_eq!(
            message(utf8("NOTEDTHAT_X", Some(OsStr::new("")))),
            "NOTEDTHAT_X (--x) must not be empty"
        );
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt as _;
            assert_eq!(
                message(utf8("NOTEDTHAT_X", Some(OsStr::from_bytes(b"\xff")))),
                "NOTEDTHAT_X (--x) must be valid UTF-8"
            );
        }
    }

    #[test]
    fn parse_bool_accepts_exactly_true_and_false() {
        assert!(parse_bool("NOTEDTHAT_X", None, true).unwrap());
        assert!(parse_bool("NOTEDTHAT_X", Some("true"), false).unwrap());
        assert!(!parse_bool("NOTEDTHAT_X", Some("false"), true).unwrap());
        for value in ["", " true", "yes"] {
            assert_eq!(
                message(parse_bool("NOTEDTHAT_X", Some(value), false)),
                format!(
                    "NOTEDTHAT_X (--x) is invalid: expected \"true\" or \"false\", got \"{value}\""
                )
            );
        }
    }

    #[test]
    fn parse_choice_names_what_it_accepts() {
        let parse = |value: &str| (value == "a").then_some(1);
        assert_eq!(
            parse_choice("NOTEDTHAT_X", None, parse, "\"a\"").unwrap(),
            None
        );
        assert_eq!(
            parse_choice("NOTEDTHAT_X", Some("a"), parse, "\"a\"").unwrap(),
            Some(1)
        );
        assert_eq!(
            message(parse_choice("NOTEDTHAT_X", Some("b"), parse, "\"a\"")),
            "NOTEDTHAT_X (--x) is invalid: expected \"a\", got \"b\""
        );
    }

    #[test]
    fn parse_positive_refuses_zero_and_garbage() {
        assert_eq!(parse_positive("NOTEDTHAT_X", None, 7, "n").unwrap(), 7);
        assert_eq!(parse_positive("NOTEDTHAT_X", Some("5"), 7, "n").unwrap(), 5);
        for value in ["0", "-1", "x", " 5"] {
            assert_eq!(
                message(parse_positive("NOTEDTHAT_X", Some(value), 7, "n")),
                format!("NOTEDTHAT_X (--x) is invalid: expected n, got \"{value}\"")
            );
        }
    }

    #[test]
    fn parse_in_range_refuses_values_outside_the_range() {
        let parse = |value| parse_in_range("NOTEDTHAT_X", value, 50..=100, 60, "ms");
        assert_eq!(parse(None).unwrap(), 60);
        assert_eq!(parse(Some("50")).unwrap(), 50);
        assert_eq!(parse(Some("100")).unwrap(), 100);
        for value in ["49", "101", "x"] {
            assert_eq!(
                message(parse(Some(value))),
                format!("NOTEDTHAT_X (--x) is invalid: expected 50-100 ms, got \"{value}\"")
            );
        }
    }

    #[test]
    fn the_binary_namespace_is_dropped_but_other_prefixes_are_kept() {
        assert_eq!(flag_for("NOTEDTHAT_API_TOKEN"), "--api-token");
        assert_eq!(flag_for("EMBEDDING_API_KEY"), "--embedding-api-key");
    }

    #[test]
    fn a_single_word_variable_becomes_a_single_word_flag() {
        assert_eq!(flag_for("NOTEDTHAT_KBS"), "--kbs");
    }

    #[test]
    fn a_diagnostic_names_both_forms() {
        assert_eq!(
            setting("NOTEDTHAT_LISTEN_ADDR"),
            "NOTEDTHAT_LISTEN_ADDR (--listen-addr)"
        );
    }
}
