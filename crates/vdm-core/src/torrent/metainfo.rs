//! What a torrent describes: `.torrent` files (BEP 3) and magnet links (BEP 9).

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;

use percent_encoding::percent_decode_str;
use sha1::{Digest, Sha1};

use super::bencode::{self, Value};
use crate::filename;

pub(crate) type Hash = [u8; 20];

/// Largest piece accepted (real torrents use up to 16-32 MiB).
const MAX_PIECE_LENGTH: u64 = 64 << 20;
/// Most files accepted in one torrent.
const MAX_FILES: usize = 100_000;

/// A whole `.torrent`: the info dictionary and where to find peers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Metainfo {
    pub info_hash: Hash,
    pub info: Info,
    /// The info dictionary exactly as in the file (served to magnet peers).
    pub info_bytes: Vec<u8>,
    /// Tracker URLs, `announce-list` order first, without duplicates.
    pub trackers: Vec<String>,
}

/// The info dictionary: the content's layout and the hash of every piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Info {
    /// Sanitized; the file (single-file torrents) or folder name.
    pub name: String,
    pub piece_length: u64,
    pub pieces: Vec<Hash>,
    pub files: Vec<FileEntry>,
    /// One file named `name` (rather than a folder of files).
    pub single_file: bool,
    /// BEP 27: peers only from the torrent's trackers.
    pub private: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct FileEntry {
    /// Relative to the download's root folder; for single-file torrents just the name.
    pub path: PathBuf,
    pub length: u64,
    /// Where the file starts in the torrent's byte stream.
    pub offset: u64,
    /// BEP 47 padding: zeros that are never written to disk.
    pub pad: bool,
}

impl Info {
    pub fn total_length(&self) -> u64 {
        self.files.last().map_or(0, |f| f.offset + f.length)
    }

    pub fn piece_count(&self) -> u32 {
        u32::try_from(self.pieces.len()).unwrap_or(u32::MAX)
    }

    /// Length of piece `index` (the last one is usually shorter).
    pub fn piece_size(&self, index: u32) -> u64 {
        let start = u64::from(index) * self.piece_length;
        self.piece_length.min(self.total_length().saturating_sub(start))
    }
}

/// Parse a `.torrent` file.
pub(crate) fn parse_torrent(data: &[u8]) -> Result<Metainfo, String> {
    let root = bencode::decode(data).map_err(|e| e.to_string())?;
    let info_bytes = bencode::raw_value(data, "info")
        .map_err(|e| e.to_string())?
        .ok_or("the torrent has no info dictionary")?;
    let info = parse_info(&info_bytes)?;
    let mut trackers = Vec::new();
    if let Some(tiers) = root.get("announce-list").and_then(Value::as_list) {
        for tier in tiers {
            for url in tier.as_list().unwrap_or_default() {
                if let Some(url) = url.as_str() {
                    push_tracker(&mut trackers, url);
                }
            }
        }
    }
    if let Some(url) = root.get("announce").and_then(Value::as_str) {
        push_tracker(&mut trackers, url);
    }
    Ok(Metainfo { info_hash: sha1(&info_bytes), info, info_bytes, trackers })
}

/// Parse an info dictionary (from a `.torrent` file or fetched from peers).
pub(crate) fn parse_info(info_bytes: &[u8]) -> Result<Info, String> {
    let info = bencode::decode(info_bytes).map_err(|e| e.to_string())?;
    if info.as_dict().is_none() {
        return Err("the info dictionary is not a dictionary".into());
    }
    let pieces = info.get("pieces").and_then(Value::as_bytes);
    let Some(pieces) = pieces else {
        return Err(if info.get("meta version").is_some() {
            "BitTorrent v2-only torrents are not supported".into()
        } else {
            "the torrent lists no pieces".into()
        });
    };
    if pieces.is_empty() || pieces.len() % 20 != 0 {
        return Err("the piece hashes are malformed".into());
    }
    let pieces: Vec<Hash> =
        pieces.chunks_exact(20).map(|c| c.try_into().unwrap_or([0; 20])).collect();
    let piece_length = info
        .get("piece length")
        .and_then(Value::as_int)
        .and_then(|n| u64::try_from(n).ok())
        .filter(|&n| n > 0 && n <= MAX_PIECE_LENGTH)
        .ok_or("the piece length is missing or out of range")?;
    let name = info
        .get("name.utf-8")
        .or_else(|| info.get("name"))
        .and_then(Value::as_str)
        .map(|n| filename::sanitize(&n))
        .ok_or("the torrent has no name")?;

    let mut files = Vec::new();
    let mut offset = 0u64;
    let single_file = info.get("files").is_none();
    if single_file {
        let length = length_of(&info)?;
        files.push(FileEntry { path: PathBuf::from(&name), length, offset: 0, pad: false });
        offset = length;
    } else {
        let list = info.get("files").and_then(Value::as_list).ok_or("bad file list")?;
        if list.is_empty() || list.len() > MAX_FILES {
            return Err("the torrent's file list is empty or too long".into());
        }
        for entry in list {
            let length = length_of(entry)?;
            let parts = entry
                .get("path.utf-8")
                .or_else(|| entry.get("path"))
                .and_then(Value::as_list)
                .ok_or("a file has no path")?;
            let mut path = PathBuf::new();
            for part in parts {
                // Each part on its own, so `..`, `/` or `C:` can't leave the folder.
                path.push(filename::sanitize(&part.as_str().ok_or("bad path")?));
            }
            if path.as_os_str().is_empty() {
                return Err("a file has an empty path".into());
            }
            let pad =
                entry.get("attr").and_then(Value::as_str).is_some_and(|attr| attr.contains('p'));
            files.push(FileEntry { path, length, offset, pad });
            offset = offset.checked_add(length).ok_or("the torrent is too large")?;
        }
    }
    if offset == 0 {
        return Err("the torrent is empty".into());
    }
    if offset.div_ceil(piece_length) != pieces.len() as u64 {
        return Err("the piece count doesn't match the content size".into());
    }
    let private = info.get("private").and_then(Value::as_int) == Some(1);
    Ok(Info { name, piece_length, pieces, files, single_file, private })
}

