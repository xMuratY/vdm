//! A torrent's bytes on disk: pieces mapped onto its files, written block by
//! block, read back to check each piece's SHA-1 and to serve other peers.
//!
//! All calls block; the session runs them on tokio's blocking pool.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use super::metainfo::{Hash, Info, sha1};
use super::wire::Bitfield;
use crate::pwrite::{read_exact_at, write_all_at};

pub(crate) struct Storage {
    /// The content: the file (single-file torrents) or the folder.
    content: PathBuf,
    info: Arc<Info>,
    /// Open files, by index into `info.files`; dropped by [`close`](Self::close).
    open: Mutex<Vec<Option<Arc<File>>>>,
}

impl Storage {
    /// Files live under `root`: `root/name` for one file, `root/name/...` for a folder.
    pub fn new(root: &Path, info: Arc<Info>) -> Self {
        Self::at(root.join(&info.name), info)
    }

    /// Content at `content` (a finished torrent may have been renamed).
    pub fn at(content: PathBuf, info: Arc<Info>) -> Self {
        let open = Mutex::new(vec![None; info.files.len()]);
        Self { content, info, open }
    }

    /// The file or folder that holds the content.
    pub fn content_path(&self) -> &Path {
        &self.content
    }

    fn path_of(&self, index: usize) -> PathBuf {
        if self.info.single_file {
            self.content.clone()
        } else {
            self.content.join(&self.info.files[index].path)
        }
    }

    /// Whether every file is present at its full size (before seeding).
    pub fn files_present(&self) -> bool {
        self.info.files.iter().enumerate().all(|(i, f)| {
            f.pad || std::fs::metadata(self.path_of(i)).is_ok_and(|m| m.len() == f.length)
        })
    }

    fn file(&self, index: usize, create: bool) -> io::Result<Arc<File>> {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Some(f)) = open.get(index) {
            return Ok(f.clone());
        }
        let path = self.path_of(index);
        let file = if create {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(&path)?;
            let length = self.info.files[index].length;
            if f.metadata()?.len() < length {
                f.set_len(length)?;
            }
            f
        } else {
            OpenOptions::new().read(true).open(&path)?
        };
        let file = Arc::new(file);
        open[index] = Some(file.clone());
        Ok(file)
    }

    /// The (file index, offset in file, length) pieces of a byte range of the torrent.
    fn spans(&self, offset: u64, len: u64) -> Vec<(usize, u64, u64)> {
        let files = &self.info.files;
        // The last file starting at or before `offset`.
        let mut i = files.partition_point(|f| f.offset <= offset).saturating_sub(1);
        let (mut pos, end) = (offset, offset + len);
        let mut out = Vec::new();
        while pos < end && i < files.len() {
            let f = &files[i];
            let file_end = f.offset + f.length;
            if f.length > 0 && pos < file_end {
                let take = end.min(file_end) - pos;
                out.push((i, pos - f.offset, take));
                pos += take;
            }
            i += 1;
        }
        out
    }

    fn range(&self, piece: u32, begin: u32, len: u64) -> io::Result<(u64, u64)> {
        let size = self.info.piece_size(piece);
        if piece >= self.info.piece_count() || u64::from(begin) + len > size {
            return Err(io::Error::new(ErrorKind::InvalidInput, "block outside the piece"));
        }
        Ok((u64::from(piece) * self.info.piece_length + u64::from(begin), len))
    }

    pub fn write(&self, piece: u32, begin: u32, data: &[u8]) -> io::Result<()> {
        let (offset, len) = self.range(piece, begin, data.len() as u64)?;
        let mut at = 0usize;
        for (index, file_offset, n) in self.spans(offset, len) {
            let n = n as usize;
            if !self.info.files[index].pad {
                write_all_at(&*self.file(index, true)?, &data[at..at + n], file_offset)?;
            }
            at += n;
        }
        Ok(())
    }

    pub fn read(&self, piece: u32, begin: u32, len: u32) -> io::Result<Vec<u8>> {
        let (offset, len) = self.range(piece, begin, u64::from(len))?;
        let mut out = vec![0u8; len as usize];
        let mut at = 0usize;
        for (index, file_offset, n) in self.spans(offset, len) {
            let n = n as usize;
            if !self.info.files[index].pad {
                read_exact_at(&*self.file(index, false)?, &mut out[at..at + n], file_offset)?;
            }
            at += n;
        }
        Ok(out)
    }

    /// True when piece `index` on disk matches its hash (false if unreadable).
    pub fn verify(&self, index: u32) -> bool {
        let size = self.info.piece_size(index) as u32;
        let expected: &Hash = &self.info.pieces[index as usize];
        self.read(index, 0, size).is_ok_and(|data| &sha1(&data) == expected)
    }

    /// Check every piece on disk (e.g. files already there from elsewhere).
    pub fn verify_all(&self) -> Bitfield {
        let mut have = Bitfield::new(self.info.piece_count());
        for i in 0..self.info.piece_count() {
            if self.verify(i) {
                have.set(i);
            }
        }
        have
    }

    /// Create the empty files (they never get a block) and close every file,
    /// so the content can be moved.
    pub fn finish(&self) -> io::Result<()> {
        for (i, f) in self.info.files.iter().enumerate() {
            if f.length == 0 && !f.pad {
                self.file(i, true)?;
            }
        }
        self.close();
        // Padding files exist only in the byte stream, never on disk.
        Ok(())
    }

    pub fn close(&self) {
        let mut open = self.open.lock().unwrap_or_else(|e| e.into_inner());
        open.iter_mut().for_each(|f| *f = None);
    }
}

