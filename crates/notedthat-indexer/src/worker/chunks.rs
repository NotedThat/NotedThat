use super::snapshot::BodyReader;
use crate::chunker;
use notedthat_core::StagedBody;
use std::sync::Arc;

type SnapshotChunkIter = chunker::ChunkIter<BodyReader<Box<dyn notedthat_core::ReadSeek + Send>>>;

pub(super) struct ChunkCursor {
    pub chunks: SnapshotChunkIter,
    pub next_index: usize,
}

pub(super) fn take_chunk_batch(
    mut cursor: ChunkCursor,
    batch_size: usize,
) -> Result<(ChunkCursor, Vec<(usize, chunker::Chunk)>), String> {
    let mut batch = Vec::with_capacity(batch_size);
    for _ in 0..batch_size {
        let Some(chunk) = cursor.chunks.next() else {
            break;
        };
        let chunk = chunk.map_err(|err| format!("streaming chunk read failed: {err}"))?;
        let chunk_index = cursor.next_index;
        cursor.next_index = cursor
            .next_index
            .checked_add(1)
            .ok_or_else(|| "chunk index overflow".to_owned())?;
        batch.push((chunk_index, chunk));
    }
    Ok((cursor, batch))
}

pub(super) fn validate_chunk_byte_bound(
    batch: &[(usize, chunker::Chunk)],
    maximum: usize,
) -> Result<(), String> {
    if let Some((index, chunk)) = batch.iter().find(|(_, chunk)| chunk.text.len() > maximum) {
        return Err(format!(
            "chunk {index} is {} UTF-8 bytes, exceeding conservative embedder bound {maximum}",
            chunk.text.len()
        ));
    }
    Ok(())
}

pub(super) async fn open_chunk_cursor(
    body: Arc<StagedBody>,
    body_start: usize,
    max_chars: usize,
) -> Result<ChunkCursor, String> {
    tokio::task::spawn_blocking(move || {
        let reader = body.open_blocking()?;
        let body_offset = u64::try_from(body_start).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "frontmatter offset exceeds supported file position",
            )
        })?;
        let body_reader = BodyReader::new(reader, body_offset)?;
        let chunks = chunker::stream_chunks(body_reader, max_chars, body_start)?;
        Ok::<_, std::io::Error>(ChunkCursor {
            chunks,
            next_index: 0,
        })
    })
    .await
    .map_err(|err| format!("chunk iterator task failed: {err}"))?
    .map_err(|err| format!("chunk iterator setup failed: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(text: &str) -> chunker::Chunk {
        chunker::Chunk {
            text: text.to_owned(),
            byte_start: 0,
            byte_end: text.len(),
            heading_path: Vec::new(),
        }
    }

    #[test]
    fn byte_bound_accepts_ascii_and_rejects_indivisible_unicode() {
        // Given: one-scalar chunks with different UTF-8 byte lengths.
        let ascii = [(0, chunk("a"))];
        let unicode = [(0, chunk("日"))];

        // When: both are checked against a one-byte conservative bound.
        let ascii_result = validate_chunk_byte_bound(&ascii, 1);
        let unicode_result = validate_chunk_byte_bound(&unicode, 1);

        // Then: ASCII remains indexable while an indivisible larger scalar fails explicitly.
        assert!(ascii_result.is_ok());
        assert!(unicode_result.is_err());
    }
}
