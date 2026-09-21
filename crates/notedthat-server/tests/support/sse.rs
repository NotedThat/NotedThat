//! Reading `text/event-stream` responses in the E2E suites.
//!
//! A stream never ends on its own, so every reader here takes a count and a
//! deadline: it returns once that many event frames (frames carrying `data:`)
//! have arrived, and panics with what it did get if they never do.
//!
//! The indexer publishes its verdict on every write it finishes (D64), at a
//! moment of its own choosing, so a test about the *write path's* events reads
//! with [`Subscription::events_where`] and [`change_events`] and lets the
//! outcomes ride along unasserted.

#![allow(dead_code)]

use std::time::Duration;

use futures::StreamExt;

/// One SSE frame as a client parses it: comments and `retry:` are kept so a
/// test can assert on the stream's first frame too.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub id: Option<String>,
    pub event: Option<String>,
    pub data: Option<serde_json::Value>,
    pub retry: Option<String>,
    pub comments: Vec<String>,
}

impl Frame {
    /// The `data` field's `object_key`, for the common assertion.
    pub fn key(&self) -> &str {
        self.data.as_ref().expect("event frame")["object_key"]
            .as_str()
            .expect("object_key")
    }

    /// The `data` field's `source`.
    pub fn source(&self) -> &str {
        self.data.as_ref().expect("event frame")["source"]
            .as_str()
            .expect("source")
    }

    /// The frame's `id:` as a number.
    pub fn id_number(&self) -> u64 {
        self.id.as_deref().expect("id").parse().expect("numeric id")
    }

    /// Whether the frame is the indexer's verdict rather than a change.
    pub fn is_outcome(&self) -> bool {
        self.source() == "indexer"
    }
}

/// The predicate for a reader that wants change events only.
pub fn change_events(frame: &Frame) -> bool {
    !frame.is_outcome()
}

/// Parse every complete frame in `text`; a trailing partial frame is ignored.
pub fn parse_frames(text: &str) -> Vec<Frame> {
    let complete = match text.rfind("\n\n") {
        Some(end) => &text[..end],
        None => return Vec::new(),
    };
    complete
        .split("\n\n")
        .filter(|block| !block.trim().is_empty())
        .map(|block| {
            let mut frame = Frame {
                id: None,
                event: None,
                data: None,
                retry: None,
                comments: Vec::new(),
            };
            for line in block.lines() {
                if let Some(comment) = line.strip_prefix(':') {
                    frame.comments.push(comment.trim_start().to_string());
                } else if let Some((field, value)) = line.split_once(':') {
                    let value = value.strip_prefix(' ').unwrap_or(value);
                    match field {
                        "id" => frame.id = Some(value.to_string()),
                        "event" => frame.event = Some(value.to_string()),
                        "data" => {
                            frame.data = Some(serde_json::from_str(value).expect("JSON data"));
                        }
                        "retry" => frame.retry = Some(value.to_string()),
                        _ => {}
                    }
                }
            }
            frame
        })
        .collect()
}

/// An open subscription: the response body, read incrementally.
pub struct Subscription {
    body: std::pin::Pin<Box<dyn futures::Stream<Item = reqwest::Result<bytes::Bytes>> + Send>>,
    text: String,
    consumed: usize,
}

impl Subscription {
    pub fn open(response: reqwest::Response) -> Self {
        assert_eq!(response.status(), reqwest::StatusCode::OK, "subscribe");
        assert!(
            response
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream")),
            "content-type: {:?}",
            response.headers().get("content-type")
        );
        Self {
            body: Box::pin(response.bytes_stream()),
            text: String::new(),
            consumed: 0,
        }
    }

    /// Wait until at least `wanted` event frames have arrived since the last
    /// call, and return every new one (comment-only frames are skipped).
    pub async fn events(&mut self, wanted: usize, timeout: Duration) -> Vec<Frame> {
        self.events_where(wanted, timeout, |_| true).await
    }

    /// Wait until at least `wanted` event frames passing `keep` have arrived
    /// since the last call, and return those; frames failing `keep` that
    /// arrived in the meantime are consumed and dropped.
    pub async fn events_where(
        &mut self,
        wanted: usize,
        timeout: Duration,
        keep: impl Fn(&Frame) -> bool,
    ) -> Vec<Frame> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let all: Vec<Frame> = parse_frames(&self.text)
                .into_iter()
                .filter(|f| f.data.is_some())
                .collect();
            let new: Vec<Frame> = all[self.consumed..]
                .iter()
                .filter(|f| keep(f))
                .cloned()
                .collect();
            if new.len() >= wanted {
                self.consumed = all.len();
                return new;
            }
            let chunk = tokio::time::timeout_at(deadline, self.body.next())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "timed out waiting for {wanted} event(s); received so far:\n{}",
                        self.text
                    )
                })
                .expect("stream still open")
                .expect("chunk");
            self.text
                .push_str(std::str::from_utf8(&chunk).expect("utf-8"));
        }
    }

    /// Assert that no event frame arrives within `quiet`.
    pub async fn expect_silence(&mut self, quiet: Duration) {
        self.expect_silence_where(quiet, |_| true).await;
    }

    /// Assert that no event frame passing `keep` arrives within `quiet`;
    /// whatever else arrived is consumed.
    pub async fn expect_silence_where(&mut self, quiet: Duration, keep: impl Fn(&Frame) -> bool) {
        let deadline = tokio::time::Instant::now() + quiet;
        while let Ok(Some(chunk)) = tokio::time::timeout_at(deadline, self.body.next()).await {
            self.text
                .push_str(std::str::from_utf8(&chunk.expect("chunk")).expect("utf-8"));
        }
        let all: Vec<Frame> = parse_frames(&self.text)
            .into_iter()
            .filter(|f| f.data.is_some())
            .collect();
        let unread: Vec<&Frame> = all[self.consumed..].iter().filter(|f| keep(f)).collect();
        assert!(unread.is_empty(), "expected no events, got {unread:?}");
        self.consumed = all.len();
    }

    /// Every frame received so far, comments included.
    pub fn all_frames(&self) -> Vec<Frame> {
        parse_frames(&self.text)
    }
}
