//! An incremental `text/event-stream` parser.
//!
//! The MCP server reads two SSE producers: the API's object change events
//! route, which its subscription forwarders consume as the caller (D66), and —
//! in tests — its own stateful transport, whose every answer is SSE-framed.
//! Both are read chunk by chunk from an HTTP body, so the parser keeps whatever
//! did not yet end in a blank line and hands back only complete frames.
//!
//! What it implements of the specification is what those two producers use:
//! `id:`, `event:`, `data:` (multi-line, joined with `\n`), `retry:`, comment
//! lines (dropped), `\n` or `\r\n` line ends. A frame whose only field is an
//! empty `data:` is returned as such — rmcp opens every stream with one (`id:
//! 0`, `retry:`) and a reader that wants messages skips it.

/// One complete frame.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// The `id:` field, verbatim.
    pub id: Option<String>,
    /// The `event:` field.
    pub event: Option<String>,
    /// Every `data:` line, joined with `\n`; empty when the frame carried none.
    pub data: String,
    /// The `retry:` field, when it parsed as milliseconds.
    pub retry: Option<u64>,
}

impl SseEvent {
    /// Whether the frame carries a message at all, as opposed to a priming or
    /// comment-only frame.
    #[must_use]
    pub fn has_data(&self) -> bool {
        !self.data.is_empty()
    }
}

/// Feed it bytes as they arrive; take the frames they completed.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    /// A parser with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `bytes` and return every frame that is now complete, in order.
    ///
    /// Bytes are treated as UTF-8 with replacement; a producer that splits a
    /// multi-byte character across chunks is not one either of ours is.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        self.buf.push_str(&String::from_utf8_lossy(bytes));
        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_blank_line(&self.buf) {
            let block = self.buf[..end].to_owned();
            self.buf.drain(..end + sep_len);
            if let Some(event) = parse_block(&block) {
                out.push(event);
            }
        }
        out
    }
}

/// The first blank line: where the block before it ends, and how long the
/// separator is (`\n\n`, `\r\n\r\n`, or a mix).
fn find_blank_line(text: &str) -> Option<(usize, usize)> {
    let mut best: Option<(usize, usize)> = None;
    for (needle, len) in [("\r\n\r\n", 4), ("\n\n", 2), ("\r\n\n", 3), ("\n\r\n", 3)] {
        if let Some(pos) = text.find(needle)
            && best.is_none_or(|(b, _)| pos < b)
        {
            best = Some((pos, len));
        }
    }
    best
}

/// One block between blank lines; `None` when it held only comments (or
/// nothing), which the specification says to dispatch nothing for.
fn parse_block(block: &str) -> Option<SseEvent> {
    let mut event = SseEvent::default();
    let mut data_lines: Vec<&str> = Vec::new();
    let mut saw_field = false;
    for line in block.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() || line.starts_with(':') {
            continue;
        }
        saw_field = true;
        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "data" => data_lines.push(value),
            "event" => event.event = Some(value.to_owned()),
            "id" => event.id = Some(value.to_owned()),
            "retry" => event.retry = value.parse().ok(),
            _ => {}
        }
    }
    if !saw_field {
        return None;
    }
    event.data = data_lines.join("\n");
    Some(event)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_split_across_chunks_arrives_once_complete() {
        let mut parser = SseParser::new();
        assert!(parser.feed(b"id: 7\nevent: object.wri").is_empty());
        assert!(parser.feed(b"tten\ndata: {\"a\":1}\n").is_empty());
        let events = parser.feed(b"\n");
        assert_eq!(
            events,
            vec![SseEvent {
                id: Some("7".into()),
                event: Some("object.written".into()),
                data: "{\"a\":1}".into(),
                retry: None,
            }]
        );
    }

    #[test]
    fn several_frames_in_one_chunk_come_out_in_order_and_crlf_is_fine() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: one\r\n\r\ndata: two\r\n\r\ndata: thr");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "one");
        assert_eq!(events[1].data, "two");
        let events = parser.feed(b"ee\r\n\r\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "three");
    }

    #[test]
    fn comments_are_dropped_and_a_comment_only_block_dispatches_nothing() {
        let mut parser = SseParser::new();
        // The API's heartbeat, and rmcp's ping.
        assert!(parser.feed(b": keep-alive\n\n").is_empty());
        assert!(parser.feed(b": ping\n\n").is_empty());
        let events = parser.feed(b": subscribed\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn the_priming_frames_of_both_producers_are_returned_without_data() {
        let mut parser = SseParser::new();
        // The API's first frame.
        let events = parser.feed(b"retry: 3000\n: subscribed\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].retry, Some(3000));
        assert!(!events[0].has_data());
        // rmcp's first frame on every stream.
        let events = parser.feed(b"id: 0\nretry: 3000\ndata: \n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id.as_deref(), Some("0"));
        assert!(!events[0].has_data());
    }

    #[test]
    fn multi_line_data_is_joined_and_a_bare_field_name_is_an_empty_value() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"data: line one\ndata:line two\ndata\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "line one\nline two\n");
    }

    #[test]
    fn unknown_fields_are_ignored_and_a_bad_retry_is_none() {
        let mut parser = SseParser::new();
        let events = parser.feed(b"foo: bar\nretry: soon\ndata: x\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].retry, None);
        assert_eq!(events[0].data, "x");
    }
}
