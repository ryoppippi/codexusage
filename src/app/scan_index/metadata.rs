//! File metadata for the scan index.

use crate::app::report::SessionScanTarget;
use eyre::{Result, WrapErr, eyre};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// FNV-1a 64-bit offset basis.
const FNV_OFFSET_LEFT: u64 = 0xcbf2_9ce4_8422_2325;
/// Alternate FNV-1a 64-bit offset basis.
const FNV_OFFSET_RIGHT: u64 = 0x8422_2325_cbf2_9ce4;
/// FNV-1a 64-bit prime.
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
/// Bytes sampled from the start and end of an indexed prefix.
const HASH_SAMPLE_BYTES: usize = 4 * 1024;
/// Bytes sampled from the start and end of an indexed prefix as `u64`.
const HASH_SAMPLE_BYTES_U64: u64 = 4 * 1024;

/// Rolling content hash for parsed session bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct ContentHash {
    /// First FNV-1a lane.
    left: u64,
    /// Second FNV-1a lane with a different seed.
    right: u64,
}

impl ContentHash {
    /// Return a fresh hash state.
    pub(super) const fn new() -> Self {
        Self {
            left: FNV_OFFSET_LEFT,
            right: FNV_OFFSET_RIGHT,
        }
    }

    /// Update the hash with one byte slice.
    fn update(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.left ^= u64::from(*byte);
            self.left = self.left.wrapping_mul(FNV_PRIME);
            self.right ^= u64::from(*byte).rotate_left(1);
            self.right = self.right.wrapping_mul(FNV_PRIME);
        }
    }

    /// Encode the hash for `SQLite` storage.
    pub(super) fn encode(self) -> String {
        format!("{:016x}{:016x}", self.left, self.right)
    }

    /// Decode a hash from `SQLite` storage.
    pub(super) fn decode(value: &str) -> Option<Self> {
        if value.len() != 32 || !value.is_ascii() {
            return None;
        }
        Some(Self {
            left: u64::from_str_radix(&value[..16], 16).ok()?,
            right: u64::from_str_radix(&value[16..], 16).ok()?,
        })
    }
}

/// Builds the same bounded fingerprint as `content_hash_prefix` from parser-consumed bytes.
#[derive(Default)]
pub(super) struct ParsedContentHash {
    /// Number of parsed bytes.
    offset: u64,
    /// First bytes in the parsed prefix.
    head: Vec<u8>,
    /// Last bytes in the parsed prefix.
    tail: Vec<u8>,
}

impl ParsedContentHash {
    /// Observe one parser-consumed byte segment.
    pub(super) fn observe(&mut self, bytes: &[u8]) {
        let head_remaining = HASH_SAMPLE_BYTES.saturating_sub(self.head.len());
        if head_remaining > 0 {
            self.head
                .extend_from_slice(&bytes[..bytes.len().min(head_remaining)]);
        }

        if bytes.len() >= HASH_SAMPLE_BYTES {
            self.tail.clear();
            self.tail
                .extend_from_slice(&bytes[bytes.len() - HASH_SAMPLE_BYTES..]);
        } else {
            self.tail.extend_from_slice(bytes);
            if self.tail.len() > HASH_SAMPLE_BYTES {
                let excess = self.tail.len() - HASH_SAMPLE_BYTES;
                self.tail.drain(..excess);
            }
        }

        self.offset = self
            .offset
            .saturating_add(u64::try_from(bytes.len()).unwrap_or(u64::MAX));
    }

    /// Return the number of observed bytes.
    pub(super) const fn offset(&self) -> u64 {
        self.offset
    }

    /// Finish the bounded content hash.
    pub(super) fn finish(self) -> ContentHash {
        let mut hash = ContentHash::new();
        hash.update(&self.offset.to_le_bytes());
        hash.update(&self.head);
        if self.offset > HASH_SAMPLE_BYTES_U64 {
            hash.update(&[0xff]);
            hash.update(&self.tail);
        }
        hash
    }
}

/// Fingerprint the already-indexed prefix of a file using bounded reads.
pub(super) fn content_hash_prefix(path: &Path, offset: u64) -> Result<ContentHash> {
    let mut file =
        File::open(path).wrap_err_with(|| format!("failed to open {}", path.display()))?;
    let mut hash = ContentHash::new();
    hash.update(&offset.to_le_bytes());

    let sample_bytes = HASH_SAMPLE_BYTES_U64;
    let head_len = usize::try_from(offset.min(sample_bytes))
        .wrap_err("hash sample length does not fit usize")?;
    read_hash_sample(&mut file, head_len, &mut hash)?;
    if offset > sample_bytes {
        hash.update(&[0xff]);
        file.seek(SeekFrom::Start(offset.saturating_sub(sample_bytes)))?;
        read_hash_sample(&mut file, HASH_SAMPLE_BYTES, &mut hash)?;
    }
    Ok(hash)
}

/// Read a fixed-size fingerprint sample into a hash.
fn read_hash_sample(file: &mut File, len: usize, hash: &mut ContentHash) -> Result<()> {
    let mut remaining = len;
    let mut buffer = [0_u8; HASH_SAMPLE_BYTES];
    while remaining > 0 {
        let read_len = remaining.min(buffer.len());
        let bytes_read = file.read(&mut buffer[..read_len])?;
        if bytes_read == 0 {
            return Err(eyre!("file ended before indexed prefix"));
        }
        hash.update(&buffer[..bytes_read]);
        remaining -= bytes_read;
    }
    Ok(())
}

/// Metadata observed for one selected file.
pub(super) struct ObservedFile {
    /// Canonical path used as the cache key.
    pub(super) path_key: String,
    /// File metadata stamp.
    pub(super) metadata: FileMetadata,
}

impl ObservedFile {
    /// Build observed metadata for one target.
    pub(super) fn from_target(target: &SessionScanTarget) -> Self {
        Self {
            path_key: target.path_key.clone(),
            metadata: FileMetadata::from_target(target),
        }
    }
}

/// File metadata fields used to classify cache safety.
#[derive(Clone)]
pub(super) struct FileMetadata {
    /// Indexed prefix size in bytes.
    pub(super) size: u64,
    /// Modification time in nanoseconds since Unix epoch.
    pub(super) mtime_ns: Option<i64>,
    /// Unix device identifier.
    pub(super) dev: Option<i64>,
    /// Unix inode identifier.
    pub(super) ino: Option<i64>,
    /// Unix ctime in nanoseconds since Unix epoch.
    pub(super) ctime_ns: Option<i64>,
}

impl FileMetadata {
    /// Build metadata fields from one selected target.
    fn from_target(target: &SessionScanTarget) -> Self {
        Self {
            size: target.bytes,
            mtime_ns: target.metadata.mtime_ns,
            dev: target.metadata.dev,
            ino: target.metadata.ino,
            ctime_ns: target.metadata.ctime_ns,
        }
    }

    /// Return whether every observable content marker matches.
    pub(super) fn same_contents_as(&self, other: &Self) -> bool {
        self.size == other.size
            && self.mtime_ns == other.mtime_ns
            && self.dev == other.dev
            && self.ino == other.ino
            && self.ctime_ns == other.ctime_ns
    }

    /// Return whether both metadata values identify the same file object.
    pub(super) fn same_identity_as(&self, other: &Self) -> bool {
        match (self.dev, self.ino, other.dev, other.ino) {
            (Some(left_dev), Some(left_ino), Some(right_dev), Some(right_ino)) => {
                left_dev == right_dev && left_ino == right_ino
            }
            _ => true,
        }
    }
}
