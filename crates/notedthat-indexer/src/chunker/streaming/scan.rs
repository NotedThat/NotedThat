use std::{
    collections::VecDeque,
    io::{self, Read, Seek, SeekFrom},
};

mod syntax;

use syntax::{
    Fence, HtmlEnd, closes_fence, container_content, html_continues, is_setext_underline,
    opens_fence, opens_html, parse_giant_atx, parse_heading, quote_depth,
};

const READ_BUFFER_BYTES: usize = 8 * 1_024;
const LINE_CAPTURE_BYTES: usize = 16 * 1_024;

pub(super) struct Heading {
    pub(super) start: u64,
    pub(super) depth: usize,
    pub(super) label: String,
}

struct CapturedParagraph {
    start: u64,
    bytes: Vec<u8>,
    overflowed: bool,
}

pub(super) struct HeadingScanner {
    position: u64,
    line_start: u64,
    line: Vec<u8>,
    line_tail: VecDeque<u8>,
    line_overflowed: bool,
    paragraph: Option<CapturedParagraph>,
    fence: Option<Fence>,
    html: Option<HtmlEnd>,
    html_quote_depth: usize,
    label_char_cap: usize,
    eof: bool,
}

impl HeadingScanner {
    pub(super) fn new(label_char_cap: usize) -> Self {
        Self {
            position: 0,
            line_start: 0,
            line: Vec::with_capacity(LINE_CAPTURE_BYTES),
            line_tail: VecDeque::with_capacity(64),
            line_overflowed: false,
            paragraph: None,
            fence: None,
            html: None,
            html_quote_depth: 0,
            label_char_cap,
            eof: false,
        }
    }

    pub(super) const fn position(&self) -> u64 {
        self.position
    }

    pub(super) fn next_heading<R: Read + Seek>(
        &mut self,
        reader: &mut R,
    ) -> io::Result<Option<Heading>> {
        if self.eof {
            return Ok(None);
        }
        reader.seek(SeekFrom::Start(self.position))?;
        let mut buffer = [0; READ_BUFFER_BYTES];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                self.eof = true;
                if self.position > self.line_start {
                    return self.finish_line();
                }
                return Ok(None);
            }
            for byte in &buffer[..read] {
                self.position += 1;
                if self.line.len() < LINE_CAPTURE_BYTES {
                    self.line.push(*byte);
                } else {
                    self.line_overflowed = true;
                }
                if self.line_tail.len() == 64 {
                    self.line_tail.pop_front();
                }
                self.line_tail.push_back(*byte);
                if *byte == b'\n' {
                    let heading = self.finish_line()?;
                    self.line_start = self.position;
                    if heading.is_some() {
                        return Ok(heading);
                    }
                }
            }
            reader.seek(SeekFrom::Start(self.position))?;
        }
    }

    fn finish_line(&mut self) -> io::Result<Option<Heading>> {
        let current = CapturedParagraph {
            start: self.line_start,
            bytes: std::mem::take(&mut self.line),
            overflowed: std::mem::take(&mut self.line_overflowed),
        };
        let line = captured_utf8(&current.bytes);
        let tail: Vec<_> = self.line_tail.drain(..).collect();
        let tail = captured_utf8(&tail);
        let content = container_content(line);
        let quote_depth = quote_depth(line);

        if let Some(html_end) = self.html.take()
            && quote_depth >= self.html_quote_depth
        {
            if html_continues(&html_end, content, tail) {
                self.html = Some(html_end);
            }
            self.paragraph = None;
            return Ok(None);
        }

        if let Some(fence) = self.fence.take()
            && quote_depth >= fence.quote_depth
        {
            if !closes_fence(content, &fence) {
                self.fence = Some(fence);
            }
            self.paragraph = None;
            return Ok(None);
        }

        if let Some(fence) = opens_fence(content, quote_depth) {
            self.fence = Some(fence);
            self.paragraph = None;
            return Ok(None);
        }

        if let Some(html_end) = opens_html(content) {
            if html_continues(&html_end, content, tail) {
                self.html = Some(html_end);
                self.html_quote_depth = quote_depth;
            }
            self.paragraph = None;
            return Ok(None);
        }

        if !current.overflowed {
            if let Some((offset, depth, label)) = parse_heading(line, self.label_char_cap) {
                self.paragraph = None;
                return Ok(Some(Heading {
                    start: current.start + u64::try_from(offset).map_err(io::Error::other)?,
                    depth,
                    label,
                }));
            }
        } else if let Some((depth, label)) = parse_giant_atx(content, self.label_char_cap) {
            self.paragraph = None;
            return Ok(Some(Heading {
                start: current.start
                    + u64::try_from(line.len() - content.len()).map_err(io::Error::other)?,
                depth,
                label,
            }));
        }

        if is_setext_underline(content)
            && let Some(paragraph) = self.paragraph.take()
        {
            let mut span = paragraph.bytes;
            let valid_prefix = captured_utf8(&span).len();
            span.truncate(valid_prefix);
            if !span.ends_with(b"\n") {
                span.push(b'\n');
            }
            span.extend_from_slice(&current.bytes);
            let span = captured_utf8(&span);
            if let Some((offset, depth, label)) = parse_heading(span, self.label_char_cap) {
                return Ok(Some(Heading {
                    start: paragraph.start + u64::try_from(offset).map_err(io::Error::other)?,
                    depth,
                    label,
                }));
            }
        }

        if content.trim().is_empty() {
            self.paragraph = None;
        } else if let Some(paragraph) = self.paragraph.as_mut() {
            let remaining = LINE_CAPTURE_BYTES.saturating_sub(paragraph.bytes.len());
            let copied = remaining.min(current.bytes.len());
            paragraph.bytes.extend_from_slice(&current.bytes[..copied]);
            paragraph.overflowed |= current.overflowed || copied < current.bytes.len();
        } else {
            self.paragraph = Some(current);
        }
        Ok(None)
    }
}

fn captured_utf8(bytes: &[u8]) -> &str {
    match std::str::from_utf8(bytes) {
        Ok(value) => value,
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap_or("")
        }
        Err(_) => "",
    }
}
