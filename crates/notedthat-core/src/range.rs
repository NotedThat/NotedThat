//! RFC 7233 byte range parsing and conversion helpers.

use std::ops::Range;

/// RFC 7233 §2.1 byte range spec. All bounds are inclusive HTTP semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ByteRange {
    /// `bytes=first-last` (both bounds present, inclusive)
    FromStart {
        /// The first byte position (inclusive).
        first: u64,
        /// The last byte position (inclusive).
        last: u64,
    },
    /// `bytes=first-` (no upper bound; from first to end)
    FromStartOpen {
        /// The first byte position (inclusive).
        first: u64,
    },
    /// `bytes=-length` (last N bytes)
    Suffix {
        /// The number of bytes from the end.
        length: u64,
    },
}

/// Line range spec. All bounds are 1-based inclusive line semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineRange {
    /// `lines=first-last` (both bounds present, inclusive)
    FromStart {
        /// The first line number (inclusive, 1-based).
        first: u64,
        /// The last line number (inclusive, 1-based).
        last: u64,
    },
    /// `lines=first-` (no upper bound; from first to end)
    FromStartOpen {
        /// The first line number (inclusive, 1-based).
        first: u64,
    },
    /// `lines=-length` (last N lines)
    Suffix {
        /// The number of lines from the end.
        length: u64,
    },
    /// `lines=before-before-1` (zero-width insert at `before`)
    Insert {
        /// The line number to insert before (1-based).
        before: u64,
    },
}

/// Parsed `Range:` header. Unit is preserved so callers can ignore non-`bytes` units per RFC 7233 §2.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRanges {
    /// Range unit token, such as `bytes`.
    pub unit: String,
    /// Parsed byte ranges. Empty when `unit` is not `bytes`.
    pub ranges: Vec<ByteRange>,
}

/// Errors produced while parsing an RFC 7233 `Range:` header value.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RangeParseError {
    /// Header value is empty.
    #[error("empty Range header value")]
    Empty,
    /// Header value is missing the `=` separator between unit and range set.
    #[error("Range header missing '=' separator")]
    MissingEquals,
    /// Header value has no range specs after `=`.
    #[error("Range header has no range specs after '='")]
    NoRanges,
    /// A single range spec is syntactically invalid.
    #[error("invalid range spec: {0}")]
    InvalidSpec(String),
    /// A numeric range component does not fit in `u64`.
    #[error("range spec overflows u64")]
    OverflowU64,
}

impl ByteRange {
    /// Return this range in single-range HTTP header form.
    pub fn to_http_string(&self) -> String {
        match self {
            Self::FromStart { first, last } => format!("bytes={first}-{last}"),
            Self::FromStartOpen { first } => format!("bytes={first}-"),
            Self::Suffix { length } => format!("bytes=-{length}"),
        }
    }

    /// Convert inclusive HTTP range semantics into a zero-based exclusive Rust range for `total_size`.
    pub fn to_exclusive_range(&self, total_size: u64) -> Option<Range<u64>> {
        match *self {
            Self::FromStart { first, last } => {
                if first > last || first >= total_size {
                    return None;
                }

                let end_inclusive = last.min(total_size - 1);
                Some(first..end_inclusive + 1)
            }
            Self::FromStartOpen { first } => {
                if first >= total_size {
                    None
                } else {
                    Some(first..total_size)
                }
            }
            Self::Suffix { length } => {
                if length == 0 || total_size == 0 {
                    None
                } else {
                    Some(total_size.saturating_sub(length)..total_size)
                }
            }
        }
    }

    /// Return whether this range can be satisfied for `total_size`.
    pub fn is_satisfiable(&self, total_size: u64) -> bool {
        self.to_exclusive_range(total_size).is_some()
    }
}

impl LineRange {
    /// Return this range in single-range HTTP header form.
    pub fn to_http_string(&self) -> String {
        match self {
            Self::FromStart { first, last } => format!("lines={first}-{last}"),
            Self::FromStartOpen { first } => format!("lines={first}-"),
            Self::Suffix { length } => format!("lines=-{length}"),
            Self::Insert { before } => format!("lines={before}-{}", before - 1),
        }
    }
}

/// Parse an RFC 7233 §2.1 `Range:` header value.
pub fn parse_range_header(value: &str) -> Result<ParsedRanges, RangeParseError> {
    if value.is_empty() {
        return Err(RangeParseError::Empty);
    }

    let (unit, range_set) = value
        .split_once('=')
        .ok_or(RangeParseError::MissingEquals)?;
    if range_set.is_empty() {
        return Err(RangeParseError::NoRanges);
    }

    let unit = unit.to_string();
    if unit != "bytes" {
        return Ok(ParsedRanges {
            unit,
            ranges: Vec::new(),
        });
    }

    let ranges = range_set
        .split(',')
        .map(|spec| parse_byte_range_spec(spec.trim()))
        .collect::<Result<Vec<_>, _>>()?;

    Ok(ParsedRanges { unit, ranges })
}

