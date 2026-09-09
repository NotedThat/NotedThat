//! `KeyPattern` — the glob language used to scope an access rule to part of a
//! knowledge base.
//!
//! # Why this is hand-rolled
//!
//! The obvious move is `globset`. It is rejected deliberately, for reasons that
//! are specific rather than stylistic:
//!
//! - `globset`'s `literal_separator` defaults to **false**, so its `*` matches
//!   `/`. A rule written `public/*` would then grant `public/deep/secret.md`.
//!   Getting that right means remembering `GlobBuilder::literal_separator(true)`
//!   at a call site where a reviewer cannot see the consequence. This is an
//!   authorization boundary; a matcher whose only possible behaviour is the
//!   documented one removes that entire failure class.
//! - `globset` only accepts `**` as a whole path component, so `docs/**.md` is a
//!   parse error there too — we would be writing this validator and these error
//!   messages regardless.
//! - [`crate::kb::KbManifest`] is `Serialize + Deserialize + Clone + PartialEq`.
//!   `globset::GlobMatcher` is none of the last three, so a newtype hand-writing
//!   all of them is required either way.
//!
//! # The language
//!
//! Patterns match a whole object key (no leading `/`), byte for byte,
//! case-sensitively, with four constructs:
//!
//! | Construct | Matches |
//! |---|---|
//! | literal   | itself |
//! | `*`       | zero or more characters **within one segment** — never `/` |
//! | `?`       | exactly one character, never `/` |
//! | `**`      | zero or more whole segments; must be an entire segment |
//! | `{a,b,c}` | alternation; no nesting, no empty branch |
//!
//! `**` also consumes the `/` that precedes it, so `public/**` matches the keys
//! `public`, `public/a` and `public/a/b/c`. Operators write `public/**` meaning
//! "the public area", and an object stored at exactly `public` is the thing they
//! meant to share. Strictly-below is spelled `public/*` plus `public/*/**`.
//!
//! There is no escape character in v1, so a key containing a literal `*`, `?` or
//! `{` cannot be matched by a pattern.

use crate::error::Error;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// Longest accepted pattern source, in bytes.
const MAX_PATTERN_BYTES: usize = 1024;
/// Most segments a single pattern may contain.
const MAX_SEGMENTS: usize = 64;
/// Most brace alternatives a single pattern may expand to.
///
/// Braces are expanded to a cartesian product at parse time, so this is what
/// stops `{a,b}{a,b}{a,b}…` from turning a manifest into a startup hang.
const MAX_ALTERNATIVES: usize = 256;

/// One piece of a pattern segment, between `/` separators.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    /// `*` — zero or more non-`/` characters.
    Star,
    /// `?` — exactly one non-`/` character.
    Any,
    /// A run of literal characters.
    Literal(String),
}

/// One `/`-separated piece of a pattern.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    /// `**` — zero or more whole segments.
    DoubleStar,
    /// A segment with no wildcards; compared with `==`.
    Literal(String),
    /// A segment containing `*` or `?`.
    Wild(Vec<Token>),
}

/// A validated glob pattern scoping an access rule to part of a knowledge base.
///
/// Parsing rejects anything malformed, so an invalid pattern cannot exist — the
/// same guarantee [`crate::ObjectPath`] gives for keys. Brace alternation is
/// expanded once, at parse time, which is what keeps matching linear and free of
/// recursion (and therefore free of any backtracking blow-up to reason about).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyPattern {
    /// The pattern exactly as written, so a manifest round-trips unchanged.
    source: String,
    /// Brace-free alternatives; the pattern matches if any one of them does.
    alternatives: Vec<Vec<Segment>>,
}

impl KeyPattern {
    /// The pattern granting a whole knowledge base.
    ///
    /// This is what an omitted `under` means, expressed as a real pattern rather
    /// than as a special case the evaluator has to remember.
    pub fn whole_kb() -> Self {
        Self::parse("**").expect("`**` is a valid pattern")
    }

    /// Returns whether this is exactly the whole-knowledge-base pattern.
    pub fn is_whole_kb(&self) -> bool {
        self.source == "**"
    }

