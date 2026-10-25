//! The in-progress `name.vdmpart` file and its `name.vdmpart.json` sidecar.
//!
//! The sidecar records which URL the part belongs to, the server's
//! validators (so a later run can tell whether resuming is safe), and the
//! segment table: how far each segment of the preallocated part has been
//! written. (`vdm-store` will replace the sidecar once the engine has queues.)

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use vdm_proto::SegmentProgress;

use crate::error::{Error, IoContext, Result};
use crate::filename::numbered;
use crate::http::is_strong_etag;

pub(crate) const PART_SUFFIX: &str = ".vdmpart";
const META_SUFFIX: &str = ".vdmpart.json";
const META_VERSION: u32 = 2;

/// Upper bound on `name (n).ext` candidates.
const MAX_CANDIDATES: u32 = 10_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PartMeta {
    pub version: u32,
    pub url: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    /// Committed progress per segment; empty for single-connection downloads.
    #[serde(default)]
    pub segments: Vec<SegmentProgress>,
}

impl PartMeta {
    pub fn new(
        url: &str,
        size: Option<u64>,
        etag: Option<String>,
        last_modified: Option<String>,
    ) -> Self {
        Self {
            version: META_VERSION,
            url: url.to_owned(),
            size,
            etag,
            last_modified,
            segments: Vec::new(),
        }
    }

    /// True if `other` describes a different version of the remote file.
    /// Validators are compared only when both sides have them.
    pub fn differs(&self, other: &Self) -> bool {
        if self.size != other.size {
            return true;
        }
        if let (Some(a), Some(b)) = (&self.etag, &other.etag) {
            return a != b;
        }
        if let (Some(a), Some(b)) = (&self.last_modified, &other.last_modified) {
            return a != b;
        }
        false
    }

    /// Value for `If-Range`: a strong ETag, else Last-Modified.
    pub fn if_range(&self) -> Option<&str> {
        self.etag.as_deref().filter(|e| is_strong_etag(e)).or(self.last_modified.as_deref())
    }
}

/// Where a download lives on disk: the chosen (possibly numbered) name, and
/// any earlier part for the same URL found there.
#[derive(Debug)]
pub(crate) struct Slot {
    pub dir: PathBuf,
    pub base: String,
    pub index: u32,
    pub part: PathBuf,
    pub meta_path: PathBuf,
    /// Sidecar of an existing part for the same URL.
    pub previous: Option<PartMeta>,
}

impl Slot {
    /// Find a slot and claim it, so no other download in this process picks the
    /// same part file while the claim is held.
    pub async fn claim(
        dir: &Path,
        base: &str,
        url: &str,
        claims: &Claims,
    ) -> Result<(Self, Claim)> {
        loop {
            let taken = claims.snapshot();
            let slot = Self::find_excluding(dir, base, url, &taken).await?;
            // Another download may have claimed it while we looked; then look again.
            if let Some(claim) = claims.claim(&slot.part) {
                return Ok((slot, claim));
            }
        }
    }

    #[cfg(test)]
    pub async fn find(dir: &Path, base: &str, url: &str) -> Result<Self> {
        Self::find_excluding(dir, base, url, &HashSet::new()).await
    }

    /// Find the first `base`, `base (1)`, ... that either has a part for this
    /// URL (resume) or is completely free (fresh start), skipping part files
    /// in `taken`.
    async fn find_excluding(
        dir: &Path,
        base: &str,
        url: &str,
        taken: &HashSet<PathBuf>,
    ) -> Result<Self> {
        for index in 0..MAX_CANDIDATES {
            let name = numbered(base, index);
            let part = dir.join(format!("{name}{PART_SUFFIX}"));
            if taken.contains(&part) {
                continue;
            }
            let meta_path = dir.join(format!("{name}{META_SUFFIX}"));
            let slot = |previous| Self {
                dir: dir.to_owned(),
                base: base.to_owned(),
                index,
                part: part.clone(),
                meta_path: meta_path.clone(),
                previous,
            };
            if exists(&part).await? {
                match read_meta(&meta_path).await {
                    Some(meta) if meta.url == url => return Ok(slot(Some(meta))),
                    // A part for another URL, or one we can't identify: leave it alone.
                    _ => continue,
                }
            }
            if !exists(&dir.join(&name)).await? {
                return Ok(slot(None));
            }
        }
        Err(Error::Io {
            path: dir.join(base),
            source: std::io::Error::new(ErrorKind::AlreadyExists, "no free file name"),
        })
    }

