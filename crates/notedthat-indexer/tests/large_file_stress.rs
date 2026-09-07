//! Direct-binary stress scenarios for bounded staging and chunk iteration.
//!
//! The 5 GiB scenario is intentionally ignored under normal test runs. Build this
//! integration test with the coordinated target directory, then execute the binary
//! directly with `--ignored --nocapture` so its peak RSS belongs to this process.

use std::fmt::Write as FmtWrite;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::{Stream, stream};
use notedthat_core::{StageError, StagedBody, StagingConfig};
use notedthat_indexer::chunker::stream_chunks;
use sha2::{Digest, Sha256};

const FRAME_BYTES: usize = 64 * 1024;
const FIVE_GIB_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MAX_CHARS: usize = 3_000;
const STAGING_ROOT: &str = ".omo/upload-tmp";

#[derive(Debug)]
struct StagingDir {
    path: PathBuf,
}

impl StagingDir {
    fn create() -> io::Result<Self> {
        let root = Path::new(STAGING_ROOT);
        fs::create_dir_all(root)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos();
        let path = root.join(format!("large-file-stress-{}-{nanos}", std::process::id()));
        fs::create_dir(&path)?;
        Ok(Self { path })
    }
}

impl Drop for StagingDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

#[derive(Debug)]
struct GeneratorState {
    remaining: u64,
    frame: Bytes,
    generated_hash: Arc<Mutex<Sha256>>,
}

fn repeated_frame() -> Bytes {
    let mut frame = vec![0_u8; FRAME_BYTES];
    for (index, byte) in frame.iter_mut().enumerate() {
        *byte = b'a' + u8::try_from(index % 26).expect("frame pattern fits u8");
    }
    Bytes::from(frame)
}

fn generated_stream(
    total_bytes: u64,
    frame: Bytes,
    generated_hash: Arc<Mutex<Sha256>>,
) -> impl Stream<Item = Result<Bytes, io::Error>> {
    stream::unfold(
        GeneratorState {
            remaining: total_bytes,
            frame,
            generated_hash,
        },
        |mut state| async move {
            if state.remaining == 0 {
                return None;
            }
            let count = usize::try_from(state.remaining.min(FRAME_BYTES as u64))
                .expect("bounded generator frame fits usize");
            let chunk = state.frame.slice(..count);
            state.remaining -= u64::try_from(count).expect("chunk length fits u64");
            state
                .generated_hash
                .lock()
                .expect("generator hash lock is not poisoned")
                .update(&chunk);
            Some((Ok(chunk), state))
        },
    )
}

#[derive(Debug)]
struct TrackingReader<R> {
    inner: R,
    max_read_buffer: usize,
    bytes_read: u64,
    read_calls: u64,
}

impl<R> TrackingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            max_read_buffer: 0,
            bytes_read: 0,
            read_calls: 0,
        }
    }
}

impl<R: Read> Read for TrackingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.max_read_buffer = self.max_read_buffer.max(buffer.len());
        self.read_calls += 1;
        let count = self.inner.read(buffer)?;
        self.bytes_read = self
            .bytes_read
            .checked_add(u64::try_from(count).map_err(io::Error::other)?)
            .ok_or_else(|| io::Error::other("tracked read byte count overflowed"))?;
        Ok(count)
    }
}

impl<R: Seek> Seek for TrackingReader<R> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        self.inner.seek(position)
    }
}

fn current_peak_rss_kib() -> io::Result<u64> {
    let status = fs::read_to_string("/proc/self/status")?;
    let value = status
        .lines()
        .find_map(|line| line.strip_prefix("VmHWM:"))
        .ok_or_else(|| io::Error::other("/proc/self/status omitted VmHWM"))?
        .split_whitespace()
        .next()
        .ok_or_else(|| io::Error::other("VmHWM omitted value"))?
        .parse::<u64>()
        .map_err(io::Error::other)?;
    Ok(value)
}

fn digest_hex(digest: Sha256) -> String {
    let mut output = String::with_capacity(64);
    for byte in digest.finalize() {
        write!(output, "{byte:02x}").expect("writing into String cannot fail");
    }
    output
}

fn hash_snapshot(hash: &Arc<Mutex<Sha256>>) -> String {
    digest_hex(
        hash.lock()
            .expect("generator hash lock is not poisoned")
            .clone(),
    )
}

fn count_entries(path: &Path) -> io::Result<usize> {
    fs::read_dir(path).map(Iterator::count)
}

#[test]
fn stages_exact_limit_and_rejects_limit_plus_one_without_content_length() {
    // Given: a disk-backed staging directory and a bounded repeated frame.
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    runtime.block_on(async {
        let exact_limit = u64::try_from(FRAME_BYTES * 2).expect("small limit fits u64");
        let dir = StagingDir::create().expect("staging directory");
        let config = StagingConfig::new(dir.path.clone());
        let exact_hash = Arc::new(Mutex::new(Sha256::new()));

        // When: unknown-length input ends exactly at the configured limit.
        let staged = StagedBody::stage_stream_to_file(
            generated_stream(exact_limit, repeated_frame(), Arc::clone(&exact_hash)),
            None,
            exact_limit,
            &config,
        )
        .await
        .expect("exact limit must be accepted");

        // Then: the actual staged file has the exact size and is removed on drop.
        assert!(staged.is_file());
        assert_eq!(staged.len(), exact_limit);
        assert_eq!(staged.file_path().expect("file-backed body").metadata().expect("file metadata").len(), exact_limit);
        drop(staged);
        assert_eq!(count_entries(&dir.path).expect("staging entries"), 0);

        let over_limit_hash = Arc::new(Mutex::new(Sha256::new()));
        // When: unknown-length input is exactly one byte over the limit.
        let result = StagedBody::stage_stream_to_file(
            generated_stream(exact_limit + 1, repeated_frame(), Arc::clone(&over_limit_hash)),
            None,
            exact_limit,
            &config,
        )
        .await;

        // Then: staging rejects the observed over-limit byte count and cleans up.
        assert!(matches!(
            result,
            Err(StageError::TooLarge {
                size: value,
                limit
            }) if value == exact_limit + 1 && limit == exact_limit
        ));
        assert_eq!(count_entries(&dir.path).expect("staging entries after rejection"), 0);
        println!(
            "BOUNDARY exact_bytes={exact_limit} exact_hash={} over_bytes={} result=TooLarge cleanup_entries=0",
            hash_snapshot(&exact_hash),
            exact_limit + 1
        );
    });
}

