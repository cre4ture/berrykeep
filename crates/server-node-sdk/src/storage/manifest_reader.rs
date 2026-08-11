use std::io::{self, BufRead, Read, Seek, SeekFrom};

use super::{ObjectManifest, StorageContentKind, StoragePool, hash_hex};

struct LoadedChunk {
    index: usize,
    payload: Vec<u8>,
}

/// Presents a chunked content-addressed object as one verified logical stream.
///
/// At most one source chunk is retained in memory. A chunk is fully verified
/// before any of its bytes are returned to the caller.
pub(super) struct ManifestReader<'a> {
    manifest: &'a ObjectManifest,
    storage_pool: &'a StoragePool,
    chunk_offsets: Vec<u64>,
    total_size: u64,
    position: u64,
    loaded_chunk: Option<LoadedChunk>,
    verified_chunks: Vec<bool>,
}

impl<'a> ManifestReader<'a> {
    pub(super) fn new(
        manifest: &'a ObjectManifest,
        storage_pool: &'a StoragePool,
    ) -> io::Result<Self> {
        let mut chunk_offsets = Vec::with_capacity(manifest.chunks.len());
        let mut computed_size = 0u64;
        for chunk in &manifest.chunks {
            chunk_offsets.push(computed_size);
            computed_size = computed_size
                .checked_add(chunk.size_bytes as u64)
                .ok_or_else(|| invalid_data("manifest chunk sizes overflow u64"))?;
        }

        let manifest_size = manifest.total_size_bytes as u64;
        if computed_size != manifest_size {
            return Err(invalid_data(format!(
                "manifest size mismatch for key={} expected={} actual={computed_size}",
                manifest.key, manifest.total_size_bytes
            )));
        }

        Ok(Self {
            manifest,
            storage_pool,
            chunk_offsets,
            total_size: computed_size,
            position: 0,
            loaded_chunk: None,
            verified_chunks: vec![false; manifest.chunks.len()],
        })
    }

    /// Verifies chunks that a successful short-reading consumer did not reach.
    pub(super) fn verify_all(&mut self) -> io::Result<()> {
        for index in 0..self.manifest.chunks.len() {
            if !self.verified_chunks[index] {
                self.read_verified_chunk(index)?;
                self.verified_chunks[index] = true;
            }
        }
        Ok(())
    }

    fn chunk_index_at(&self, position: u64) -> Option<usize> {
        if position >= self.total_size {
            return None;
        }

        self.chunk_offsets
            .partition_point(|offset| *offset <= position)
            .checked_sub(1)
    }

    fn ensure_chunk_loaded(&mut self, index: usize) -> io::Result<()> {
        if self
            .loaded_chunk
            .as_ref()
            .is_some_and(|chunk| chunk.index == index)
        {
            return Ok(());
        }

        let payload = self.read_verified_chunk(index)?;
        self.verified_chunks[index] = true;
        self.loaded_chunk = Some(LoadedChunk { index, payload });
        Ok(())
    }

    fn read_verified_chunk(&self, index: usize) -> io::Result<Vec<u8>> {
        let chunk = &self.manifest.chunks[index];
        let chunk_path = self
            .storage_pool
            .content_path(StorageContentKind::Chunk, &chunk.hash)
            .map_err(|error| {
                io::Error::other(format!("failed resolving chunk {}: {error:#}", chunk.hash))
            })?;
        let payload = std::fs::read(&chunk_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("failed reading chunk {}: {error}", chunk.hash),
            )
        })?;

        if payload.len() != chunk.size_bytes {
            return Err(invalid_data(format!(
                "size mismatch for chunk hash={} expected={} actual={}",
                chunk.hash,
                chunk.size_bytes,
                payload.len()
            )));
        }

        let actual_hash = hash_hex(&payload);
        if actual_hash != chunk.hash {
            return Err(invalid_data(format!(
                "hash mismatch for chunk expected={} actual={actual_hash}",
                chunk.hash
            )));
        }

        Ok(payload)
    }

    #[cfg(test)]
    fn buffered_bytes(&self) -> usize {
        self.loaded_chunk
            .as_ref()
            .map_or(0, |chunk| chunk.payload.len())
    }
}

impl Read for ManifestReader<'_> {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if output.is_empty() || self.position >= self.total_size {
            return Ok(0);
        }

        let mut written = 0;
        while written < output.len() && self.position < self.total_size {
            let index = self
                .chunk_index_at(self.position)
                .ok_or_else(|| invalid_data("manifest position is not covered by a chunk"))?;
            self.ensure_chunk_loaded(index)?;

            let chunk_offset = (self.position - self.chunk_offsets[index]) as usize;
            let payload = &self
                .loaded_chunk
                .as_ref()
                .expect("loaded chunk should be present")
                .payload;
            let available = &payload[chunk_offset..];
            let copy_len = available.len().min(output.len() - written);
            output[written..written + copy_len].copy_from_slice(&available[..copy_len]);
            written += copy_len;
            self.position += copy_len as u64;
        }

        Ok(written)
    }
}

