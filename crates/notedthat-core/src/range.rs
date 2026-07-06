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

/// Per-request line index built from raw bytes.
/// `line_starts[i]` is the byte offset at which line `i+1` begins (0-indexed into the array).
/// `line_starts[0] = 0` always.
#[derive(Debug, Clone)]
pub struct LineIndex {
    /// Byte offset at which each line begins. Length equals `total_lines`.
    pub line_starts: Vec<u64>,
    /// Total number of lines (including trailing partial line without a `\n`).
    pub total_lines: u64,
    /// Total number of bytes in the buffer.
    pub total_bytes: u64,
}

impl LineIndex {
    /// Build a line index from raw bytes without decoding text.
    pub fn from_bytes(bytes: &[u8]) -> Self {
        if bytes.is_empty() {
            return Self {
                line_starts: Vec::new(),
                total_lines: 0,
                total_bytes: 0,
            };
        }

        let mut line_starts = vec![0];
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'\r' && i + 1 < bytes.len() && bytes[i + 1] == b'\n' {
                i += 2;
                if i < bytes.len() {
                    line_starts.push(i as u64);
                }
            } else if bytes[i] == b'\n' {
                i += 1;
                if i < bytes.len() {
                    line_starts.push(i as u64);
                }
            } else {
                i += 1;
            }
        }

        Self {
            total_lines: line_starts.len() as u64,
            total_bytes: bytes.len() as u64,
            line_starts,
        }
    }

    /// Convert a line range into an exclusive byte range.
    pub fn byte_range(&self, line_range: &LineRange) -> Option<Range<u64>> {
        match *line_range {
            LineRange::FromStart { first, last } => {
                if first == 0 || first > self.total_lines || first > last.saturating_add(1) {
                    return None;
                }

                let clamped_last = last.min(self.total_lines);
                let start = self.line_start(first - 1)?;
                let end = if clamped_last == self.total_lines {
                    self.total_bytes
                } else {
                    self.line_start(clamped_last)?
                };
                Some(start..end)
            }
            LineRange::FromStartOpen { first } => {
                if first == 0 || first > self.total_lines {
                    return None;
                }

                let start = self.line_start(first - 1)?;
                Some(start..self.total_bytes)
            }
            LineRange::Suffix { length } => {
                if length == 0 || self.total_lines == 0 {
                    return None;
                }

                let clamped = length.min(self.total_lines);
                let start_line = self.total_lines - clamped;
                let start = self.line_start(start_line)?;
                Some(start..self.total_bytes)
            }
            LineRange::Insert { before } => {
                if before == 0 || before > self.total_lines + 1 {
                    return None;
                }

                if before == self.total_lines + 1 {
                    return Some(self.total_bytes..self.total_bytes);
                }

                let offset = self.line_start(before - 1)?;
                Some(offset..offset)
            }
        }
    }

    fn line_start(&self, zero_based_line: u64) -> Option<u64> {
        let index = usize::try_from(zero_based_line).ok()?;
        self.line_starts.get(index).copied()
    }

    /// Return the `Content-Range` header value for a line-mode response.
    pub fn content_range_string(&self, line_range: &LineRange) -> String {
        match *line_range {
            LineRange::FromStart { first, last } => {
                let clamped_last = last.min(self.total_lines);
                format!("lines {first}-{clamped_last}/{}", self.total_lines)
            }
            LineRange::FromStartOpen { first } => {
                format!("lines {first}-{}/{}", self.total_lines, self.total_lines)
            }
            LineRange::Suffix { length } => {
                let actual_first = self
                    .total_lines
                    .saturating_sub(length.min(self.total_lines))
                    + 1;
                format!(
                    "lines {actual_first}-{}/{}",
                    self.total_lines, self.total_lines
                )
            }
            LineRange::Insert { before } => {
                format!("lines {before}-{}/{}", before - 1, self.total_lines)
            }
        }
    }
}

/// Parsed `Range:` header. Unit is preserved so callers can ignore non-`bytes` units per RFC 7233 §2.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedRanges {
    /// Range unit token, such as `bytes`.
    pub unit: String,
    /// Parsed byte ranges. Empty when `unit` is not `bytes`.
    pub ranges: Vec<ByteRange>,
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

