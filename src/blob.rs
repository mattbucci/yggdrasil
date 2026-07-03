//! Content-addressed blob store. ADR 0016: large agent outputs (transcripts,
//! analysis docs, anything > MAX_INLINE_PAYLOAD_BYTES) live here, referenced
//! from `task_runs.output_blob_ref` by their sha256 hex.
//!
//! On-disk layout mirrors git's object store:
//!
//! ```text
//! <root>/blobs/ab/cd/ef0123…   (first 2/2 hex chars as fanout dirs)
//! ```
//!
//! Two-character fanout keeps each subdir under a few thousand entries even
//! at hundreds of thousands of blobs, which matters on filesystems that get
//! slow with very large directories.

use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum BlobError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid blob ref {0:?}: must be 64 hex chars")]
    InvalidRef(String),
}

/// 64-char lowercase hex. Newtype keeps random strings out of typed APIs.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct BlobRef(String);

impl BlobRef {
    pub fn parse(s: impl Into<String>) -> Result<Self, BlobError> {
        let s = s.into();
        if s.len() != 64
            || !s
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        {
            return Err(BlobError::InvalidRef(s));
        }
        Ok(Self(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone)]
pub struct BlobStat {
    pub size: u64,
    pub modified: std::time::SystemTime,
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

impl BlobStore {
    /// Create or open a store rooted at `root/blobs`. The directory is created
    /// if missing.
    pub fn new(root: impl AsRef<Path>) -> Result<Self, BlobError> {
        let root = root.as_ref().join("blobs");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// Default global root: `$YGG_BLOB_DIR` if set, else
    /// `$XDG_DATA_HOME/yggdrasil` if set, else `~/.local/share/yggdrasil`.
    /// Falls back to `/tmp/yggdrasil` if home discovery fails.
    pub fn default_root() -> PathBuf {
        if let Ok(p) = std::env::var("YGG_BLOB_DIR") {
            return PathBuf::from(p);
        }
        if let Ok(p) = std::env::var("XDG_DATA_HOME") {
            return PathBuf::from(p).join("yggdrasil");
        }
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(".local/share/yggdrasil");
        }
        PathBuf::from("/tmp/yggdrasil")
    }

    /// Open the default global blob store.
    pub fn open_default() -> Result<Self, BlobError> {
        Self::new(Self::default_root())
    }

    fn path_for(&self, r: &BlobRef) -> PathBuf {
        let s = r.as_str();
        self.root.join(&s[0..2]).join(&s[2..4]).join(&s[4..])
    }

    /// Hash the bytes, write atomically (tmp + rename), return the ref.
    /// Idempotent: putting the same bytes twice is a no-op the second time.
    pub fn put(&self, bytes: &[u8]) -> Result<BlobRef, BlobError> {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = hasher.finalize();
        let hex = hex_lower(&digest);
        let blob_ref = BlobRef(hex);

        let dest = self.path_for(&blob_ref);
        if dest.exists() {
            return Ok(blob_ref);
        }

        let parent = dest.parent().expect("blob path has parent");
        fs::create_dir_all(parent)?;

        let tmp = parent.join(format!(".tmp.{}.{}", std::process::id(), blob_ref.as_str()));
        {
            let mut f = fs::File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
        }
        // rename is atomic on the same filesystem.
        fs::rename(&tmp, &dest)?;
        Ok(blob_ref)
    }

    pub fn get(&self, r: &BlobRef) -> Result<Vec<u8>, BlobError> {
        Ok(fs::read(self.path_for(r))?)
    }

    pub fn stat(&self, r: &BlobRef) -> Result<BlobStat, BlobError> {
        let m = fs::metadata(self.path_for(r))?;
        Ok(BlobStat {
            size: m.len(),
            modified: m.modified()?,
        })
    }

    pub fn exists(&self, r: &BlobRef) -> bool {
        self.path_for(r).is_file()
    }

    /// Walk the store; for each blob whose ref is NOT in `keep`, delete it.
    /// Returns the number of files removed. Empty fanout dirs are pruned as a
    /// side effect.
    pub fn gc(&self, keep: &std::collections::HashSet<BlobRef>) -> Result<usize, BlobError> {
        let mut removed = 0usize;
        if !self.root.is_dir() {
            return Ok(0);
        }
        for outer in fs::read_dir(&self.root)? {
            let outer = outer?;
            if !outer.file_type()?.is_dir() {
                continue;
            }
            for inner in fs::read_dir(outer.path())? {
                let inner = inner?;
                if !inner.file_type()?.is_dir() {
                    continue;
                }
                for blob in fs::read_dir(inner.path())? {
                    let blob = blob?;
                    if !blob.file_type()?.is_file() {
                        continue;
                    }
                    let name = blob.file_name();
                    let name_str = name.to_string_lossy();
                    if name_str.starts_with(".tmp.") {
                        continue;
                    }
                    let outer_name = outer.file_name();
                    let inner_name = inner.file_name();
                    let full = format!(
                        "{}{}{}",
                        outer_name.to_string_lossy(),
                        inner_name.to_string_lossy(),
                        name_str
                    );
                    if let Ok(bref) = BlobRef::parse(&full)
                        && !keep.contains(&bref)
                    {
                        fs::remove_file(blob.path())?;
                        removed += 1;
                    }
                }
                // Try removing now-empty subdir; ignore not-empty errors.
                let _ = fs::remove_dir(inner.path());
            }
            let _ = fs::remove_dir(outer.path());
        }
        Ok(removed)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = tmp();
        let store = BlobStore::new(dir.path()).unwrap();
        let bytes = b"hello world";
        let r = store.put(bytes).unwrap();
        assert_eq!(r.as_str().len(), 64);
        let back = store.get(&r).unwrap();
        assert_eq!(back, bytes);
    }

    #[test]
    fn put_is_idempotent() {
        let dir = tmp();
        let store = BlobStore::new(dir.path()).unwrap();
        let r1 = store.put(b"x").unwrap();
        let r2 = store.put(b"x").unwrap();
        assert_eq!(r1, r2);
    }

    #[test]
    fn stat_returns_size() {
        let dir = tmp();
        let store = BlobStore::new(dir.path()).unwrap();
        let r = store.put(b"abcdef").unwrap();
        assert_eq!(store.stat(&r).unwrap().size, 6);
    }

    #[test]
    fn gc_drops_orphans_keeps_named() {
        let dir = tmp();
        let store = BlobStore::new(dir.path()).unwrap();
        let keep_ref = store.put(b"keep me").unwrap();
        let _drop_ref = store.put(b"drop me").unwrap();
        let mut keep = std::collections::HashSet::new();
        keep.insert(keep_ref.clone());
        let removed = store.gc(&keep).unwrap();
        assert_eq!(removed, 1);
        assert!(store.exists(&keep_ref));
    }

    #[test]
    fn invalid_ref_rejected() {
        assert!(BlobRef::parse("too-short").is_err());
        assert!(BlobRef::parse("X".repeat(64)).is_err()); // uppercase rejected
        assert!(BlobRef::parse("0".repeat(64)).is_ok());
    }

    #[test]
    fn ref_path_fanout() {
        let dir = tmp();
        let store = BlobStore::new(dir.path()).unwrap();
        let r = BlobRef::parse("a".repeat(64)).unwrap();
        let p = store.path_for(&r);
        let suffix = p.strip_prefix(store.root()).unwrap();
        let mut comps = suffix.components();
        assert_eq!(comps.next().unwrap().as_os_str(), "aa");
        assert_eq!(comps.next().unwrap().as_os_str(), "aa");
    }
}