impl BufRead for ManifestReader<'_> {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        let Some(index) = self.chunk_index_at(self.position) else {
            return Ok(&[]);
        };
        self.ensure_chunk_loaded(index)?;
        let chunk_offset = (self.position - self.chunk_offsets[index]) as usize;
        let payload = &self
            .loaded_chunk
            .as_ref()
            .expect("loaded chunk should be present")
            .payload;
        Ok(&payload[chunk_offset..])
    }

    fn consume(&mut self, amount: usize) {
        let current_index = self.chunk_index_at(self.position);
        let available = self.loaded_chunk.as_ref().map_or(0, |chunk| {
            if current_index != Some(chunk.index) {
                return 0;
            }
            let chunk_offset = (self.position - self.chunk_offsets[chunk.index]) as usize;
            chunk.payload.len().saturating_sub(chunk_offset)
        });
        self.position += amount.min(available) as u64;
    }
}

impl Seek for ManifestReader<'_> {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let next = match position {
            SeekFrom::Start(position) => position,
            SeekFrom::End(offset) => checked_position(self.total_size, offset)?,
            SeekFrom::Current(offset) => checked_position(self.position, offset)?,
        };
        self.position = next;
        Ok(next)
    }
}

fn checked_position(base: u64, offset: i64) -> io::Result<u64> {
    let position = i128::from(base) + i128::from(offset);
    u64::try_from(position).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid seek"))
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ChunkRef, StoragePool, chunk_path_for_hash};
    use std::path::PathBuf;

    async fn fixture(chunks: &[&[u8]]) -> (PathBuf, StoragePool, ObjectManifest) {
        let root = std::env::temp_dir().join(format!(
            "berrykeep-manifest-reader-{}",
            uuid::Uuid::new_v4()
        ));
        let storage_pool = StoragePool::load(&root, None).await.unwrap();
        let mut chunk_refs = Vec::new();
        let mut total_size_bytes = 0;

        for payload in chunks {
            let hash = hash_hex(payload);
            let path = chunk_path_for_hash(&root.join("chunks"), &hash).unwrap();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, payload).unwrap();
            chunk_refs.push(ChunkRef {
                hash,
                size_bytes: payload.len(),
            });
            total_size_bytes += payload.len();
        }

        let manifest = ObjectManifest {
            key: "test-object".to_string(),
            total_size_bytes,
            chunks: chunk_refs,
        };
        (root, storage_pool, manifest)
    }

    #[tokio::test]
    async fn reads_across_chunk_boundaries_with_one_chunk_buffered() {
        let (root, storage_pool, manifest) = fixture(&[b"abc", b"defgh", b"ijk"]).await;
        let mut reader = ManifestReader::new(&manifest, &storage_pool).unwrap();
        let mut payload = Vec::new();

        reader.read_to_end(&mut payload).unwrap();
        reader.verify_all().unwrap();

        assert_eq!(payload, b"abcdefghijk");
        assert!(reader.buffered_bytes() <= 5);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn supports_seeking_and_buffered_reads() {
        let (root, storage_pool, manifest) = fixture(&[b"abcd", b"efgh"]).await;
        let mut reader = ManifestReader::new(&manifest, &storage_pool).unwrap();

        reader.seek(SeekFrom::Start(3)).unwrap();
        assert_eq!(reader.fill_buf().unwrap(), b"d");
        reader.consume(1);
        assert_eq!(reader.fill_buf().unwrap(), b"efgh");
        reader.seek(SeekFrom::End(-2)).unwrap();
        let mut tail = [0; 2];
        reader.read_exact(&mut tail).unwrap();

        assert_eq!(&tail, b"gh");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn verifies_unread_chunks_before_accepting_a_short_read() {
        let (root, storage_pool, manifest) = fixture(&[b"valid", b"corrupt-later"]).await;
        let corrupt_path = storage_pool
            .content_path(StorageContentKind::Chunk, &manifest.chunks[1].hash)
            .unwrap();
        std::fs::write(corrupt_path, b"changed-data!").unwrap();
        let mut reader = ManifestReader::new(&manifest, &storage_pool).unwrap();
        let mut prefix = [0; 5];

        reader.read_exact(&mut prefix).unwrap();
        let error = reader.verify_all().unwrap_err();

        assert_eq!(&prefix, b"valid");
        assert!(error.to_string().contains("hash mismatch"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn rejects_missing_chunks() {
        let (root, storage_pool, manifest) = fixture(&[b"missing"]).await;
        let path = storage_pool
            .content_path(StorageContentKind::Chunk, &manifest.chunks[0].hash)
            .unwrap();
        std::fs::remove_file(path).unwrap();
        let mut reader = ManifestReader::new(&manifest, &storage_pool).unwrap();

        let error = reader.fill_buf().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn supports_empty_objects_and_rejects_invalid_manifest_totals() {
        let (root, storage_pool, mut manifest) = fixture(&[]).await;
        let mut reader = ManifestReader::new(&manifest, &storage_pool).unwrap();
        assert_eq!(reader.fill_buf().unwrap(), b"");
        reader.verify_all().unwrap();

        manifest.total_size_bytes = 1;
        let error = match ManifestReader::new(&manifest, &storage_pool) {
            Ok(_) => panic!("invalid manifest total must be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("manifest size mismatch"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