    /// File name this slot stands for: `base`, or `base (n)`.
    pub fn name(&self) -> String {
        numbered(&self.base, self.index)
    }

    /// Write the sidecar atomically (temp file + rename), so a crash mid-write
    /// never leaves a truncated sidecar behind.
    pub async fn write_meta(&self, meta: &PartMeta) -> Result<()> {
        let json =
            serde_json::to_vec_pretty(meta).map_err(std::io::Error::other).at(&self.meta_path)?;
        let tmp = self.meta_path.with_extension("json.tmp");
        tokio::fs::write(&tmp, json).await.at(&tmp)?;
        rename_with_retry(&tmp, &self.meta_path).await
    }

    /// Move the finished part to its final name (numbering again if that name
    /// was taken meanwhile) and delete the sidecar.
    pub async fn finish(&self) -> Result<PathBuf> {
        self.finish_from(&self.part).await
    }

    /// Like [`finish`](Self::finish), but the finished file is `file` (for
    /// media downloads, whose part is a folder of segments).
    pub async fn finish_from(&self, file: &Path) -> Result<PathBuf> {
        for index in self.index..MAX_CANDIDATES {
            let target = self.dir.join(numbered(&self.base, index));
            if exists(&target).await? {
                continue;
            }
            rename_with_retry(file, &target).await?;
            if let Err(err) = tokio::fs::remove_file(&self.meta_path).await
                && err.kind() != ErrorKind::NotFound
            {
                tracing::warn!(path = %self.meta_path.display(), %err, "could not delete sidecar");
            }
            return Ok(target);
        }
        Err(Error::Io {
            path: self.dir.join(&self.base),
            source: std::io::Error::new(ErrorKind::AlreadyExists, "no free file name"),
        })
    }
}

/// Part files in use by downloads in this process. Clones share the set.
#[derive(Debug, Clone, Default)]
pub(crate) struct Claims(Arc<Mutex<HashSet<PathBuf>>>);

impl Claims {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashSet<PathBuf>> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn snapshot(&self) -> HashSet<PathBuf> {
        self.lock().clone()
    }

    fn claim(&self, part: &Path) -> Option<Claim> {
        self.lock()
            .insert(part.to_owned())
            .then(|| Claim { claims: self.clone(), part: part.to_owned() })
    }
}

/// Releases a claimed part file when dropped.
#[derive(Debug)]
pub(crate) struct Claim {
    claims: Claims,
    part: PathBuf,
}

impl Drop for Claim {
    fn drop(&mut self) {
        self.claims.lock().remove(&self.part);
    }
}

async fn exists(path: &Path) -> Result<bool> {
    tokio::fs::try_exists(path).await.at(path)
}

async fn read_meta(path: &Path) -> Option<PartMeta> {
    let bytes = tokio::fs::read(path).await.ok()?;
    serde_json::from_slice::<PartMeta>(&bytes).ok().filter(|m| m.version == META_VERSION)
}