    /// Parse and validate a pattern.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidInput`] describing what is wrong with the pattern.
    pub fn parse(input: &str) -> Result<Self, Error> {
        if input.is_empty() {
            return Err(invalid("pattern must not be empty"));
        }
        if input.len() > MAX_PATTERN_BYTES {
            return Err(invalid(format!(
                "pattern exceeds {MAX_PATTERN_BYTES} bytes"
            )));
        }
        if input.starts_with('/') {
            return Err(invalid(
                "pattern must not start with '/'; keys have no leading slash",
            ));
        }
        if input.contains('\\') {
            return Err(invalid("pattern must not contain backslash"));
        }
        if input.contains('\0') {
            return Err(invalid("pattern must not contain NUL byte"));
        }

        let raw_segments: Vec<&str> = input.split('/').collect();
        if raw_segments.len() > MAX_SEGMENTS {
            return Err(invalid(format!("pattern exceeds {MAX_SEGMENTS} segments")));
        }

        // Each segment expands to one-or-more brace-free strings; the pattern's
        // alternatives are the cartesian product across segments.
        let mut alternatives: Vec<Vec<Segment>> = vec![Vec::new()];
        for raw in raw_segments {
            if raw.is_empty() {
                return Err(invalid(
                    "pattern must not contain empty segments (double slash or trailing slash)",
                ));
            }
            if raw == "." || raw == ".." {
                return Err(invalid(
                    "pattern must not contain '.' or '..' segments; no key can contain them",
                ));
            }

            let expansions = expand_braces(raw)?;
            if alternatives.len().saturating_mul(expansions.len()) > MAX_ALTERNATIVES {
                return Err(invalid(format!(
                    "pattern expands to more than {MAX_ALTERNATIVES} brace alternatives"
                )));
            }

            let mut next = Vec::with_capacity(alternatives.len() * expansions.len());
            for prefix in &alternatives {
                for expansion in &expansions {
                    let mut extended = prefix.clone();
                    extended.push(parse_segment(expansion)?);
                    next.push(extended);
                }
            }
            alternatives = next;
        }

        Ok(Self {
            source: input.to_string(),
            alternatives,
        })
    }

    /// Returns whether `key` matches this pattern.
    ///
    /// `key` is a knowledge-base-relative object key with no leading slash.
    pub fn matches(&self, key: &str) -> bool {
        let segments: Vec<&str> = key.split('/').collect();
        self.alternatives
            .iter()
            .any(|alternative| match_segments(alternative, &segments))
    }

    /// The pattern exactly as written in the manifest.
    pub fn as_str(&self) -> &str {
        &self.source
    }

    /// The longest literal prefix every alternative of this pattern shares.
    ///
    /// Used to push a grant's narrowing down into the storage backend: a rule
    /// scoped to `public/**` should make the backend list `public/` rather than
    /// scan the whole knowledge base so the caller can throw most of it away.
    /// Returns `None` when the pattern starts with a wildcard.
    pub fn literal_prefix(&self) -> Option<&str> {
        let mut shortest: Option<&str> = None;
        for alternative in &self.alternatives {
            let Some(Segment::Literal(first)) = alternative.first() else {
                return None;
            };
            // Only a whole leading literal segment is usable: a partial segment
            // prefix would need the backend to match mid-segment, which S3's
            // prefix does do — but `Wild`/`DoubleStar` segments make the bound
            // unsound, so keep it to the simple, always-correct case.
            shortest = Some(match shortest {
                None => first.as_str(),
                Some(existing) if existing == first.as_str() => existing,
                Some(_) => return None,
            });
        }
        shortest
    }
}

/// Match one brace-free alternative against a key's segments.
///
/// Two-pointer greedy wildcard matching with a single remembered backtrack
/// point, the same shape as the classic iterative glob algorithm. No recursion,
/// so there is no stack depth or catastrophic-backtracking behaviour to bound.
fn match_segments(pattern: &[Segment], key: &[&str]) -> bool {
    let (mut p, mut k) = (0_usize, 0_usize);
    // Where to resume from if a `**` guess turns out to be too short.
    let mut star_p: Option<usize> = None;
    let mut star_k = 0_usize;

    while k < key.len() {
        match pattern.get(p) {
            Some(Segment::DoubleStar) => {
                // Try consuming nothing first, and remember that we can come
                // back and let this `**` swallow one more segment.
                star_p = Some(p);
                star_k = k;
                p += 1;
            }
            Some(segment) if segment_matches(segment, key[k]) => {
                p += 1;
                k += 1;
            }
            _ => {
                let Some(resume) = star_p else { return false };
                p = resume + 1;
                star_k += 1;
                k = star_k;
            }
        }
    }

    // Trailing `**`s may match zero segments; nothing else may.
    pattern[p..]
        .iter()
        .all(|segment| matches!(segment, Segment::DoubleStar))
}

fn segment_matches(segment: &Segment, candidate: &str) -> bool {
    match segment {
        // Reached only via `pattern[p..]` above; a `**` never compares here.
        Segment::DoubleStar => true,
        Segment::Literal(literal) => literal == candidate,
        Segment::Wild(tokens) => match_tokens(tokens, candidate),
    }
}

