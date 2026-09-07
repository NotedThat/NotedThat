//! Bounded in-memory and private-file request body staging.

use std::io::{Read, Seek};
use std::path::Path;
use std::pin::Pin;

use bytes::{Bytes, BytesMut};
use futures::{Stream, StreamExt};
use tempfile::TempPath;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncWriteExt};

mod config;
pub use config::{BoxError, StageError, StagingConfig, UPLOAD_TMP_DIR_ENV};

/// Trait object bound for asynchronously replaying a staged body.
pub trait AsyncReadSeek: AsyncRead + AsyncSeek {}
impl<T: AsyncRead + AsyncSeek + ?Sized> AsyncReadSeek for T {}

/// Trait object bound for synchronously replaying a staged body.
pub trait ReadSeek: Read + Seek {}
impl<T: Read + Seek + ?Sized> ReadSeek for T {}

enum StagedBodyStorage {
    Memory(Bytes),
    File(TempPath),
}

/// An owned request body with a verified length and deterministic cleanup.
pub struct StagedBody {
    storage: StagedBodyStorage,
    len: u64,
}

impl StagedBody {
    /// Largest body retained in memory before staging spills to disk.
    pub const MEMORY_THRESHOLD: usize = 16 * 1024 * 1024;

    /// Wrap already-owned bytes without copying.
    #[must_use]
    pub fn from_bytes(bytes: Bytes) -> Self {
        let len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        Self {
            storage: StagedBodyStorage::Memory(bytes),
            len,
        }
    }

    /// Stage an asynchronous reader, verifying its declared and maximum lengths.
    pub async fn stage<R>(
        reader: R,
        expected_len: Option<u64>,
        limit: u64,
        config: &StagingConfig,
    ) -> Result<Self, StageError>
    where
        R: AsyncRead + Unpin,
    {
        let stream = tokio_util::io::ReaderStream::new(reader);
        Self::stage_stream(stream, expected_len, limit, config).await
    }

