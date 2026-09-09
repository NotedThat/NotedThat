//! Naming one configuration setting across both of the ways it can be supplied.
//!
//! Every setting has an environment variable and a command-line flag, and the
//! flag wins. Which one an operator used is not knowable from inside a
//! validator — so a diagnostic names both, and the mapping between them is
//! mechanical rather than a table that can drift.

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

#[cfg(test)]
mod tests {
    use super::{flag_for, setting};

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
