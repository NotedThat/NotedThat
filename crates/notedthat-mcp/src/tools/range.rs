//! The range a `read` or `edit` call names, parsed once from the four flat
//! arguments both tools publish (#299).
//!
//! On the wire a range stays `line_start`/`line_end`/`byte_start`/`byte_end`,
//! four optional numbers: a flat input schema every MCP host accepts, where an
//! `anyOf` of shapes is refused by some. These types are what those numbers
//! mean. Each tool deserializes its arguments through a raw twin and
//! [`EditSpan::parse`] or [`ReadSpan::parse`], so an arguments value that
//! exists names a range the tool can send, and a call naming anything else is
//! refused before the tool runs, with the rule it broke.

const MIXED: &str =
    "line_* and byte_* arguments are mutually exclusive; provide one pair or the other";
const LINE_END_ALONE: &str = "line_end requires line_start; provide both or omit both";
const BYTE_END_ALONE: &str = "byte_end requires byte_start; provide both or omit both";
const LINE_ZERO: &str = "line numbers are 1-based; line_start must be >= 1";
const LINES_REVERSED: &str =
    "line_start must be <= line_end + 1 (set line_end = line_start - 1 for insert)";

/// One start/end pair as the caller sent it.
type Pair = (Option<u64>, Option<u64>);

/// The four range arguments as the caller sent them.
#[derive(Debug, Clone, Copy, Default)]
pub struct RangeArgs {
    pub line_start: Option<u64>,
    pub line_end: Option<u64>,
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
}

impl RangeArgs {
    /// The line pair and the byte pair, the shape both parsers match on.
    fn pairs(self) -> (Pair, Pair) {
        (
            (self.line_start, self.line_end),
            (self.byte_start, self.byte_end),
        )
    }
}

/// What an `edit` replaces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditSpan {
    /// Lines `start..=end`, 1-based; `end == start - 1` is the insert point
    /// before line `start`.
    Lines { start: u64, end: u64 },
    /// Bytes `start..end`, 0-based and never empty: the PATCH byte-range wire
    /// contract cannot spell a zero-width range.
    Bytes { start: u64, end: u64 },
}

impl EditSpan {
    /// An `edit` names exactly one complete pair.
    pub fn parse(args: RangeArgs) -> Result<Self, String> {
        match args.pairs() {
            ((Some(start), Some(end)), (None, None)) => {
                check_lines(start, end)?;
                Ok(Self::Lines { start, end })
            }
            ((None, None), (Some(start), Some(end))) if start >= end => Err(
                "byte_start must be strictly less than byte_end; byte-mode insert (zero-width range) is not supported in v1 — the PATCH byte-range wire contract cannot represent it".into(),
            ),
            ((None, None), (Some(start), Some(end))) => Ok(Self::Bytes { start, end }),
            ((None, None), (None, None)) => Err(
                "edit requires either (line_start, line_end) or (byte_start, byte_end); use append for EOF-only writes".into(),
            ),
            ((Some(_), None), (None, None)) => {
                Err("line_start requires line_end; provide both or omit both".into())
            }
            ((None, Some(_)), (None, None)) => Err(LINE_END_ALONE.into()),
            ((None, None), (Some(_), None)) => {
                Err("byte_start requires byte_end; provide both or omit both".into())
            }
            ((None, None), (None, Some(_))) => Err(BYTE_END_ALONE.into()),
            // Every shape left names at least one line and one byte argument.
            (_, _) => Err(MIXED.into()),
        }
    }

    /// The PATCH `Content-Range`; the byte form's end is inclusive.
    pub fn content_range(self) -> String {
        match self {
            Self::Lines { start, end } => format!("lines {start}-{end}/*"),
            Self::Bytes { start, end } => format!("bytes {start}-{}/*", end - 1),
        }
    }
}

/// What a `read` fetches. A missing end reads to the end of the object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadSpan {
    /// The whole object.
    Whole,
    /// Lines from `start` (1-based) through `end` inclusive; `end == start - 1`
    /// is the insert point before line `start`.
    Lines { start: u64, end: Option<u64> },
    /// Bytes from `start` (0-based) up to `end` exclusive, never empty.
    Bytes { start: u64, end: Option<u64> },
}

