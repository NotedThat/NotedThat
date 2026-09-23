//! An incremental `text/event-stream` parser.
//!
//! The MCP server reads two SSE producers: the API's object change events
//! route, which its subscription forwarders consume as the caller (D66), and —
//! in tests — its own stateful transport, whose every answer is SSE-framed.
//! Both are read chunk by chunk from an HTTP body, so the parser keeps whatever
//! did not yet end in a blank line — down to a character split across two
//! chunks — and hands back only complete frames.
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

/// The most a parser holds for a frame that has not ended.
///
/// Both producers are ours and reached over loopback, so this is not an attack
/// surface; it is a stall that would never resolve. One that stopped emitting
/// blank lines would grow the buffer for the life of the stream, and the
/// notification leg lives as long as its session. A megabyte is orders of
/// magnitude above any frame either producer emits.
const MAX_PENDING: usize = 1024 * 1024;

/// Feed it bytes as they arrive; take the frames they completed.
#[derive(Debug, Default)]
pub struct SseParser {
    /// Decoded text of frames not yet ended by a blank line.
    text: String,
    /// Bytes that end mid-character and cannot be decoded until more arrive.
    partial: Vec<u8>,
}

impl SseParser {
    /// A parser with nothing buffered.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Append `bytes` and return every frame that is now complete, in order.
    ///
    /// Bytes are buffered, not text. Where a chunk ends is a transport
    /// artefact — `reqwest::Response::chunk` hands back whatever hyper read off
    /// the socket — so a multi-byte character can straddle two chunks whatever
    /// the producer does. Decoding each chunk on its own would replace such a
    /// character with `U+FFFD` on both sides of the split, and the `data:`
    /// payload here carries an arbitrary object key: a subscription to
    /// `notes/café.md` would silently miss its own notification, rarely and
    /// non-deterministically. So a trailing incomplete character is kept for
    /// the next call, and only a genuinely invalid sequence is replaced.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<SseEvent> {
        if self.partial.is_empty() {
            self.decode(bytes);
        } else {
            let mut joined = std::mem::take(&mut self.partial);
            joined.extend_from_slice(bytes);
            self.decode(&joined);
        }

        let mut out = Vec::new();
        while let Some((end, sep_len)) = find_blank_line(&self.text) {
            let block = self.text[..end].to_owned();
            self.text.drain(..end + sep_len);
            if let Some(event) = parse_block(&block) {
                out.push(event);
            }
        }
        if self.text.len() + self.partial.len() > MAX_PENDING {
            tracing::warn!(
                pending = self.text.len() + self.partial.len(),
                "an SSE producer sent more than a megabyte without ending a frame; dropping it"
            );
            self.text.clear();
            self.partial.clear();
        }
        out
    }

    /// Decode as much of `bytes` as is complete, keeping a trailing partial
    /// character for the next chunk.
    fn decode(&mut self, mut bytes: &[u8]) {
        loop {
            match std::str::from_utf8(bytes) {
                Ok(text) => {
                    self.text.push_str(text);
                    return;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    // Safe by `valid_up_to`'s contract.
                    self.text
                        .push_str(std::str::from_utf8(&bytes[..valid]).unwrap_or_default());
                    // `Some(len)` is a sequence that is invalid however much
                    // more arrives: replace it and carry on, the way a lossy
                    // decode would. `None` means the chunk ended mid-character,
                    // so the tail is kept for the next one.
                    let Some(len) = error.error_len() else {
                        self.partial.extend_from_slice(&bytes[valid..]);
                        return;
                    };
                    self.text.push(char::REPLACEMENT_CHARACTER);
                    bytes = &bytes[valid + len..];
                }
            }
        }
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

    /// The bug this parser buffers bytes for: `é` split across an HTTP chunk
    /// boundary used to become two `U+FFFD`, so a subscription to `notes/café.md`
    /// looked up a key that did not exist and dropped its own notification.
    #[test]
    fn a_character_split_across_chunks_survives() {
        let frame = "data: {\"object_key\":\"notes/café.md\"}\n\n".as_bytes();
        let split = frame
            .iter()
            .position(|byte| *byte == 0xC3)
            .expect("the é is two bytes")
            + 1;
        let mut parser = SseParser::new();
        assert!(parser.feed(&frame[..split]).is_empty());
        let events = parser.feed(&frame[split..]);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "{\"object_key\":\"notes/café.md\"}");
    }

    /// Every single-byte split of a frame full of multi-byte characters.
    #[test]
    fn no_split_of_a_multi_byte_frame_corrupts_it() {
        let payload = "data: notes/café-日本語-Кир.md";
        let frame = format!("{payload}\n\n");
        // Every split, including the ones that land inside a character — which is
        // the case under test.
        for split in 1..frame.len() {
            let (head, tail) = frame.as_bytes().split_at(split);
            let mut parser = SseParser::new();
            let mut events = parser.feed(head);
            events.extend(parser.feed(tail));
            assert_eq!(events.len(), 1, "split at {split}");
            assert_eq!(
                events[0].data,
                payload.trim_start_matches("data: "),
                "split at {split}"
            );
        }
    }

    /// A byte sequence that is invalid however much more arrives is replaced, not
    /// held forever.
    #[test]
    fn an_invalid_sequence_is_replaced_and_the_frame_still_parses() {
        let mut parser = SseParser::new();
        let mut bytes = b"data: a".to_vec();
        bytes.push(0xFF);
        bytes.extend_from_slice(b"b\n\n");
        let events = parser.feed(&bytes);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "a\u{FFFD}b");
    }

    /// A producer that never ends a frame does not grow the buffer without bound.
    #[test]
    fn a_frame_that_never_ends_is_dropped_rather_than_buffered_forever() {
        let mut parser = SseParser::new();
        for _ in 0..3 {
            assert!(parser.feed(&vec![b'x'; 512 * 1024]).is_empty());
        }
        assert!(
            parser.feed(b"data: after\n\n").len() <= 1,
            "the stream recovers rather than accumulating"
        );
    }

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
