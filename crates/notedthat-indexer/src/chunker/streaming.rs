use std::io::{self, Read, Seek, SeekFrom};

use super::Chunk;

mod scan;

use scan::{Heading, HeadingScanner};

const HEADING_LABEL_CHAR_CAP: usize = 3_000;

struct Section {
    cursor: u64,
    end: u64,
    /// Heading paths of the sections merged into this span, in byte order. The
    /// first entry always starts at or before `cursor`.
    boundaries: Vec<Boundary>,
    lookahead: Vec<u8>,
}

struct Boundary {
    start: u64,
    heading_path: Vec<String>,
}

impl Section {
    fn new(start: u64, end: u64, heading_path: Vec<String>) -> Self {
        Self {
            cursor: start,
            end,
            boundaries: vec![Boundary {
                start,
                heading_path,
            }],
            lookahead: Vec::new(),
        }
    }

    /// The heading path of the merged section containing `cursor`.
    fn heading_path(&mut self) -> Vec<String> {
        let passed = self
            .boundaries
            .iter()
            .skip(1)
            .take_while(|boundary| boundary.start <= self.cursor)
            .count();
        self.boundaries.drain(..passed);
        self.boundaries[0].heading_path.clone()
    }
}

/// A bounded-memory iterator over chunks from a seekable Markdown reader.
///
/// A section shorter than the minimum chunk size is merged with the section
/// after it, and a short final chunk is merged into the chunk before it when
/// both fit. No chunk is only whitespace, no chunk exceeds the character
/// bound, and only the last chunk can be shorter than the minimum (D72).
pub struct ChunkIter<R> {
    reader: R,
    scanner: HeadingScanner,
    section: Option<Section>,
    next_heading: Option<Heading>,
    heading_path: Vec<String>,
    held: Option<Chunk>,
    max_chars: usize,
    min_chars: usize,
    max_read_bytes: usize,
    source_base_offset: usize,
    started: bool,
    finished: bool,
}

/// Creates a bounded-memory Markdown chunk iterator.
///
/// `source_base_offset` is added to every returned byte range, allowing the reader
/// to begin after an independently parsed prefix such as OKF frontmatter.
/// The minimum chunk size defaults to a quarter of `max_chars`; see
/// [`ChunkIter::with_min_chars`].
///
/// # Errors
///
/// Returns an [`InvalidInput`](io::ErrorKind::InvalidInput) error when `max_chars` is zero,
/// or so large that its byte bound overflows `usize`.
pub fn stream_chunks<R: Read + Seek>(
    reader: R,
    max_chars: usize,
    source_base_offset: usize,
) -> io::Result<ChunkIter<R>> {
    if max_chars == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk character bound must be greater than zero",
        ));
    }
    let max_read_bytes = max_chars.checked_mul(4).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "chunk character bound is too large",
        )
    })?;
    Ok(ChunkIter {
        reader,
        scanner: HeadingScanner::new(HEADING_LABEL_CHAR_CAP),
        section: None,
        next_heading: None,
        heading_path: Vec::new(),
        held: None,
        max_chars,
        min_chars: max_chars / 4,
        max_read_bytes,
        source_base_offset,
        started: false,
        finished: false,
    })
}

impl<R> ChunkIter<R> {
    /// Sets the minimum chunk size in characters, capped at a quarter of the
    /// character bound. Zero disables merging, so every heading-delimited
    /// section and soft-cap split is emitted as it is found.
    #[must_use]
    pub fn with_min_chars(mut self, min_chars: usize) -> Self {
        self.min_chars = min_chars.min(self.max_chars / 4);
        self
    }

    /// Returns the underlying reader after iteration or early termination.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: Read + Seek> ChunkIter<R> {
    /// Moves the heading path into `heading` and returns where its section starts.
    fn enter(&mut self, heading: Heading) -> u64 {
        let keep = heading.depth.saturating_sub(1);
        self.heading_path
            .truncate(keep.min(self.heading_path.len()));
        self.heading_path.push(heading.label);
        heading.start
    }

    /// Scans for the next heading and returns where the current section ends.
    fn scan_section_end(&mut self) -> io::Result<u64> {
        self.next_heading = self.scanner.next_heading(&mut self.reader)?;
        Ok(self
            .next_heading
            .as_ref()
            .map_or(self.scanner.position(), |heading| heading.start))
    }