impl ReadSpan {
    /// A `read` names at most one pair, whose end may be omitted.
    pub fn parse(args: RangeArgs) -> Result<Self, String> {
        match args.pairs() {
            ((None, None), (None, None)) => Ok(Self::Whole),
            ((Some(start), end), (None, None)) => {
                match end {
                    Some(end) => check_lines(start, end)?,
                    None => check_line_start(start)?,
                }
                Ok(Self::Lines { start, end })
            }
            ((None, Some(_)), (None, None)) => Err(LINE_END_ALONE.into()),
            ((None, None), (Some(start), Some(end))) if start >= end => Err(format!(
                "byte_start ({start}) must be less than byte_end ({end})"
            )),
            ((None, None), (Some(start), end)) => Ok(Self::Bytes { start, end }),
            ((None, None), (None, Some(_))) => Err(BYTE_END_ALONE.into()),
            // Every shape left names at least one line and one byte argument.
            (_, _) => Err(MIXED.into()),
        }
    }

    /// The GET `Range` header, `None` for the whole object; the byte form's
    /// end is inclusive.
    pub fn range_header(self) -> Option<String> {
        match self {
            Self::Whole => None,
            Self::Lines { start, end: None } => Some(format!("lines={start}-")),
            Self::Lines {
                start,
                end: Some(end),
            } => Some(format!("lines={start}-{end}")),
            Self::Bytes { start, end: None } => Some(format!("bytes={start}-")),
            Self::Bytes {
                start,
                end: Some(end),
            } => Some(format!("bytes={start}-{}", end - 1)),
        }
    }
}

fn check_line_start(start: u64) -> Result<(), String> {
    if start == 0 {
        return Err(LINE_ZERO.into());
    }
    Ok(())
}