fn parse_byte_range_spec(spec: &str) -> Result<ByteRange, RangeParseError> {
    let (start, end) = spec
        .split_once('-')
        .ok_or_else(|| RangeParseError::InvalidSpec(spec.to_string()))?;

    if end.contains('-') || (start.is_empty() && end.is_empty()) {
        return Err(RangeParseError::InvalidSpec(spec.to_string()));
    }

    if start.is_empty() {
        let length = parse_u64(end, spec)?;
        return Ok(ByteRange::Suffix { length });
    }

    let first = parse_u64(start, spec)?;
    if end.is_empty() {
        Ok(ByteRange::FromStartOpen { first })
    } else {
        let last = parse_u64(end, spec)?;
        Ok(ByteRange::FromStart { first, last })
    }
}

fn parse_u64(component: &str, spec: &str) -> Result<u64, RangeParseError> {
    if component.is_empty() || !component.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(RangeParseError::InvalidSpec(spec.to_string()));
    }

    component.parse().map_err(|_| RangeParseError::OverflowU64)
}

#[cfg(test)]
mod tests {
    use super::*;

    mod line_range {
        use super::*;

        #[test]
        fn constructs_each_variant() {
            assert_eq!(
                LineRange::FromStart { first: 1, last: 10 },
                LineRange::FromStart { first: 1, last: 10 }
            );
            assert_eq!(
                LineRange::FromStartOpen { first: 7 },
                LineRange::FromStartOpen { first: 7 }
            );
            assert_eq!(
                LineRange::Suffix { length: 3 },
                LineRange::Suffix { length: 3 }
            );
            assert_eq!(
                LineRange::Insert { before: 5 },
                LineRange::Insert { before: 5 }
            );
        }

        #[test]
        fn from_start_to_http_string() {
            assert_eq!(
                LineRange::FromStart { first: 1, last: 10 }.to_http_string(),
                "lines=1-10"
            );
        }

        #[test]
        fn insert_to_http_string() {
            assert_eq!(
                LineRange::Insert { before: 5 }.to_http_string(),
                "lines=5-4"
            );
        }

        #[test]
        fn suffix_to_http_string() {
            assert_eq!(LineRange::Suffix { length: 3 }.to_http_string(), "lines=-3");
        }

        #[test]
        fn is_send_sync_clone() {
            let _: fn() = || {
                fn f<T: Send + Sync + Clone>() {}
                f::<LineRange>();
            };
        }
    }

    #[test]
    fn parse_closed_range() {
        assert_eq!(
            parse_range_header("bytes=0-499"),
            Ok(ParsedRanges {
                unit: "bytes".into(),
                ranges: vec![ByteRange::FromStart {
                    first: 0,
                    last: 499,
                }],
            })
        );
    }

    #[test]
    fn parse_open_ended_range() {
        assert_eq!(
            parse_range_header("bytes=500-"),
            Ok(ParsedRanges {
                unit: "bytes".into(),
                ranges: vec![ByteRange::FromStartOpen { first: 500 }],
            })
        );
    }

    #[test]
    fn parse_suffix_range() {
        assert_eq!(
            parse_range_header("bytes=-500"),
            Ok(ParsedRanges {
                unit: "bytes".into(),
                ranges: vec![ByteRange::Suffix { length: 500 }],
            })
        );
    }

    #[test]
    fn parse_single_byte_range() {
        assert_eq!(
            parse_range_header("bytes=0-0"),
            Ok(ParsedRanges {
                unit: "bytes".into(),
                ranges: vec![ByteRange::FromStart { first: 0, last: 0 }],
            })
        );
    }

    #[test]
    fn parse_multiple_ranges() {
        let parsed = parse_range_header("bytes=0-499,1000-1499").unwrap();
        assert_eq!(parsed.unit, "bytes");
        assert_eq!(
            parsed.ranges,
            vec![
                ByteRange::FromStart {
                    first: 0,
                    last: 499,
                },
                ByteRange::FromStart {
                    first: 1000,
                    last: 1499,
                },
            ]
        );
    }

    #[test]
    fn parse_multiple_ranges_with_space_after_comma() {
        let parsed = parse_range_header("bytes=0-499, 1000-1499").unwrap();
        assert_eq!(parsed.unit, "bytes");
        assert_eq!(parsed.ranges.len(), 2);
    }