    fn prepare_section(&mut self) -> io::Result<bool> {
        loop {
            let start = if self.started {
                let Some(heading) = self.next_heading.take() else {
                    self.finished = true;
                    return Ok(false);
                };
                self.enter(heading)
            } else {
                self.started = true;
                0
            };

            let end = self.scan_section_end()?;
            if start < end {
                self.section = Some(Section::new(start, end, self.heading_path.clone()));
                return Ok(true);
            }
        }
    }

    /// Extends the unread part of the current section over the sections after
    /// it while it is shorter than the minimum chunk size.
    fn absorb_short_span(&mut self) -> io::Result<()> {
        loop {
            if self.next_heading.is_none() {
                return Ok(());
            }
            let Some(section) = self.section.as_mut() else {
                return Ok(());
            };
            let Some(blank) = short_span(&mut self.reader, section, self.min_chars)? else {
                return Ok(());
            };
            let Some(heading) = self.next_heading.take() else {
                return Ok(());
            };
            let start = self.enter(heading);
            let end = self.scan_section_end()?;
            let heading_path = self.heading_path.clone();
            let Some(section) = self.section.as_mut() else {
                return Ok(());
            };
            section.end = end;
            if blank {
                // Whitespace carries no heading of its own: label the span
                // with the first section that has content.
                section.boundaries = vec![Boundary {
                    start: section.cursor,
                    heading_path,
                }];
            } else {
                section.boundaries.push(Boundary {
                    start,
                    heading_path,
                });
            }
        }
    }

    fn next_chunk(&mut self) -> io::Result<Option<Chunk>> {
        if self.finished {
            return Ok(None);
        }
        if self
            .section
            .as_ref()
            .is_none_or(|section| section.cursor == section.end)
            && !self.prepare_section()?
        {
            return Ok(None);
        }
        self.absorb_short_span()?;

        let section = self.section.as_mut().ok_or_else(|| {
            io::Error::other("chunk iterator reached an invalid empty section state")
        })?;
        fill(&mut self.reader, section, self.max_read_bytes)?;

        let valid_len = match std::str::from_utf8(&section.lookahead) {
            Ok(_) => section.lookahead.len(),
            Err(error) if error.error_len().is_none() => error.valid_up_to(),
            Err(error) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "Markdown input is not UTF-8 at byte {}",
                        section.cursor
                            + u64::try_from(error.valid_up_to()).map_err(io::Error::other)?
                    ),
                ));
            }
        };
        if valid_len == 0 && !section.lookahead.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Markdown input is not UTF-8 at byte {}", section.cursor),
            ));
        }
        let text = std::str::from_utf8(&section.lookahead[..valid_len])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let remaining = section.end - section.cursor;
        let split = split_byte(
            text,
            self.max_chars,
            self.min_chars,
            remaining == u64::try_from(valid_len).map_err(io::Error::other)?,
        );
        let text = String::from_utf8(section.lookahead[..split].to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let heading_path = section.heading_path();
        let local_start = section.cursor;
        section.cursor += u64::try_from(split).map_err(io::Error::other)?;
        section.lookahead.drain(..split);
        let local_end = section.cursor;
        let byte_start = absolute_offset(self.source_base_offset, local_start)?;
        let byte_end = absolute_offset(self.source_base_offset, local_end)?;
        Ok(Some(Chunk {
            text,
            byte_start,
            byte_end,
            heading_path,
        }))
    }

    /// Returns the next chunk, holding each one back until the next is known
    /// so a short final chunk, or whitespace, can be merged into a neighbour.
    /// Whitespace that fits nowhere is dropped.
    fn next_merged(&mut self) -> io::Result<Option<Chunk>> {
        loop {
            let next = self.next_chunk()?;
            let chunk = match (self.held.take(), next) {
                (None, None) => return Ok(None),
                (Some(held), None) => held,
                (None, Some(next)) => {
                    self.held = Some(next);
                    continue;
                }
                (Some(mut held), Some(next)) => {
                    let next_chars = next.text.chars().count();
                    let held_blank = held.text.trim().is_empty();
                    let mergeable = self.min_chars > 0
                        && (held_blank
                            || next_chars < self.min_chars
                            || next.text.trim().is_empty());
                    if mergeable && held.text.chars().count() + next_chars <= self.max_chars {
                        if held_blank {
                            held.heading_path = next.heading_path;
                        }
                        held.text.push_str(&next.text);
                        held.byte_end = next.byte_end;
                        self.held = Some(held);
                        continue;
                    }
                    self.held = Some(next);
                    held
                }
            };
            if self.min_chars > 0 && chunk.text.trim().is_empty() {
                continue;
            }
            return Ok(Some(chunk));
        }
    }
}