/// A line range is 1-based and ascending, or the insert point `end == start - 1`.
fn check_lines(start: u64, end: u64) -> Result<(), String> {
    check_line_start(start)?;
    if start > end.saturating_add(1) {
        return Err(LINES_REVERSED.into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(start: Option<u64>, end: Option<u64>) -> RangeArgs {
        RangeArgs {
            line_start: start,
            line_end: end,
            ..RangeArgs::default()
        }
    }

    fn bytes(start: Option<u64>, end: Option<u64>) -> RangeArgs {
        RangeArgs {
            byte_start: start,
            byte_end: end,
            ..RangeArgs::default()
        }
    }

    /// Every shape that names at least one line and one byte argument.
    fn mixed_shapes() -> Vec<RangeArgs> {
        let halves = [(Some(1), None), (None, Some(2)), (Some(1), Some(2))];
        halves
            .iter()
            .flat_map(|&(ls, le)| {
                halves.iter().map(move |&(bs, be)| RangeArgs {
                    line_start: ls,
                    line_end: le,
                    byte_start: bs,
                    byte_end: be,
                })
            })
            .collect()
    }

    #[test]
    fn edit_accepts_a_line_range_an_insert_point_and_a_byte_range() {
        assert_eq!(
            EditSpan::parse(lines(Some(2), Some(3))),
            Ok(EditSpan::Lines { start: 2, end: 3 })
        );
        assert_eq!(
            EditSpan::parse(lines(Some(5), Some(4))),
            Ok(EditSpan::Lines { start: 5, end: 4 })
        );
        assert_eq!(
            EditSpan::parse(bytes(Some(100), Some(200))),
            Ok(EditSpan::Bytes {
                start: 100,
                end: 200
            })
        );
    }

    #[test]
    fn edit_refuses_each_invalid_shape_with_its_rule() {
        let cases = [
            (
                RangeArgs::default(),
                "edit requires either (line_start, line_end) or (byte_start, byte_end); use append for EOF-only writes",
            ),
            (
                lines(Some(1), None),
                "line_start requires line_end; provide both or omit both",
            ),
            (lines(None, Some(1)), LINE_END_ALONE),
            (
                bytes(Some(1), None),
                "byte_start requires byte_end; provide both or omit both",
            ),
            (bytes(None, Some(1)), BYTE_END_ALONE),
            (lines(Some(0), Some(0)), LINE_ZERO),
            (lines(Some(5), Some(3)), LINES_REVERSED),
            (
                bytes(Some(100), Some(100)),
                "byte_start must be strictly less than byte_end; byte-mode insert (zero-width range) is not supported in v1 — the PATCH byte-range wire contract cannot represent it",
            ),
            (
                bytes(Some(200), Some(100)),
                "byte_start must be strictly less than byte_end; byte-mode insert (zero-width range) is not supported in v1 — the PATCH byte-range wire contract cannot represent it",
            ),
        ];
        for (args, message) in cases {
            assert_eq!(EditSpan::parse(args), Err(message.into()), "{args:?}");
        }
        for args in mixed_shapes() {
            assert_eq!(EditSpan::parse(args), Err(MIXED.into()), "{args:?}");
        }
    }

    #[test]
    fn read_accepts_the_whole_object_and_open_or_closed_ranges() {
        let cases = [
            (RangeArgs::default(), ReadSpan::Whole),
            (
                lines(Some(3), None),
                ReadSpan::Lines {
                    start: 3,
                    end: None,
                },
            ),
            (
                lines(Some(1), Some(5)),
                ReadSpan::Lines {
                    start: 1,
                    end: Some(5),
                },
            ),
            (
                lines(Some(5), Some(4)),
                ReadSpan::Lines {
                    start: 5,
                    end: Some(4),
                },
            ),
            (
                bytes(Some(10), None),
                ReadSpan::Bytes {
                    start: 10,
                    end: None,
                },
            ),
            (
                bytes(Some(0), Some(10)),
                ReadSpan::Bytes {
                    start: 0,
                    end: Some(10),
                },
            ),
        ];
        for (args, span) in cases {
            assert_eq!(ReadSpan::parse(args), Ok(span), "{args:?}");
        }
    }

    #[test]
    fn read_refuses_each_invalid_shape_with_its_rule() {
        let cases = [
            (lines(None, Some(10)), LINE_END_ALONE.to_owned()),
            (bytes(None, Some(100)), BYTE_END_ALONE.to_owned()),
            (lines(Some(0), None), LINE_ZERO.to_owned()),
            (lines(Some(0), Some(3)), LINE_ZERO.to_owned()),
            (lines(Some(5), Some(3)), LINES_REVERSED.to_owned()),
            (
                bytes(Some(10), Some(10)),
                "byte_start (10) must be less than byte_end (10)".to_owned(),
            ),
        ];
        for (args, message) in cases {
            assert_eq!(ReadSpan::parse(args), Err(message), "{args:?}");
        }
        for args in mixed_shapes() {
            assert_eq!(ReadSpan::parse(args), Err(MIXED.into()), "{args:?}");
        }
    }

    #[test]
    fn edit_content_range_makes_the_byte_end_inclusive() {
        assert_eq!(
            EditSpan::Bytes { start: 0, end: 10 }.content_range(),
            "bytes 0-9/*"
        );
        assert_eq!(
            EditSpan::Lines { start: 5, end: 4 }.content_range(),
            "lines 5-4/*"
        );
    }

    #[test]
    fn read_range_header_spells_open_and_closed_ranges() {
        let cases = [
            (ReadSpan::Whole, None),
            (
                ReadSpan::Lines {
                    start: 3,
                    end: None,
                },
                Some("lines=3-"),
            ),
            (
                ReadSpan::Lines {
                    start: 5,
                    end: Some(4),
                },
                Some("lines=5-4"),
            ),
            (
                ReadSpan::Bytes {
                    start: 10,
                    end: None,
                },
                Some("bytes=10-"),
            ),
            (
                ReadSpan::Bytes {
                    start: 0,
                    end: Some(10),
                },
                Some("bytes=0-9"),
            ),
        ];
        for (span, header) in cases {
            assert_eq!(span.range_header().as_deref(), header, "{span:?}");
        }
    }
}