/// Parse a single `lines=` `Range:` header value into a line range.
///
/// # Errors
///
/// Returns [`RangeParseError`] when the value is empty, misses `=`, has no
/// range set, uses unsupported multi-range line syntax, or contains an invalid
/// line range component.
pub fn parse_line_range_header(value: &str) -> Result<LineRange, RangeParseError> {
    if value.is_empty() {
        return Err(RangeParseError::Empty);
    }

    let (_unit, range_set) = value
        .split_once('=')
        .ok_or(RangeParseError::MissingEquals)?;
    if range_set.is_empty() {
        return Err(RangeParseError::NoRanges);
    }
    if range_set.contains(',') {
        return Err(RangeParseError::InvalidSpec(
            "multi-range lines= not supported".into(),
        ));
    }

    let (start, end) = range_set
        .split_once('-')
        .ok_or_else(|| RangeParseError::InvalidSpec(range_set.to_string()))?;

    if start.is_empty() && end.is_empty() {
        return Err(RangeParseError::InvalidSpec(range_set.to_string()));
    }

    if start.is_empty() {
        let length = parse_u64(end, range_set)?;
        return Ok(LineRange::Suffix { length });
    }

    let first = parse_u64(start, range_set)?;
    if end.is_empty() {
        return Ok(LineRange::FromStartOpen { first });
    }

    let last = parse_u64(end, range_set)?;
    if first == 0 {
        return Err(RangeParseError::InvalidSpec(range_set.to_string()));
    }
    if last == first - 1 {
        Ok(LineRange::Insert { before: first })
    } else {
        Ok(LineRange::FromStart { first, last })
    }
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

    mod line_index {
        use super::*;

        fn ten_line_index() -> LineIndex {
            LineIndex::from_bytes(b"1\n2\n3\n4\n5\n6\n7\n8\n9\n10")
        }

        #[test]
        fn empty_buffer_has_no_lines() {
            let index = LineIndex::from_bytes(b"");

            assert_eq!(index.total_lines, 0);
            assert_eq!(index.line_starts, Vec::<u64>::new());
            assert_eq!(index.total_bytes, 0);
            assert_eq!(
                index.byte_range(&LineRange::FromStart { first: 1, last: 1 }),
                None
            );
        }

        #[test]
        fn trailing_newline_belongs_to_last_line() {
            let index = LineIndex::from_bytes(b"one\n");

            assert_eq!(index.total_lines, 1);
            assert_eq!(index.line_starts, vec![0]);
            assert_eq!(index.total_bytes, 4);
            assert_eq!(
                index.byte_range(&LineRange::FromStart { first: 1, last: 1 }),
                Some(0..4)
            );
        }

        #[test]
        fn trailing_partial_line_is_counted() {
            let index = LineIndex::from_bytes(b"one");

            assert_eq!(index.total_lines, 1);
            assert_eq!(index.line_starts, vec![0]);
            assert_eq!(index.total_bytes, 3);
            assert_eq!(
                index.byte_range(&LineRange::FromStart { first: 1, last: 1 }),
                Some(0..3)
            );
        }

        #[test]
        fn crlf_is_one_line_boundary() {
            let index = LineIndex::from_bytes(b"a\r\nb\n");

            assert_eq!(index.total_lines, 2);
            assert_eq!(index.line_starts, vec![0, 3]);
            assert_eq!(index.total_bytes, 5);
        }

        #[test]
        fn from_start_clamps_past_eof_end() {
            let index = ten_line_index();

            assert_eq!(
                index.byte_range(&LineRange::FromStart {
                    first: 1,
                    last: 999,
                }),
                Some(0..index.total_bytes)
            );
        }

        #[test]
        fn from_start_past_eof_is_unsatisfiable() {
            let index = ten_line_index();

            assert_eq!(
                index.byte_range(&LineRange::FromStart {
                    first: 999,
                    last: 1000,
                }),
                None
            );
        }

        #[test]
        fn insert_before_existing_line_is_zero_width() {
            let index = ten_line_index();
            let offset = index.line_starts[4];

            assert_eq!(
                index.byte_range(&LineRange::Insert { before: 5 }),
                Some(offset..offset)
            );
        }

        #[test]
        fn content_range_strings_render_line_bounds() {
            let index = ten_line_index();

            assert_eq!(
                index.content_range_string(&LineRange::FromStart { first: 2, last: 4 }),
                "lines 2-4/10"
            );
            assert_eq!(
                index.content_range_string(&LineRange::Insert { before: 5 }),
                "lines 5-4/10"
            );
        }

        #[test]
        fn empty_file_insert_allows_position_one_only() {
            let index = LineIndex::from_bytes(b"");

            assert_eq!(
                index.byte_range(&LineRange::Insert { before: 1 }),
                Some(0..0)
            );
            assert_eq!(index.byte_range(&LineRange::Insert { before: 2 }), None);
        }

        #[test]
        fn lone_cr_and_bom_are_ordinary_bytes() {
            let lone_cr = LineIndex::from_bytes(b"a\rb");
            let bom = LineIndex::from_bytes(b"\xef\xbb\xbffoo\n");

            assert_eq!(lone_cr.total_lines, 1);
            assert_eq!(bom.total_lines, 1);
            assert_eq!(bom.line_starts, vec![0]);
        }
    }

    mod parse_line_range {
        use super::*;

        #[test]
        fn closed_range_parses_as_from_start() {
            assert_eq!(
                parse_line_range_header("lines=1-10"),
                Ok(LineRange::FromStart { first: 1, last: 10 })
            );
        }

        #[test]
        fn open_ended_range_parses_as_from_start_open() {
            assert_eq!(
                parse_line_range_header("lines=100-"),
                Ok(LineRange::FromStartOpen { first: 100 })
            );
        }

        #[test]
        fn suffix_range_parses_as_suffix() {
            assert_eq!(
                parse_line_range_header("lines=-5"),
                Ok(LineRange::Suffix { length: 5 })
            );
        }

        #[test]
        fn zero_width_range_parses_as_insert() {
            assert_eq!(
                parse_line_range_header("lines=5-4"),
                Ok(LineRange::Insert { before: 5 })
            );
        }

        #[test]
        fn single_line_range_is_not_insert() {
            assert_eq!(
                parse_line_range_header("lines=1-1"),
                Ok(LineRange::FromStart { first: 1, last: 1 })
            );
        }

        #[test]
        fn empty_range_set_is_no_ranges_error() {
            assert_eq!(
                parse_line_range_header("lines="),
                Err(RangeParseError::NoRanges)
            );
        }

        #[test]
        fn zero_start_is_invalid_spec() {
            assert_eq!(
                parse_line_range_header("lines=0-10"),
                Err(RangeParseError::InvalidSpec("0-10".into()))
            );
        }

        #[test]
        fn multi_range_is_invalid_spec() {
            assert_eq!(
                parse_line_range_header("lines=1-10,20-30"),
                Err(RangeParseError::InvalidSpec(
                    "multi-range lines= not supported".into()
                ))
            );
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