    /// Stage ordered byte chunks, spilling incrementally after 16 MiB.
    pub async fn stage_stream<S, E>(
        stream: S,
        expected_len: Option<u64>,
        limit: u64,
        config: &StagingConfig,
    ) -> Result<Self, StageError>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::stage_stream_inner(stream, expected_len, limit, config, false).await
    }

    /// Stage ordered byte chunks in a private file regardless of body size.
    pub async fn stage_stream_to_file<S, E>(
        stream: S,
        expected_len: Option<u64>,
        limit: u64,
        config: &StagingConfig,
    ) -> Result<Self, StageError>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: std::error::Error + Send + Sync + 'static,
    {
        Self::stage_stream_inner(stream, expected_len, limit, config, true).await
    }

    async fn stage_stream_inner<S, E>(
        stream: S,
        expected_len: Option<u64>,
        limit: u64,
        config: &StagingConfig,
        force_file: bool,
    ) -> Result<Self, StageError>
    where
        S: Stream<Item = Result<Bytes, E>>,
        E: std::error::Error + Send + Sync + 'static,
    {
        if let Some(size) = expected_len
            && size > limit
        {
            return Err(StageError::TooLarge { size, limit });
        }
        let mut chunks = Box::pin(stream);
        let mut memory = BytesMut::new();
        let mut file: Option<(tokio::fs::File, TempPath)> = None;
        let mut len = 0_u64;

        if force_file {
            file = Some(create_private_file(config).await?);
        }
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|source| StageError::Read {
                source: Box::new(source),
            })?;
            len = len
                .checked_add(u64::try_from(chunk.len()).unwrap_or(u64::MAX))
                .ok_or(StageError::TooLarge {
                    size: u64::MAX,
                    limit,
                })?;
            if len > limit {
                return Err(StageError::TooLarge { size: len, limit });
            }
            if file.is_none() && memory.len().saturating_add(chunk.len()) > Self::MEMORY_THRESHOLD {
                let (mut staged_file, path) = create_private_file(config).await?;
                staged_file
                    .write_all(&memory)
                    .await
                    .map_err(|source| StageError::Write { source })?;
                memory = BytesMut::new();
                file = Some((staged_file, path));
            }
            match &mut file {
                Some((staged_file, _)) => staged_file
                    .write_all(&chunk)
                    .await
                    .map_err(|source| StageError::Write { source })?,
                None => memory.extend_from_slice(&chunk),
            }
        }
        if let Some(expected) = expected_len
            && expected != len
        {
            return Err(StageError::LengthMismatch {
                expected,
                actual: len,
            });
        }
        match file {
            Some((mut staged_file, path)) => {
                staged_file
                    .flush()
                    .await
                    .map_err(|source| StageError::Write { source })?;
                staged_file
                    .sync_all()
                    .await
                    .map_err(|source| StageError::Write { source })?;
                drop(staged_file);
                Ok(Self {
                    storage: StagedBodyStorage::File(path),
                    len,
                })
            }
            None => Ok(Self {
                storage: StagedBodyStorage::Memory(memory.freeze()),
                len,
            }),
        }
    }

    /// Verified body length.
    #[must_use]
    pub const fn len(&self) -> u64 {
        self.len
    }

    /// Whether the body has no bytes.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether the body is backed by a private staging file.
    #[must_use]
    pub const fn is_file(&self) -> bool {
        matches!(&self.storage, StagedBodyStorage::File(_))
    }

    /// Return at most `maximum` leading bytes without loading a file in full.
    pub async fn prefix(&self, maximum: usize) -> Result<Bytes, std::io::Error> {
        match &self.storage {
            StagedBodyStorage::Memory(bytes) => Ok(bytes.slice(..bytes.len().min(maximum))),
            StagedBodyStorage::File(path) => {
                let mut file = tokio::fs::File::open(path).await?;
                let mut bytes = vec![0; maximum.min(usize::try_from(self.len).unwrap_or(maximum))];
                file.read_exact(&mut bytes).await?;
                Ok(Bytes::from(bytes))
            }
        }
    }

    /// Open an asynchronous replay reader positioned at byte zero.
    pub async fn open(&self) -> Result<Pin<Box<dyn AsyncReadSeek + Send>>, std::io::Error> {
        match &self.storage {
            StagedBodyStorage::Memory(bytes) => Ok(Box::pin(std::io::Cursor::new(bytes.clone()))),
            StagedBodyStorage::File(path) => Ok(Box::pin(tokio::fs::File::open(path).await?)),
        }
    }

    /// Open a blocking replay reader positioned at byte zero.
    pub fn open_blocking(&self) -> Result<Box<dyn ReadSeek + Send>, std::io::Error> {
        match &self.storage {
            StagedBodyStorage::Memory(bytes) => Ok(Box::new(std::io::Cursor::new(bytes.clone()))),
            StagedBodyStorage::File(path) => Ok(Box::new(std::fs::File::open(path)?)),
        }
    }

    /// Borrow in-memory bytes when no staging file was needed.
    #[must_use]
    pub fn memory_bytes(&self) -> Option<&Bytes> {
        match &self.storage {
            StagedBodyStorage::Memory(bytes) => Some(bytes),
            StagedBodyStorage::File(_) => None,
        }
    }

    /// Borrow the private file path for retryable file streaming.
    #[must_use]
    pub fn file_path(&self) -> Option<&Path> {
        match &self.storage {
            StagedBodyStorage::File(path) => Some(path),
            StagedBodyStorage::Memory(_) => None,
        }
    }
}

impl From<Bytes> for StagedBody {
    fn from(bytes: Bytes) -> Self {
        Self::from_bytes(bytes)
    }
}

async fn create_private_file(
    config: &StagingConfig,
) -> Result<(tokio::fs::File, TempPath), StageError> {
    let directory = config.directory.clone();
    let named = tokio::task::spawn_blocking(move || tempfile::NamedTempFile::new_in(directory))
        .await
        .map_err(|error| StageError::Config {
            message: format!("staging file task failed: {error}"),
        })?
        .map_err(|source| StageError::Write { source })?;
    let path = named.into_temp_path();
    let file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .await
        .map_err(|source| StageError::Write { source })?;
    Ok((file, path))
}