/// Antivirus scanners and indexers often hold a just-written file open
/// briefly on Windows, so retry a few times on "access denied".
async fn rename_with_retry(from: &Path, to: &Path) -> Result<()> {
    let mut attempt = 0;
    loop {
        match tokio::fs::rename(from, to).await {
            Ok(()) => return Ok(()),
            Err(err) if err.kind() == ErrorKind::PermissionDenied && attempt < 10 => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(100 * attempt)).await;
            }
            Err(err) => return Err(err).at(to),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(size: Option<u64>, etag: Option<&str>, lm: Option<&str>) -> PartMeta {
        PartMeta::new("u", size, etag.map(Into::into), lm.map(Into::into))
    }

    #[test]
    fn change_detection() {
        let a = meta(Some(10), Some("\"a\""), Some("d1"));
        assert!(!a.differs(&a.clone()));
        assert!(a.differs(&meta(Some(11), Some("\"a\""), Some("d1"))));
        assert!(a.differs(&meta(Some(10), Some("\"b\""), Some("d1"))));
        // ETag wins over Last-Modified when both sides have one.
        assert!(!a.differs(&meta(Some(10), Some("\"a\""), Some("d2"))));
        // Falls back to Last-Modified when an ETag is missing.
        assert!(a.differs(&meta(Some(10), None, Some("d2"))));
        assert!(!a.differs(&meta(Some(10), None, None)));
    }

    #[test]
    fn if_range_prefers_strong_etag() {
        assert_eq!(meta(None, Some("\"a\""), Some("d")).if_range(), Some("\"a\""));
        assert_eq!(meta(None, Some("W/\"a\""), Some("d")).if_range(), Some("d"));
        assert_eq!(meta(None, Some("W/\"a\""), None).if_range(), None);
    }

    #[tokio::test]
    async fn slot_resumes_matching_part_and_skips_others() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();

        // Fresh directory: first candidate.
        let s = Slot::find(d, "f.bin", "u1").await.unwrap();
        assert_eq!((s.index, s.previous.is_some()), (0, false));

        // Part for u1 exists: resume it.
        s.write_meta(&PartMeta::new("u1", Some(3), None, None)).await.unwrap();
        tokio::fs::write(&s.part, b"abc").await.unwrap();
        let s = Slot::find(d, "f.bin", "u1").await.unwrap();
        assert_eq!((s.index, s.previous.is_some()), (0, true));

        // Another URL with the same name gets the next free name.
        let s2 = Slot::find(d, "f.bin", "u2").await.unwrap();
        assert_eq!((s2.index, s2.previous.is_some()), (1, false));

        // A finished file also blocks its name.
        tokio::fs::write(d.join("g.bin"), b"x").await.unwrap();
        assert_eq!(Slot::find(d, "g.bin", "u3").await.unwrap().index, 1);
    }

    #[tokio::test]
    async fn claimed_slots_are_skipped_until_released() {
        let dir = tempfile::tempdir().unwrap();
        let claims = Claims::default();
        let (a, claim_a) = Slot::claim(dir.path(), "f.bin", "u", &claims).await.unwrap();
        let (b, claim_b) = Slot::claim(dir.path(), "f.bin", "u", &claims).await.unwrap();
        assert_eq!((a.index, b.index), (0, 1), "same URL, same name: different files");
        drop(claim_a);
        let (c, _claim_c) = Slot::claim(dir.path(), "f.bin", "u", &claims).await.unwrap();
        assert_eq!(c.index, 0, "released slots can be claimed again");
        drop(claim_b);
        assert_eq!(claims.snapshot().len(), 1);
    }

    #[tokio::test]
    async fn finish_renames_and_removes_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let s = Slot::find(dir.path(), "f.bin", "u").await.unwrap();
        s.write_meta(&PartMeta::new("u", Some(1), None, None)).await.unwrap();
        tokio::fs::write(&s.part, b"x").await.unwrap();
        // Name taken after the slot was chosen: numbered on finish.
        tokio::fs::write(dir.path().join("f.bin"), b"other").await.unwrap();

        let path = s.finish().await.unwrap();
        assert_eq!(path, dir.path().join("f (1).bin"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), b"x");
        assert!(!s.part.exists());
        assert!(!s.meta_path.exists());
    }
}
