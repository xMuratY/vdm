//! BitTorrent downloads: `.torrent` files and magnet links, with optional seeding.
//!
//! A small client of our own (no extra crates): trackers over HTTP and UDP,
//! the peer wire protocol with metadata and peer exchange, rarest-first piece
//! picking, SHA-1 checks of every piece, and resume across runs.

pub(crate) mod bencode;
pub(crate) mod metainfo;
pub(crate) mod wire;

pub use metainfo::make_torrent;
pub use run::{Seed, display_name, is_torrent_url};

pub(crate) mod picker;
mod run;
pub(crate) mod session;
pub(crate) mod storage;
pub(crate) mod tracker;

/// 64 random bits from the standard library's per-process random hasher keys
/// (seeded by the OS), mixed with a counter and the time.
pub(crate) fn random_u64() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    h.write_u128(nanos);
    h.finish()
}

/// Our peer id: client and version (Azureus style), then 12 random characters.
pub(crate) fn new_peer_id() -> [u8; 20] {
    const CHARS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut id = *b"-VB0000-000000000000";
    let version: Vec<u8> =
        env!("CARGO_PKG_VERSION").bytes().filter(u8::is_ascii_digit).take(4).collect();
    for (slot, digit) in id[3..7].iter_mut().zip(version) {
        *slot = digit;
    }
    let mut bits = random_u64();
    for (i, slot) in id[8..].iter_mut().enumerate() {
        if i == 6 {
            bits = random_u64();
        }
        *slot = CHARS[(bits % CHARS.len() as u64) as usize];
        bits /= CHARS.len() as u64;
    }
    id
}