/// Match a wildcard segment against one key segment, same two-pointer shape.
fn match_tokens(tokens: &[Token], candidate: &str) -> bool {
    let chars: Vec<char> = candidate.chars().collect();
    let (mut t, mut c) = (0_usize, 0_usize);
    let mut star_t: Option<usize> = None;
    let mut star_c = 0_usize;

    while c < chars.len() {
        match tokens.get(t) {
            Some(Token::Star) => {
                star_t = Some(t);
                star_c = c;
                t += 1;
            }
            Some(Token::Any) => {
                t += 1;
                c += 1;
            }
            Some(Token::Literal(literal)) if literal_at(&chars, c, literal) => {
                t += 1;
                c += literal.chars().count();
            }
            _ => {
                let Some(resume) = star_t else { return false };
                t = resume + 1;
                star_c += 1;
                c = star_c;
            }
        }
    }

    tokens[t..].iter().all(|token| matches!(token, Token::Star))
}

/// Whether `literal` appears in `chars` starting at `at`.
fn literal_at(chars: &[char], at: usize, literal: &str) -> bool {
    let mut index = at;
    for expected in literal.chars() {
        if chars.get(index) != Some(&expected) {
            return false;
        }
        index += 1;
    }
    true
}

/// Split one segment into tokens, rejecting a malformed `**`.
fn parse_segment(segment: &str) -> Result<Segment, Error> {
    if segment == "**" {
        return Ok(Segment::DoubleStar);
    }
    if segment.contains("**") {
        return Err(invalid(format!(
            "'**' must be a whole segment; found it inside '{segment}'"
        )));
    }
    if !segment.contains(['*', '?']) {
        return Ok(Segment::Literal(segment.to_string()));
    }

    let mut tokens = Vec::new();
    let mut literal = String::new();
    for ch in segment.chars() {
        match ch {
            '*' | '?' => {
                if !literal.is_empty() {
                    tokens.push(Token::Literal(std::mem::take(&mut literal)));
                }
                tokens.push(if ch == '*' { Token::Star } else { Token::Any });
            }
            other => literal.push(other),
        }
    }
    if !literal.is_empty() {
        tokens.push(Token::Literal(literal));
    }
    Ok(Segment::Wild(tokens))
}

/// Expand `{a,b}` groups in one segment into every brace-free combination.
fn expand_braces(segment: &str) -> Result<Vec<String>, Error> {
    if !segment.contains('{') {
        if segment.contains('}') {
            return Err(invalid(format!(
                "'}}' without a matching '{{' in '{segment}'"
            )));
        }
        return Ok(vec![segment.to_string()]);
    }

    let mut results = vec![String::new()];
    let mut rest = segment;
    while let Some(open) = rest.find('{') {
        let (literal, tail) = rest.split_at(open);
        let inner_and_rest = &tail[1..];
        let Some(close) = inner_and_rest.find('}') else {
            return Err(invalid(format!("unclosed '{{' in '{segment}'")));
        };
        let inner = &inner_and_rest[..close];
        if inner.contains('{') {
            return Err(invalid(format!(
                "nested braces are not supported: '{segment}'"
            )));
        }
        if inner.contains("**") {
            return Err(invalid(format!(
                "'**' must be a whole segment and cannot appear inside braces: '{segment}'"
            )));
        }

        let branches: Vec<&str> = inner.split(',').collect();
        if branches.iter().any(|branch| branch.is_empty()) {
            return Err(invalid(format!(
                "brace alternation must not contain an empty branch: '{segment}'"
            )));
        }
        if results.len().saturating_mul(branches.len()) > MAX_ALTERNATIVES {
            return Err(invalid(format!(
                "pattern expands to more than {MAX_ALTERNATIVES} brace alternatives"
            )));
        }

        let mut next = Vec::with_capacity(results.len() * branches.len());
        for prefix in &results {
            for branch in &branches {
                next.push(format!("{prefix}{literal}{branch}"));
            }
        }
        results = next;
        rest = &inner_and_rest[close + 1..];
    }

    if rest.contains('}') {
        return Err(invalid(format!(
            "'}}' without a matching '{{' in '{segment}'"
        )));
    }
    for result in &mut results {
        result.push_str(rest);
    }
    Ok(results)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::InvalidInput {
        message: message.into(),
    }
}

impl fmt::Display for KeyPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.source.fmt(f)
    }
}

impl TryFrom<&str> for KeyPattern {
    type Error = Error;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for KeyPattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.source)
    }
}

impl<'de> Deserialize<'de> for KeyPattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let source = String::deserialize(deserializer)?;
        Self::parse(&source).map_err(serde::de::Error::custom)
    }
}
