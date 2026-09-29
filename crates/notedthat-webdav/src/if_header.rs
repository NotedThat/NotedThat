//! The RFC 4918 §10.4 `If` request header.
//!
//! `If` carries one or more lists of conditions. Each list is either untagged, applying
//! to the request URI, or tagged with the resource it applies to. A condition is an
//! entity tag in brackets or a state token (a lock token) in angle brackets, either of
//! which may be negated with `Not`. The header is true when at least one list is true,
//! and a list is true when every one of its conditions is.
//!
//! `NotedThat` has no locks (D17), so no state token is ever current: a state-token
//! condition is false, and `Not <token>` true. A list that asserts a lock token
//! therefore fails and the request answers `412`, which is what RFC 4918 §10.4.1 asks
//! of a server that cannot honour the condition.

/// One condition inside a list.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Condition {
    /// `[etag]`, compared strongly against the resource's current `ETag`.
    ETag(String),
    /// `<state-token>`, a lock token; never current here.
    StateToken,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    not: bool,
    condition: Condition,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct List {
    /// The resource tag, or `None` for an untagged list (the request URI).
    resource: Option<String>,
    terms: Vec<Term>,
}

/// A parsed `If` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IfHeader {
    lists: Vec<List>,
}

/// The `If` header could not be parsed; the request answers `400`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Malformed;

impl IfHeader {
    /// Read the request's `If` header, if any.
    ///
    /// # Errors
    ///
    /// [`Malformed`] when the header is repeated, is not UTF-8, or does not follow the
    /// RFC 4918 §10.4.2 grammar.
    pub(crate) fn from_headers(headers: &axum::http::HeaderMap) -> Result<Option<Self>, Malformed> {
        let mut values = headers.get_all("if").iter();
        let Some(value) = values.next() else {
            return Ok(None);
        };
        if values.next().is_some() {
            return Err(Malformed);
        }
        let value = value.to_str().map_err(|_| Malformed)?;
        Self::parse(value).map(Some)
    }

    /// Parse an `If` header value.
    ///
    /// # Errors
    ///
    /// [`Malformed`] when `value` does not follow the RFC 4918 §10.4.2 grammar, or mixes
    /// tagged and untagged lists.
    pub(crate) fn parse(value: &str) -> Result<Self, Malformed> {
        let mut parser = Parser { rest: value };
        let mut lists = Vec::new();
        let mut resource: Option<String> = None;
        let mut tagged = None;
        loop {
            parser.skip_whitespace();
            match parser.peek() {
                None => break,
                Some('<') => {
                    if tagged == Some(false) {
                        return Err(Malformed);
                    }
                    tagged = Some(true);
                    resource = Some(parser.angle_bracketed()?.to_string());
                    // A resource tag must be followed by at least one list.
                    parser.skip_whitespace();
                    if parser.peek() != Some('(') {
                        return Err(Malformed);
                    }
                }
                Some('(') => {
                    if tagged.is_none() {
                        tagged = Some(false);
                    }
                    lists.push(List {
                        resource: resource.clone(),
                        terms: parser.list()?,
                    });
                }
                Some(_) => return Err(Malformed),
            }
        }
        if lists.is_empty() {
            return Err(Malformed);
        }
        Ok(Self { lists })
    }

    /// The resource tags the header names, for a caller that must look them up.
    pub(crate) fn resources(&self) -> impl Iterator<Item = &str> {
        self.lists
            .iter()
            .filter_map(|list| list.resource.as_deref())
    }

    /// Evaluate the header.
    ///
    /// `current_etag` answers with the current `ETag` of a resource: `None` as its
    /// argument is the request URI, `Some(tag)` a resource tag. It returns `None` for a
    /// resource that does not exist, whose entity-tag conditions are then false.
    pub(crate) fn evaluate<'a>(
        &self,
        current_etag: impl Fn(Option<&str>) -> Option<&'a str>,
    ) -> bool {
        self.lists.iter().any(|list| {
            let etag = current_etag(list.resource.as_deref());
            list.terms.iter().all(|term| {
                let holds = match &term.condition {
                    Condition::ETag(expected) => etag.is_some_and(|current| {
                        !current.starts_with("W/") && current == expected.as_str()
                    }),
                    Condition::StateToken => false,
                };
                holds != term.not
            })
        })
    }
}

struct Parser<'a> {
    rest: &'a str,
}

impl<'a> Parser<'a> {
    fn skip_whitespace(&mut self) {
        self.rest = self.rest.trim_start_matches([' ', '\t', '\r', '\n']);
    }

    fn peek(&self) -> Option<char> {
        self.rest.chars().next()
    }

