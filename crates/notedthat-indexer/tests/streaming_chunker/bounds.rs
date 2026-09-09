use std::io::{Read, Seek, SeekFrom};

use notedthat_indexer::chunker::stream_chunks;

struct GeneratedReader {
    len: u64,
    position: u64,
    max_request: usize,
    bytes_read: u64,
}

impl Read for GeneratedReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let buffer_len = u64::try_from(buffer.len()).map_err(std::io::Error::other)?;
        let count = usize::try_from((self.len - self.position).min(buffer_len))
            .map_err(std::io::Error::other)?;
        buffer[..count].fill(b'x');
        let count_u64 = u64::try_from(count).map_err(std::io::Error::other)?;
        self.position += count_u64;
        self.bytes_read += count_u64;
        self.max_request = self.max_request.max(buffer.len());
        Ok(count)
    }
}

impl Seek for GeneratedReader {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        let next = match position {
            SeekFrom::Start(value) => i128::from(value),
            SeekFrom::End(value) => i128::from(self.len) + i128::from(value),
            SeekFrom::Current(value) => i128::from(self.position) + i128::from(value),
        };
        self.position = u64::try_from(next).map_err(std::io::Error::other)?;
        Ok(self.position)
    }
}

fn consume(size: u64) -> (u64, GeneratedReader) {
    let reader = GeneratedReader {
        len: size,
        position: 0,
        max_request: 0,
        bytes_read: 0,
    };
    let mut iterator = stream_chunks(reader, 3_000, 0).expect("valid iterator");
    let mut emitted = 0_u64;
    for chunk in iterator.by_ref() {
        emitted += u64::try_from(chunk.expect("generated ASCII is valid").text.len())
            .expect("chunk length fits u64");
    }
    (emitted, iterator.into_inner())
}

#[test]
fn bounds_read_requests_and_total_streaming_io() {
    // Given
    let size = 1_024 * 1_024_u64;

    // When
    let (emitted, reader) = consume(size);

    // Then
    assert_eq!(emitted, size);
    assert!(reader.max_request <= 12_000);
    assert!(reader.bytes_read <= size * 2 + 12_000);
}

#[test]
#[ignore = "generated 5 GiB bounded-memory stress scenario"]
fn stress_streams_five_gibibytes_without_retaining_outputs() {
    // Given
    let size = 5 * 1_024 * 1_024 * 1_024_u64;

    // When
    let (emitted, reader) = consume(size);

    // Then
    assert_eq!(emitted, size);
    assert!(reader.max_request <= 12_000);
    assert!(reader.bytes_read <= size * 2 + 12_000);
}