    #[test]
    fn parse_start_greater_than_end() {
        assert_eq!(
            parse_range_header("bytes=100-50"),
            Ok(ParsedRanges {
                unit: "bytes".into(),
                ranges: vec![ByteRange::FromStart {
                    first: 100,
                    last: 50,
                }],
            })
        );
    }

    #[test]
    fn parse_unknown_unit_as_empty_ranges() {
        assert_eq!(
            parse_range_header("items=0-10"),
            Ok(ParsedRanges {
                unit: "items".into(),
                ranges: vec![],
            })
        );
    }

    #[test]
    fn parse_empty_value_is_error() {
        assert_eq!(parse_range_header(""), Err(RangeParseError::Empty));
    }

    #[test]
    fn parse_missing_equals_is_error() {
        assert_eq!(
            parse_range_header("bytes"),
            Err(RangeParseError::MissingEquals)
        );
    }

    #[test]
    fn parse_no_ranges_is_error() {
        assert_eq!(parse_range_header("bytes="), Err(RangeParseError::NoRanges));
    }

    #[test]
    fn parse_alpha_spec_is_error() {
        assert_eq!(
            parse_range_header("bytes=abc"),
            Err(RangeParseError::InvalidSpec("abc".into()))
        );
    }

    #[test]
    fn parse_bare_dash_is_error() {
        assert_eq!(
            parse_range_header("bytes=-"),
            Err(RangeParseError::InvalidSpec("-".into()))
        );
    }

    #[test]
    fn parse_double_dash_is_error() {
        assert_eq!(
            parse_range_header("bytes=0--5"),
            Err(RangeParseError::InvalidSpec("0--5".into()))
        );
    }

    #[test]
    fn parse_overflow_is_error() {
        assert_eq!(
            parse_range_header("bytes=99999999999999999999999-"),
            Err(RangeParseError::OverflowU64)
        );
    }

    #[test]
    fn closed_range_converts_to_exclusive() {
        assert_eq!(
            ByteRange::FromStart {
                first: 0,
                last: 499,
            }
            .to_exclusive_range(1000),
            Some(0..500)
        );
    }

    #[test]
    fn closed_range_clamps_last_to_total_size() {
        assert_eq!(
            ByteRange::FromStart {
                first: 0,
                last: 999,
            }
            .to_exclusive_range(500),
            Some(0..500)
        );
    }

    #[test]
    fn closed_range_start_past_total_is_unsatisfiable() {
        assert_eq!(
            ByteRange::FromStart {
                first: 500,
                last: 999,
            }
            .to_exclusive_range(100),
            None
        );
    }

    #[test]
    fn open_range_converts_to_exclusive() {
        assert_eq!(
            ByteRange::FromStartOpen { first: 100 }.to_exclusive_range(500),
            Some(100..500)
        );
    }

    #[test]
    fn open_range_start_past_total_is_unsatisfiable() {
        assert_eq!(
            ByteRange::FromStartOpen { first: 500 }.to_exclusive_range(100),
            None
        );
    }

    #[test]
    fn suffix_range_converts_to_exclusive() {
        assert_eq!(
            ByteRange::Suffix { length: 100 }.to_exclusive_range(500),
            Some(400..500)
        );
    }

    #[test]
    fn suffix_range_clamps_to_total_size() {
        assert_eq!(
            ByteRange::Suffix { length: 9999 }.to_exclusive_range(100),
            Some(0..100)
        );
    }

    #[test]
    fn suffix_zero_is_unsatisfiable() {
        assert_eq!(
            ByteRange::Suffix { length: 0 }.to_exclusive_range(500),
            None
        );
    }

    #[test]
    fn is_satisfiable_true_when_exclusive_range_exists() {
        assert!(ByteRange::Suffix { length: 100 }.is_satisfiable(500));
    }

    #[test]
    fn is_satisfiable_false_when_exclusive_range_is_none() {
        assert!(!ByteRange::Suffix { length: 0 }.is_satisfiable(500));
    }

    #[test]
    fn closed_range_start_greater_than_end_is_unsatisfiable() {
        assert_eq!(
            ByteRange::FromStart {
                first: 100,
                last: 50,
            }
            .to_exclusive_range(1000),
            None
        );
    }

    #[test]
    fn closed_range_to_http_string() {
        assert_eq!(
            ByteRange::FromStart {
                first: 0,
                last: 499,
            }
            .to_http_string(),
            "bytes=0-499"
        );
    }

    #[test]
    fn open_range_to_http_string() {
        assert_eq!(
            ByteRange::FromStartOpen { first: 500 }.to_http_string(),
            "bytes=500-"
        );
    }

    #[test]
    fn suffix_range_to_http_string() {
        assert_eq!(
            ByteRange::Suffix { length: 500 }.to_http_string(),
            "bytes=-500"
        );
    }
}