    /// Consume `open ... close` and return what is between.
    fn delimited(&mut self, open: char, close: char) -> Result<&'a str, Malformed> {
        let body = self.rest.strip_prefix(open).ok_or(Malformed)?;
        let end = body.find(close).ok_or(Malformed)?;
        let (inner, after) = body.split_at(end);
        self.rest = &after[close.len_utf8()..];
        Ok(inner)
    }

    fn angle_bracketed(&mut self) -> Result<&'a str, Malformed> {
        let inner = self.delimited('<', '>')?;
        if inner.is_empty() || inner.contains(char::is_whitespace) {
            return Err(Malformed);
        }
        Ok(inner)
    }

    /// `"(" 1*Condition ")"`
    fn list(&mut self) -> Result<Vec<Term>, Malformed> {
        self.rest = self.rest.strip_prefix('(').ok_or(Malformed)?;
        let mut terms = Vec::new();
        loop {
            self.skip_whitespace();
            if let Some(after) = self.rest.strip_prefix(')') {
                self.rest = after;
                break;
            }
            let not = match self.rest.get(..3) {
                Some(word) if word.eq_ignore_ascii_case("not") => {
                    self.rest = &self.rest[3..];
                    self.skip_whitespace();
                    true
                }
                _ => false,
            };
            let condition = match self.peek() {
                Some('<') => {
                    self.angle_bracketed()?;
                    Condition::StateToken
                }
                Some('[') => {
                    let etag = self.delimited('[', ']')?.trim();
                    if etag.is_empty() {
                        return Err(Malformed);
                    }
                    Condition::ETag(etag.to_string())
                }
                _ => return Err(Malformed),
            };
            terms.push(Term { not, condition });
        }
        if terms.is_empty() {
            return Err(Malformed);
        }
        Ok(terms)
    }
}

#[cfg(test)]
mod tests {
    use super::{IfHeader, Malformed};

    const CURRENT: &str = "\"v1\"";

    fn untagged(value: &str) -> bool {
        IfHeader::parse(value)
            .expect("parses")
            .evaluate(|_| Some(CURRENT))
    }

    #[test]
    fn an_entity_tag_list_compares_strongly() {
        assert!(untagged("([\"v1\"])"));
        assert!(!untagged("([\"v0\"])"));
        assert!(!untagged("([W/\"v1\"])"));
        assert!(untagged("(Not [\"v0\"])"));
    }

    #[test]
    fn a_lock_token_never_holds_because_there_are_no_locks() {
        assert!(!untagged(
            "(<opaquelocktoken:a515cfa4-5da4-22e1-f5bf-00a0451e6bf7>)"
        ));
        assert!(!untagged("(<urn:uuid:x> [\"v1\"])"));
        assert!(untagged("(Not <DAV:no-lock>)"));
    }

    #[test]
    fn any_list_may_hold() {
        assert!(untagged("([\"v0\"]) ([\"v1\"])"));
        assert!(!untagged("([\"v0\"]) (<urn:uuid:x>)"));
    }

    #[test]
    fn a_missing_resource_fails_its_entity_tag_conditions() {
        let header = IfHeader::parse("([\"v1\"])").expect("parses");
        assert!(!header.evaluate(|_| None));
        let header = IfHeader::parse("(Not [\"v1\"])").expect("parses");
        assert!(header.evaluate(|_| None));
    }

    #[test]
    fn tagged_lists_apply_to_their_resource() {
        let header =
            IfHeader::parse("</webdav/notes/a.md> ([\"v1\"]) </webdav/notes/b.md> ([\"v9\"])")
                .expect("parses");
        assert_eq!(
            header.resources().collect::<Vec<_>>(),
            ["/webdav/notes/a.md", "/webdav/notes/b.md"]
        );
        let etag_of = |resource: Option<&str>| match resource {
            Some("/webdav/notes/a.md") => Some("\"v0\""),
            Some("/webdav/notes/b.md") => Some("\"v9\""),
            _ => None,
        };
        assert!(header.evaluate(etag_of));
        assert!(!header.evaluate(|_| Some("\"v0\"")));
    }

    #[test]
    fn malformed_headers_are_refused() {
        for value in [
            "",
            "[\"v1\"]",
            "(",
            "()",
            "([\"v1\"]",
            "([])",
            "(Nope [\"v1\"])",
            "</a> ",
            "([\"v1\"]) </a> ([\"v1\"])",
            "(<>)",
        ] {
            assert_eq!(IfHeader::parse(value), Err(Malformed), "{value:?}");
        }
    }

    #[test]
    fn the_header_is_read_once() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(IfHeader::from_headers(&headers), Ok(None));
        headers.append("if", "([\"v1\"])".parse().expect("value"));
        assert!(IfHeader::from_headers(&headers).expect("parses").is_some());
        headers.append("if", "([\"v1\"])".parse().expect("value"));
        assert_eq!(IfHeader::from_headers(&headers), Err(Malformed));
    }
}
