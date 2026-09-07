use sha2::{Digest, Sha256};
use std::io::{Error, ErrorKind, Read, Seek, SeekFrom};

#[derive(Debug, PartialEq, Eq)]
pub(super) struct SnapshotFacts {
    pub content_hash: String,
    pub prefix: String,
}

pub(super) struct SnapshotObserver {
    hasher: Sha256,
    prefix: Vec<u8>,
    incomplete: Vec<u8>,
    prefix_limit: usize,
}

impl SnapshotObserver {
    pub fn new(prefix_limit: usize) -> Self {
        Self {
            hasher: Sha256::new(),
            prefix: Vec::with_capacity(prefix_limit.min(64 * 1024)),
            incomplete: Vec::with_capacity(3),
            prefix_limit,
        }
    }

    pub fn observe(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.hasher.update(bytes);
        let prefix_remaining = self.prefix_limit.saturating_sub(self.prefix.len());
        self.prefix
            .extend_from_slice(&bytes[..bytes.len().min(prefix_remaining)]);

        let mut validation = Vec::with_capacity(self.incomplete.len() + bytes.len());
        validation.extend_from_slice(&self.incomplete);
        validation.extend_from_slice(bytes);
        match std::str::from_utf8(&validation) {
            Ok(_) => self.incomplete.clear(),
            Err(error) if error.error_len().is_some() => {
                return Err(Error::new(ErrorKind::InvalidData, error));
            }
            Err(error) => {
                self.incomplete.clear();
                self.incomplete
                    .extend_from_slice(&validation[error.valid_up_to()..]);
            }
        }
        Ok(())
    }

    pub fn finish(&self) -> Result<SnapshotFacts, Error> {
        if !self.incomplete.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "snapshot ends inside a UTF-8 scalar",
            ));
        }
        let mut prefix = self.prefix.clone();
        while std::str::from_utf8(&prefix).is_err_and(|error| error.error_len().is_none()) {
            prefix.pop();
        }
        let prefix = String::from_utf8(prefix)
            .map_err(|error| Error::new(ErrorKind::InvalidData, error.utf8_error()))?;
        Ok(SnapshotFacts {
            content_hash: format!("{:x}", self.hasher.clone().finalize()),
            prefix,
        })
    }
}

pub(super) struct BodyReader<R> {
    inner: R,
    base: u64,
}

impl<R: Seek> BodyReader<R> {
    pub fn new(mut inner: R, base: u64) -> Result<Self, Error> {
        inner.seek(SeekFrom::Start(base))?;
        Ok(Self { inner, base })
    }

    fn logical_position(&mut self, absolute: u64) -> Result<u64, Error> {
        absolute
            .checked_sub(self.base)
            .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "seek precedes snapshot body"))
    }
}

impl<R: Read> Read for BodyReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> Result<usize, Error> {
        self.inner.read(buffer)
    }
}

impl<R: Seek> Seek for BodyReader<R> {
    fn seek(&mut self, position: SeekFrom) -> Result<u64, Error> {
        let absolute = match position {
            SeekFrom::Start(offset) => self.base.checked_add(offset),
            SeekFrom::Current(offset) => self.inner.stream_position()?.checked_add_signed(offset),
            SeekFrom::End(offset) => self
                .inner
                .seek(SeekFrom::End(0))?
                .checked_add_signed(offset),
        }
        .ok_or_else(|| Error::new(ErrorKind::InvalidInput, "snapshot body seek overflow"))?;
        self.logical_position(absolute)?;
        let positioned = self.inner.seek(SeekFrom::Start(absolute))?;
        self.logical_position(positioned)
    }
}

#[cfg(test)]
fn inspect_reader(mut reader: impl Read, prefix_limit: usize) -> Result<SnapshotFacts, Error> {
    let mut observer = SnapshotObserver::new(prefix_limit);
    let mut buffer = vec![0_u8; 64 * 1024];

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observer.observe(&buffer[..read])?;
    }
    observer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read, Seek, SeekFrom};

    struct ThreeByteReads(Cursor<Vec<u8>>);

    impl Read for ThreeByteReads {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            let bound = buffer.len().min(3);
            self.0.read(&mut buffer[..bound])
        }
    }

    #[test]
    fn inspect_reader_hashes_complete_unicode_stream_across_read_boundaries() {
        // Given: Unicode scalar bytes split across three-byte reader frames.
        let source = "ab日本語cd";

        // When: the seekable snapshot is inspected incrementally.
        let facts = inspect_reader(ThreeByteReads(Cursor::new(source.as_bytes().to_vec())), 5)
            .expect("valid UTF-8 snapshot");

        // Then: the whole file hash and bounded prefix refer to the original bytes.
        assert_eq!(
            facts.content_hash,
            "0f94de9f77390bccef22d54ec58e7bf13e2217d06867caa9ced6be8e77a3fc35"
        );
        assert_eq!(facts.prefix, "ab日");
    }

    #[test]
    fn inspect_reader_rejects_invalid_utf8_split_across_read_boundaries() {
        // Given: an invalid continuation byte in a framed snapshot.
        let source = b"ab\xe6\x97\xffcd".to_vec();

        // When: the snapshot is inspected.
        let error = inspect_reader(ThreeByteReads(Cursor::new(source)), 16)
            .expect_err("invalid UTF-8 must fail indexing");

        // Then: the error reports invalid data rather than accepting a partial file.
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn body_reader_exposes_frontmatter_body_as_offset_zero() {
        // Given: a full snapshot and a body beginning after frontmatter.
        let source = Cursor::new(b"frontBODY".to_vec());
        let mut reader = BodyReader::new(source, 5).expect("body offset is valid");

        // When: a chunk consumer seeks relative to its logical input.
        reader.seek(SeekFrom::Start(2)).expect("seek within body");
        let mut suffix = String::new();
        reader
            .read_to_string(&mut suffix)
            .expect("read body suffix");

        // Then: logical offset two addresses the underlying snapshot after the prefix.
        assert_eq!(suffix, "DY");
    }
}
