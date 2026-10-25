//! Positional writes that do not move the file's shared cursor.

use std::fs::File;
use std::io;

/// Write all of `buf` at `offset` without relying on a shared cursor, so several
/// threads can write disjoint ranges of the same file concurrently.
#[cfg(unix)]
pub(crate) fn write_all_at(file: &File, buf: &[u8], offset: u64) -> io::Result<()> {
    std::os::unix::fs::FileExt::write_all_at(file, buf, offset)
}

/// Write all of `buf` at `offset` without relying on a shared cursor, so several
/// threads can write disjoint ranges of the same file concurrently.
#[cfg(windows)]
pub(crate) fn write_all_at(file: &File, mut buf: &[u8], mut offset: u64) -> io::Result<()> {
    use std::os::windows::fs::FileExt;

    while !buf.is_empty() {
        match file.seek_write(buf, offset) {
            Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
            Ok(n) => {
                buf = &buf[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: usize = 1 << 20;
    const CHUNK: usize = 128 << 10;

    /// Byte `j` of chunk `chunk`. Distinct per chunk so mixed-up ranges show up.
    fn pattern(chunk: usize, j: usize) -> u8 {
        (chunk as u8).wrapping_mul(29) ^ (j as u8)
    }

    #[test]
    fn parallel_writes_to_disjoint_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.bin");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .unwrap();
        file.set_len(LEN as u64).unwrap();

        std::thread::scope(|s| {
            for chunk in 0..LEN / CHUNK {
                let file = &file;
                s.spawn(move || {
                    let buf: Vec<u8> = (0..CHUNK).map(|j| pattern(chunk, j)).collect();
                    write_all_at(file, &buf, (chunk * CHUNK) as u64).unwrap();
                });
            }
        });
        drop(file);

        let data = std::fs::read(&path).unwrap();
        assert_eq!(data.len(), LEN);
        for (i, &byte) in data.iter().enumerate() {
            assert_eq!(byte, pattern(i / CHUNK, i % CHUNK), "mismatch at byte {i}");
        }
    }
}