fn length_of(v: &Value) -> Result<u64, String> {
    v.get("length")
        .and_then(Value::as_int)
        .and_then(|n| u64::try_from(n).ok())
        .ok_or_else(|| "a file has no valid length".into())
}

fn push_tracker(list: &mut Vec<String>, url: String) {
    let url = url.trim().to_owned();
    let usable = ["http://", "https://", "udp://"].iter().any(|s| url.starts_with(s));
    if usable && !list.contains(&url) {
        list.push(url);
    }
}

pub(crate) fn sha1(data: &[u8]) -> Hash {
    Sha1::digest(data).into()
}

/// A `magnet:?xt=urn:btih:...` link.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Magnet {
    pub info_hash: Hash,
    /// `dn`: a display name until the real one is known.
    pub name: Option<String>,
    /// `xl`: the content size, when the link says.
    pub size: Option<u64>,
    pub trackers: Vec<String>,
    /// `x.pe`: peers to try directly.
    pub peers: Vec<SocketAddr>,
}

pub(crate) fn is_magnet(url: &str) -> bool {
    url.trim_start().get(..7).is_some_and(|s| s.eq_ignore_ascii_case("magnet:"))
}

pub(crate) fn parse_magnet(url: &str) -> Result<Magnet, String> {
    let url = url.trim();
    if !is_magnet(url) {
        return Err("not a magnet link".into());
    }
    let query = url[7..].trim_start_matches('?');
    let mut magnet = Magnet {
        info_hash: [0; 20],
        name: None,
        size: None,
        trackers: Vec::new(),
        peers: Vec::new(),
    };
    let mut found = false;
    for pair in query.split('&') {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        let value = percent_decode_str(&value.replace('+', " ")).decode_utf8_lossy().into_owned();
        match key.to_ascii_lowercase().as_str() {
            k if k == "xt" || k.starts_with("xt.") => {
                if let Some(hash) = value.strip_prefix("urn:btih:").and_then(parse_btih) {
                    magnet.info_hash = hash;
                    found = true;
                }
            }
            "dn" => {
                magnet.name = Some(filename::sanitize(&value))
                    .filter(|n| !value.trim().is_empty() && !n.is_empty())
            }
            "xl" => magnet.size = value.parse().ok(),
            k if k == "tr" || k.starts_with("tr.") => push_tracker(&mut magnet.trackers, value),
            "x.pe" => {
                if let Some(addr) = parse_peer(&value) {
                    magnet.peers.push(addr);
                }
            }
            _ => {}
        }
    }
    if !found {
        return Err("the magnet link has no BitTorrent info hash (urn:btih)".into());
    }
    Ok(magnet)
}