// ------------------------------------------------------------ resume state

const RESUME_MAGIC: &[u8; 8] = b"VDMTOR01";

/// Save which pieces are done (atomically: temp file, then rename).
pub(crate) fn save_resume(path: &Path, info_hash: &Hash, have: &Bitfield) -> io::Result<()> {
    let mut data = Vec::with_capacity(32 + have.bytes().len());
    data.extend_from_slice(RESUME_MAGIC);
    data.extend_from_slice(info_hash);
    data.extend_from_slice(&have.len().to_be_bytes());
    data.extend_from_slice(have.bytes());
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

/// The saved pieces, if the file belongs to this torrent.
pub(crate) fn load_resume(path: &Path, info_hash: &Hash, pieces: u32) -> Option<Bitfield> {
    let data = std::fs::read(path).ok()?;
    let rest = data.strip_prefix(RESUME_MAGIC)?;
    let (hash, rest) = rest.split_at_checked(20)?;
    let (len, bits) = rest.split_at_checked(4)?;
    let len = u32::from_be_bytes(len.try_into().ok()?);
    (hash == info_hash && len == pieces).then(|| Bitfield::from_bytes(bits, len))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::metainfo::{make_torrent, parse_torrent};

    fn torrent(files: &[(&str, &[u8])], piece: u64) -> Arc<Info> {
        let name = if files.len() == 1 { files[0].0 } else { "folder" };
        Arc::new(parse_torrent(&make_torrent(name, files, piece, "http://t/a")).unwrap().info)
    }

    #[test]
    fn pieces_span_files() {
        let a: Vec<u8> = (0..10).collect();
        let b: Vec<u8> = (10..13).collect();
        let c: Vec<u8> = (13..30).collect();
        let info = torrent(&[("a", &a), ("sub/b", &b), ("empty", &[]), ("c", &c)], 8);
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::new(dir.path(), info.clone());
        let all: Vec<u8> = (0..30).collect();
        // Write every piece in 4-byte blocks, out of order.
        for piece in (0..info.piece_count()).rev() {
            let size = info.piece_size(piece) as usize;
            let start = piece as usize * 8;
            for begin in (0..size).step_by(4) {
                let end = (begin + 4).min(size);
                s.write(piece, begin as u32, &all[start + begin..start + end]).unwrap();
            }
        }
        assert!((0..info.piece_count()).all(|i| s.verify(i)));
        s.finish().unwrap();
        let root = dir.path().join("folder");
        assert_eq!(std::fs::read(root.join("a")).unwrap(), a);
        assert_eq!(std::fs::read(root.join("sub").join("b")).unwrap(), b);
        assert_eq!(std::fs::read(root.join("c")).unwrap(), c);
        assert_eq!(std::fs::read(root.join("empty")).unwrap(), Vec::<u8>::new());
        assert_eq!(s.read(1, 2, 4).unwrap(), &all[10..14]);
        assert!(s.verify_all().is_complete());
    }

    #[test]
    fn bad_data_fails_the_check_and_ranges_are_checked() {
        let data = vec![1u8; 20];
        let info = torrent(&[("x.bin", &data)], 16);
        let dir = tempfile::tempdir().unwrap();
        let s = Storage::new(dir.path(), info);
        assert!(!s.verify(0), "nothing on disk yet");
        s.write(0, 0, &[1u8; 16]).unwrap();
        s.write(1, 0, &[9u8; 4]).unwrap();
        assert!(s.verify(0));
        assert!(!s.verify(1));
        assert!(s.write(1, 2, &[0; 4]).is_err(), "past the end of the last piece");
        assert!(s.write(2, 0, &[0]).is_err(), "no such piece");
        assert_eq!(s.content_path(), dir.path().join("x.bin"));
    }

    #[test]
    fn resume_state_round_trips_and_checks_its_torrent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("resume.bin");
        let mut have = Bitfield::new(11);
        have.set(3);
        have.set(10);
        save_resume(&path, &[7; 20], &have).unwrap();
        assert_eq!(load_resume(&path, &[7; 20], 11), Some(have));
        assert_eq!(load_resume(&path, &[8; 20], 11), None, "another torrent");
        assert_eq!(load_resume(&path, &[7; 20], 12), None, "another layout");
        std::fs::write(&path, b"junk").unwrap();
        assert_eq!(load_resume(&path, &[7; 20], 11), None);
    }
}