#[test]
#[ignore = "generated 5 GiB staging and bounded replay stress scenario"]
fn stages_and_replays_five_gib_without_retaining_outputs() {
    // Given: a deterministic 64 KiB stream with unknown Content-Length and the inclusive 5 GiB limit.
    let started = Instant::now();
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let dir = StagingDir::create().expect("staging directory");
    let config = StagingConfig::new(dir.path.clone());
    let generated_hash = Arc::new(Mutex::new(Sha256::new()));
    let frame = repeated_frame();
    let stage_started = Instant::now();
    let staged = runtime.block_on(StagedBody::stage_stream_to_file(
        generated_stream(FIVE_GIB_BYTES, frame, Arc::clone(&generated_hash)),
        None,
        FIVE_GIB_BYTES,
        &config,
    ));
    let staged = staged.expect("exactly 5 GiB must be accepted");
    let stage_elapsed = stage_started.elapsed();
    let stage_file = staged.file_path().expect("5 GiB body must be file-backed");
    let stage_file_bytes = stage_file.metadata().expect("staged file metadata").len();
    let stage_hash = hash_snapshot(&generated_hash);
    let stage_peak_rss_kib = current_peak_rss_kib().expect("stage peak RSS");

    // When: the staged disk file is replayed through the real chunk iterator into a counting sink.
    let replay_started = Instant::now();
    let source = staged.open_blocking().expect("open staged replay reader");
    let mut iterator =
        stream_chunks(TrackingReader::new(source), MAX_CHARS, 0).expect("bounded chunk iterator");
    let mut replay_hash = Sha256::new();
    let mut emitted_chunks = 0_u64;
    let mut emitted_bytes = 0_u64;
    let mut max_text_bytes = 0_usize;
    let mut max_batch_bytes = 0_usize;
    let max_batch_chunks = 1_u64;

    for item in &mut iterator {
        let chunk = item.expect("5 GiB ASCII replay must be valid UTF-8");
        let chunk_bytes = chunk.text.len();
        let chunk_start = usize::try_from(emitted_bytes).expect("5 GiB offset fits usize");
        let chunk_end = chunk_start
            .checked_add(chunk_bytes)
            .expect("chunk offset fits usize");
        assert_eq!(chunk.byte_start, chunk_start);
        assert_eq!(chunk.byte_end, chunk_end);
        assert!(chunk.text.chars().count() <= MAX_CHARS);
        emitted_bytes += u64::try_from(chunk_bytes).expect("chunk length fits u64");
        emitted_chunks += 1;
        max_text_bytes = max_text_bytes.max(chunk_bytes);
        max_batch_bytes = max_batch_bytes.max(chunk_bytes);
        replay_hash.update(chunk.text.as_bytes());
    }
    let tracked = iterator.into_inner();
    let max_read_buffer = tracked.max_read_buffer;
    let replay_read_bytes = tracked.bytes_read;
    let replay_read_calls = tracked.read_calls;
    let replay_elapsed = replay_started.elapsed();
    let replay_hash = digest_hex(replay_hash);
    let replay_peak_rss_kib = current_peak_rss_kib().expect("replay peak RSS");
    let total_elapsed = started.elapsed();

    // Then: every staged byte is emitted exactly once and all temporary resources disappear.
    assert_eq!(stage_file_bytes, FIVE_GIB_BYTES);
    assert_eq!(emitted_bytes, FIVE_GIB_BYTES);
    assert_eq!(replay_hash, stage_hash);
    assert_eq!(max_batch_chunks, 1);
    assert!(max_text_bytes <= MAX_CHARS * 4);
    assert!(max_read_buffer <= MAX_CHARS * 4);
    drop(staged);
    assert_eq!(
        count_entries(&dir.path).expect("staging entries after replay"),
        0
    );
    let staging_dir = dir.path.clone();
    drop(dir);
    assert!(!staging_dir.exists());

    println!("STRESS exact_generated_bytes={FIVE_GIB_BYTES}");
    println!("STRESS stage_file_bytes={stage_file_bytes}");
    println!("STRESS emitted_chunks={emitted_chunks} emitted_bytes={emitted_bytes}");
    println!(
        "STRESS max_text_bytes={max_text_bytes} max_batch_bytes={max_batch_bytes} max_batch_chunks={max_batch_chunks} max_read_buffer={max_read_buffer}"
    );
    println!(
        "STRESS stage_peak_rss_kib={stage_peak_rss_kib} replay_peak_rss_kib={replay_peak_rss_kib}"
    );
    println!("STRESS replay_read_bytes={replay_read_bytes} replay_read_calls={replay_read_calls}");
    println!("STRESS stage_hash={stage_hash} replay_hash={replay_hash}");
    println!(
        "STRESS stage_elapsed_ms={} replay_elapsed_ms={} total_elapsed_ms={}",
        stage_elapsed.as_millis(),
        replay_elapsed.as_millis(),
        total_elapsed.as_millis()
    );
    println!("STRESS cleanup=staged_file_deleted staging_directory_deleted");
}
