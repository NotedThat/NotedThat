use std::io::{self, Read, Seek, SeekFrom};

use super::Chunk;

mod scan;

use scan::{Heading, HeadingScanner};

const HEADING_LABEL_CHAR_CAP: usize = 3_000;

struct Section {
    cursor: u64,
    end: u64,
    heading_path: Vec<String>,
    lookahead: Vec<u8>,
}

/// A bounded-memory iterator over chunks from a seekable Markdown reader.
pub struct ChunkIter<R> {
    reader: R,
    scanner: HeadingScanner,
    section: Option<Section>,
    next_heading: Option<Heading>,
    heading_path: Vec<String>,
    max_chars: usize,
    max_read_bytes: usize,
    source_base_offset: usize,
    started: bool,
    finished: bool,
}

/// Creates a bounded-memory Markdown chunk iterator.
///
/// `source_base_offset` is added to every returned byte range, allowing the reader
/// to begin after an independently parsed prefix such as OKF frontmatter.
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
        max_chars,
        max_read_bytes,
        source_base_offset,
        started: false,
        finished: false,
    })
}

impl<R> ChunkIter<R> {
    /// Returns the underlying reader after iteration or early termination.
    pub fn into_inner(self) -> R {
        self.reader
    }
}

impl<R: Read + Seek> ChunkIter<R> {
    fn prepare_section(&mut self) -> io::Result<bool> {
        loop {
            let start = if self.started {
                let Some(heading) = self.next_heading.take() else {
                    self.finished = true;
                    return Ok(false);
                };
                let keep = heading.depth.saturating_sub(1);
                self.heading_path
                    .truncate(keep.min(self.heading_path.len()));
                self.heading_path.push(heading.label);
                heading.start
            } else {
                self.started = true;
                0
            };

            self.next_heading = self.scanner.next_heading(&mut self.reader)?;
            let end = self
                .next_heading
                .as_ref()
                .map_or(self.scanner.position(), |heading| heading.start);
            if start < end {
                self.section = Some(Section {
                    cursor: start,
                    end,
                    heading_path: self.heading_path.clone(),
                    lookahead: Vec::new(),
                });
                return Ok(true);
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

        let section = self.section.as_mut().ok_or_else(|| {
            io::Error::other("chunk iterator reached an invalid empty section state")
        })?;
        let buffered = u64::try_from(section.lookahead.len()).map_err(io::Error::other)?;
        let buffered_end = section.cursor + buffered;
        let unread = section.end - buffered_end;
        let capacity = self.max_read_bytes.saturating_sub(section.lookahead.len());
        let capacity = u64::try_from(capacity).map_err(io::Error::other)?;
        let request = usize::try_from(unread.min(capacity)).map_err(io::Error::other)?;
        if request > 0 {
            self.reader.seek(SeekFrom::Start(buffered_end))?;
            let old_len = section.lookahead.len();
            section.lookahead.resize(old_len + request, 0);
            self.reader.read_exact(&mut section.lookahead[old_len..])?;
        }

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
            remaining == u64::try_from(valid_len).map_err(io::Error::other)?,
        );
        let text = String::from_utf8(section.lookahead[..split].to_vec())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
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
            heading_path: section.heading_path.clone(),
        }))
    }
}

impl<R: Read + Seek> Iterator for ChunkIter<R> {
    type Item = io::Result<Chunk>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_chunk() {
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

fn split_byte(text: &str, max_chars: usize, complete: bool) -> usize {
    let mut count = 0;
    let mut hard = text.len();
    let mut whitespace = None;
    let mut paragraph = None;
    let preference_start = max_chars.saturating_mul(3) / 4;
    let mut previous_newline = false;
    for (byte, character) in text.char_indices() {
        count += 1;
        let end = byte + character.len_utf8();
        if count >= preference_start && character.is_whitespace() {
            whitespace = Some(end);
            if character == '\n' && previous_newline {
                paragraph = Some(end);
            }
        }
        previous_newline = character == '\n';
        if count == max_chars {
            hard = end;
            break;
        }
    }
    if complete && count < max_chars {
        text.len()
    } else {
        paragraph.or(whitespace).unwrap_or(hard)
    }
}