/// 40 hex digits or 32 base32 characters.
fn parse_btih(text: &str) -> Option<Hash> {
    let text = text.trim();
    let mut out = [0u8; 20];
    match text.len() {
        40 => {
            for (i, byte) in out.iter_mut().enumerate() {
                *byte = u8::from_str_radix(text.get(i * 2..i * 2 + 2)?, 16).ok()?;
            }
            Some(out)
        }
        32 => {
            let mut bits: u64 = 0;
            let mut nbits = 0;
            let mut i = 0;
            for c in text.bytes() {
                let v = match c.to_ascii_uppercase() {
                    c @ b'A'..=b'Z' => c - b'A',
                    c @ b'2'..=b'7' => c - b'2' + 26,
                    _ => return None,
                };
                bits = (bits << 5) | u64::from(v);
                nbits += 5;
                if nbits >= 8 {
                    nbits -= 8;
                    out[i] = (bits >> nbits) as u8;
                    i += 1;
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn parse_peer(text: &str) -> Option<SocketAddr> {
    if let Ok(addr) = text.parse::<SocketAddr>() {
        return Some(addr);
    }
    let (host, port) = text.rsplit_once(':')?;
    let ip: IpAddr = host.trim_matches(['[', ']']).parse().ok()?;
    Some(SocketAddr::new(ip, port.parse().ok()?))
}

/// The info hash as 40 lowercase hex digits.
pub(crate) fn hex(hash: &Hash) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

/// Build a `.torrent` for `files` (path inside the torrent, contents) with one
/// tracker. A single file whose path is `name` makes a single-file torrent.
pub fn make_torrent(
    name: &str,
    files: &[(&str, &[u8])],
    piece_length: u64,
    tracker: &str,
) -> Vec<u8> {
    use std::collections::BTreeMap;
    let all: Vec<u8> = files.iter().flat_map(|(_, data)| data.iter().copied()).collect();
    let pieces: Vec<u8> = all.chunks(piece_length as usize).flat_map(sha1).collect();
    let mut info = BTreeMap::new();
    info.insert(b"name".to_vec(), bencode::bytes(name));
    info.insert(b"piece length".to_vec(), Value::Int(piece_length as i64));
    info.insert(b"pieces".to_vec(), Value::Bytes(pieces));
    if let [(_, data)] = files
        && files[0].0 == name
    {
        info.insert(b"length".to_vec(), Value::Int(data.len() as i64));
    } else {
        let list = files
            .iter()
            .map(|(path, data)| {
                let parts = path.split('/').map(bencode::bytes).collect();
                bencode::dict([
                    ("length", Value::Int(data.len() as i64)),
                    ("path", Value::List(parts)),
                ])
            })
            .collect();
        info.insert(b"files".to_vec(), Value::List(list));
    }
    bencode::dict([("announce", bencode::bytes(tracker)), ("info", Value::Dict(info))]).encode()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_file_torrent() {
        let data = vec![7u8; 100];
        let t = parse_torrent(&make_torrent("a.bin", &[("a.bin", &data)], 32, "http://t/announce"))
            .unwrap();
        assert_eq!(t.info.name, "a.bin");
        assert!(t.info.single_file);
        assert_eq!(t.info.total_length(), 100);
        assert_eq!(t.info.piece_count(), 4);
        assert_eq!(t.info.piece_size(3), 4);
        assert_eq!(t.trackers, ["http://t/announce"]);
        assert_eq!(t.info_hash, sha1(&t.info_bytes));
    }

    #[test]
    fn multi_file_paths_stay_inside_the_folder() {
        let bytes = make_torrent(
            "Album",
            &[("cd1/01.flac", b"abc"), ("../evil", b"de"), ("C:\\x/y", b"f")],
            4,
            "udp://t:1",
        );
        let t = parse_torrent(&bytes).unwrap();
        let paths: Vec<_> = t.info.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(paths[0], PathBuf::from("cd1").join("01.flac"));
        for p in &paths {
            assert!(p.components().all(|c| matches!(c, std::path::Component::Normal(_))), "{p:?}");
        }
        assert_eq!(t.info.files[2].offset, 5);
        assert_eq!(t.info.total_length(), 6);
    }

    #[test]
    fn rejects_inconsistent_torrents() {
        let good = make_torrent("a", &[("a", b"0123456789")], 4, "http://t/a");
        // 10 bytes in 4-byte pieces is 3 pieces; claim 2.
        let info = bencode::raw_value(&good, "info").unwrap().unwrap();
        let mut v = bencode::decode(&info).unwrap();
        if let Value::Dict(d) = &mut v {
            d.insert(b"pieces".to_vec(), Value::Bytes(vec![0; 40]));
        }
        assert!(parse_info(&v.encode()).unwrap_err().contains("piece count"));
        assert!(parse_torrent(b"d8:announce1:xe").is_err());
        let v2 = bencode::dict([
            ("meta version", Value::Int(2)),
            ("name", bencode::bytes("x")),
            ("piece length", Value::Int(16384)),
        ]);
        assert!(parse_info(&v2.encode()).unwrap_err().contains("v2"));
    }

    #[test]
    fn magnet_links() {
        let m = parse_magnet(
            "magnet:?xt=urn:btih:C12FE1C06BBA254A9DC9F519B335AA7C1367A88A&dn=Some+Name%21&tr=udp%3A%2F%2Ft.example%3A80&tr=http%3A%2F%2Fx%2Fa&xl=1234&x.pe=1.2.3.4:6881",
        )
        .unwrap();
        assert_eq!(hex(&m.info_hash), "c12fe1c06bba254a9dc9f519b335aa7c1367a88a");
        assert_eq!(m.name.as_deref(), Some("Some Name!"));
        assert_eq!(m.trackers, ["udp://t.example:80", "http://x/a"]);
        assert_eq!(m.size, Some(1234));
        assert_eq!(m.peers, ["1.2.3.4:6881".parse::<SocketAddr>().unwrap()]);

        // Base32 form of the same hash.
        let b32 = parse_magnet("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK").unwrap();
        assert_eq!(b32.info_hash, m.info_hash);

        assert!(parse_magnet("magnet:?dn=x").is_err());
        assert!(parse_magnet("http://x").is_err());
        assert!(is_magnet("MAGNET:?xt=1"));
    }
}
