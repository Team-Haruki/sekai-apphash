//! Lazy random access to one entry of a UnityFS bundle.
//!
//! `unity-rs-core` 0.6 can stream an entry front to back
//! (`UnityFsBundle::copy_entry*`) but has no random-access view of an entry,
//! so reading one small object deep inside a large `resources.assets` would
//! decode (and, over HTTP, download) every block before it. This source
//! decodes only the storage blocks a read overlaps, which keeps a remote
//! extraction to the blocks that hold the serialized metadata and the config
//! objects. Candidate for upstreaming as an entry `Region` in `unity-rs`.

use std::io;
use std::sync::{Arc, Mutex};

use unity_rs_core::bundle::{CompressionType, StorageBlock, UnityFsBundle};
use unity_rs_core::source::{Region, Source};

const DECODED_BLOCK_SLOTS: usize = 16;

/// Whether every block of `bundle` can be decoded by [`BundleEntrySource`].
/// (Encrypted bundles never get this far: they need a key to open at all.)
#[must_use]
pub fn supports_lazy_entries(bundle: &UnityFsBundle) -> bool {
    bundle.blocks.iter().all(|block| {
        matches!(
            block.compression,
            CompressionType::None | CompressionType::Lz4 | CompressionType::Lz4Hc
        )
    })
}

/// A [`Source`] over one bundle entry that decodes blocks on demand.
pub struct BundleEntrySource {
    bundle_region: Region,
    blocks: Arc<[StorageBlock]>,
    entry_offset: u64,
    entry_size: u64,
    decoded: Mutex<Vec<(usize, Arc<[u8]>)>>,
}

impl BundleEntrySource {
    /// `bundle_region` must be the region the bundle was opened from.
    #[must_use]
    pub fn new(
        bundle_region: Region,
        blocks: Arc<[StorageBlock]>,
        entry_offset: u64,
        entry_size: u64,
    ) -> Self {
        Self {
            bundle_region,
            blocks,
            entry_offset,
            entry_size,
            decoded: Mutex::new(Vec::new()),
        }
    }

    /// Opens `entry_index` of `bundle` as a lazily decoded region.
    pub fn entry_region(
        bundle_region: &Region,
        bundle: &UnityFsBundle,
        blocks: &Arc<[StorageBlock]>,
        entry_index: usize,
    ) -> unity_rs_core::Result<Region> {
        let entry = bundle.entries.get(entry_index).ok_or_else(|| {
            unity_rs_core::Error::invalid_data(format!(
                "bundle entry {entry_index} is out of range"
            ))
        })?;
        let source = Self::new(
            bundle_region.clone(),
            Arc::clone(blocks),
            entry.offset,
            entry.size,
        );
        Region::new(Arc::new(source), 0, entry.size)
    }

    fn block_index(&self, logical: u64) -> Option<usize> {
        let index = self
            .blocks
            .partition_point(|block| block.uncompressed_offset <= logical);
        index.checked_sub(1).filter(|&i| {
            let block = &self.blocks[i];
            logical < block.uncompressed_offset + u64::from(block.uncompressed_size)
        })
    }

    fn decoded_block(&self, index: usize) -> io::Result<Arc<[u8]>> {
        if let Some((_, data)) = self
            .decoded
            .lock()
            .expect("block cache poisoned")
            .iter()
            .find(|(cached, _)| *cached == index)
        {
            return Ok(Arc::clone(data));
        }
        let block = &self.blocks[index];
        let mut compressed = vec![0u8; block.compressed_size as usize];
        self.bundle_region
            .read_exact_at(block.compressed_offset, &mut compressed)
            .map_err(io::Error::other)?;
        let mut output = vec![0u8; block.uncompressed_size as usize];
        let written =
            lz4_flex::block::decompress_into(&compressed, &mut output).map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("bundle block {index}: invalid LZ4 data: {error}"),
                )
            })?;
        if written != output.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "bundle block {index}: decoded {written} bytes, expected {}",
                    output.len()
                ),
            ));
        }
        let data: Arc<[u8]> = Arc::from(output);
        let mut cache = self.decoded.lock().expect("block cache poisoned");
        if cache.len() >= DECODED_BLOCK_SLOTS {
            cache.remove(0);
        }
        cache.push((index, Arc::clone(&data)));
        Ok(data)
    }
}

impl Source for BundleEntrySource {
    fn len(&self) -> u64 {
        self.entry_size
    }

    fn read_exact_at(&self, offset: u64, output: &mut [u8]) -> io::Result<()> {
        let length = output.len() as u64;
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.entry_size)
        {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("entry read {offset}+{length} exceeds {}", self.entry_size),
            ));
        }
        let mut logical = self.entry_offset + offset;
        let mut written = 0usize;
        while written < output.len() {
            let index = self.block_index(logical).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("no bundle block covers offset {logical}"),
                )
            })?;
            let block = &self.blocks[index];
            let within = logical - block.uncompressed_offset;
            let available = u64::from(block.uncompressed_size) - within;
            let take = available.min((output.len() - written) as u64) as usize;
            let target = &mut output[written..written + take];
            match block.compression {
                CompressionType::None => self
                    .bundle_region
                    .read_exact_at(block.compressed_offset + within, target)
                    .map_err(io::Error::other)?,
                CompressionType::Lz4 | CompressionType::Lz4Hc => {
                    let data = self.decoded_block(index)?;
                    target.copy_from_slice(&data[within as usize..within as usize + take]);
                }
                other => {
                    return Err(io::Error::new(
                        io::ErrorKind::Unsupported,
                        format!("bundle block compression {other:?} is not lazily decodable"),
                    ))
                }
            }
            written += take;
            logical += take as u64;
        }
        Ok(())
    }
}