/// Reads the unread part of `section` into its lookahead, up to `max_bytes`.
fn fill<R: Read + Seek>(reader: &mut R, section: &mut Section, max_bytes: usize) -> io::Result<()> {
    let buffered = u64::try_from(section.lookahead.len()).map_err(io::Error::other)?;
    let buffered_end = section.cursor + buffered;
    let unread = section.end - buffered_end;
    let capacity = max_bytes.saturating_sub(section.lookahead.len());
    let capacity = u64::try_from(capacity).map_err(io::Error::other)?;
    let request = usize::try_from(unread.min(capacity)).map_err(io::Error::other)?;
    if request > 0 {
        reader.seek(SeekFrom::Start(buffered_end))?;
        let old_len = section.lookahead.len();
        section.lookahead.resize(old_len + request, 0);
        reader.read_exact(&mut section.lookahead[old_len..])?;
    }
    Ok(())
}

/// Whether the unread part of `section` is shorter than `min_chars`, and if so
/// whether it is only whitespace. `None` means it is not short.
fn short_span<R: Read + Seek>(
    reader: &mut R,
    section: &mut Section,
    min_chars: usize,
) -> io::Result<Option<bool>> {
    let remaining = section.end - section.cursor;
    // Every character is at most four bytes, so a span this long is not short.
    let limit = u64::try_from(min_chars.saturating_mul(4)).map_err(io::Error::other)?;
    if remaining >= limit {
        return Ok(None);
    }
    let remaining = usize::try_from(remaining).map_err(io::Error::other)?;
    fill(reader, section, remaining)?;
    // Invalid UTF-8 is reported when the span is chunked.
    Ok(std::str::from_utf8(&section.lookahead)
        .ok()
        .filter(|text| text.chars().count() < min_chars)
        .map(|text| text.trim().is_empty()))
}

impl<R: Read + Seek> Iterator for ChunkIter<R> {
    type Item = io::Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_merged() {
            Ok(Some(chunk)) => Some(Ok(chunk)),
            Ok(None) => None,
            Err(error) => {
                self.finished = true;
                Some(Err(error))
            }
        }
    }
}

fn absolute_offset(base: usize, local: u64) -> io::Result<usize> {
    let local = usize::try_from(local).map_err(io::Error::other)?;
    base.checked_add(local)
        .ok_or_else(|| io::Error::other("absolute chunk byte offset overflowed usize"))
}

/// Picks where the next chunk ends: a paragraph or whitespace boundary in the
/// last quarter before `max_chars`, else the bound itself. When `text` is the
/// whole rest of the section, the cut also leaves at least `min_tail`
/// characters after it, so no remainder is shorter than the minimum.
fn split_byte(text: &str, max_chars: usize, min_tail: usize, complete: bool) -> usize {
    let limit = if complete {
        let total = text.chars().count();
        if total <= max_chars {
            return text.len();
        }
        max_chars.min(total - min_tail)
    } else {
        max_chars
    };
    let mut count = 0;
    let mut hard = text.len();
    let mut whitespace = None;
    let mut paragraph = None;
    let preference_start = max_chars.saturating_mul(3) / 4;
    // A line ends at `\n`, `\r\n` or a lone `\r`; a blank line holds only other whitespace.
    let mut seen_line_end = false;
    let mut line_blank = true;
    let mut characters = text.char_indices().peekable();
    while let Some((byte, character)) = characters.next() {
        count += 1;
        let end = byte + character.len_utf8();
        // A `\r` before `\n`, or at the end of an incomplete read, may be half of a CRLF pair.
        let cr_pending = character == '\r'
            && match characters.peek() {
                Some(&(_, next)) => next == '\n',
                None => !complete,
            };
        let line_end = character == '\n' || (character == '\r' && !cr_pending);
        if count >= preference_start && character.is_whitespace() && !cr_pending {
            whitespace = Some(end);
            if line_end && seen_line_end && line_blank {
                paragraph = Some(end);
            }
        }
        if line_end {
            seen_line_end = true;
            line_blank = true;
        } else if !character.is_whitespace() {
            line_blank = false;
        }
        if count == limit {
            // Cutting before a leading `\r` would make an empty chunk and never advance, so
            // with `max_chars == 1` a CRLF pair (two chars) is split rather than kept whole.
            hard = if cr_pending && byte > 0 { byte } else { end };
            break;
        }
    }
    paragraph.or(whitespace).unwrap_or(hard)
}
