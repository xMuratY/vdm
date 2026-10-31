//! Which blocks to ask which peer for: finish started pieces first, then start
//! the rarest piece the peer has; near the end ("endgame") ask a second peer
//! for blocks still outstanding, so one slow peer can't hold up the finish.

use std::collections::BTreeMap;

use super::metainfo::Info;
use super::random_u64;
use super::wire::{BLOCK, Bitfield};

pub(crate) type PeerKey = u64;

/// One block to request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct BlockRef {
    pub piece: u32,
    pub begin: u32,
    pub length: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Block {
    Free,
    /// Asked of these peers (more than one only in endgame).
    Requested(Vec<PeerKey>),
    Received,
}

#[derive(Debug)]
struct Piece {
    blocks: Vec<Block>,
}

impl Piece {
    fn received(&self) -> usize {
        self.blocks.iter().filter(|b| **b == Block::Received).count()
    }
}

/// What happened when a block arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arrival {
    /// Nobody asked for it, or it came already (endgame duplicate): drop it.
    Unwanted,
    /// Keep it; the piece still misses blocks.
    Stored,
    /// Keep it; that was the piece's last block: check it now.
    PieceDone,
}

#[derive(Debug)]
pub(crate) struct Picker {
    sizes: Vec<u32>,
    have: Bitfield,
    /// How many connected peers have each piece.
    availability: Vec<u32>,
    /// Pieces with blocks requested or received, but not yet checked.
    active: BTreeMap<u32, Piece>,
}

impl Picker {
    pub fn new(info: &Info, have: Bitfield) -> Self {
        let count = info.piece_count();
        Self {
            sizes: (0..count).map(|i| info.piece_size(i) as u32).collect(),
            have,
            availability: vec![0; count as usize],
            active: BTreeMap::new(),
        }
    }

    pub fn have(&self) -> &Bitfield {
        &self.have
    }

    pub fn is_complete(&self) -> bool {
        self.have.is_complete()
    }

    /// Bytes in checked pieces plus blocks received for pieces in progress.
    pub fn downloaded(&self) -> u64 {
        let done: u64 = (0..self.have.len())
            .filter(|&i| self.have.has(i))
            .map(|i| u64::from(self.sizes[i as usize]))
            .sum();
        let partial: u64 =
            self.active.values().map(|p| p.received() as u64 * u64::from(BLOCK)).sum();
        done + partial
    }

    pub fn left(&self) -> u64 {
        let total: u64 = self.sizes.iter().map(|&s| u64::from(s)).sum();
        total.saturating_sub(self.downloaded())
    }

    pub fn peer_joined(&mut self, has: &Bitfield) {
        for i in 0..has.len() {
            if has.has(i) {
                self.availability[i as usize] += 1;
            }
        }
    }

    pub fn peer_left(&mut self, key: PeerKey, has: &Bitfield) {
        for i in 0..has.len() {
            if has.has(i) {
                self.availability[i as usize] = self.availability[i as usize].saturating_sub(1);
            }
        }
        self.release_peer(key);
    }

    pub fn peer_has(&mut self, index: u32) {
        if let Some(a) = self.availability.get_mut(index as usize) {
            *a += 1;
        }
    }

    /// Whether a peer with `has` has anything we still need.
    pub fn wants_from(&self, has: &Bitfield) -> bool {
        (0..has.len()).any(|i| has.has(i) && !self.have.has(i))
    }

    fn block_count(&self, piece: u32) -> usize {
        (self.sizes[piece as usize] as usize).div_ceil(BLOCK as usize)
    }

    fn block_ref(&self, piece: u32, block: usize) -> BlockRef {
        let begin = block as u32 * BLOCK;
        BlockRef { piece, begin, length: BLOCK.min(self.sizes[piece as usize] - begin) }
    }

    /// Up to `n` blocks for `peer` (which has `has`), marked as requested by it.
    pub fn pick(&mut self, peer: PeerKey, has: &Bitfield, n: usize) -> Vec<BlockRef> {
        let mut out = Vec::new();
        // 1. Blocks of pieces already started.
        let started: Vec<u32> = self.active.keys().copied().filter(|&p| has.has(p)).collect();
        self.take_free(peer, &started, n, &mut out);
        // 2. New pieces, rarest first (ties broken at random).
        while out.len() < n {
            let Some(piece) = self.rarest_new(has) else { break };
            let blocks = vec![Block::Free; self.block_count(piece)];
            self.active.insert(piece, Piece { blocks });
            self.take_free(peer, &[piece], n, &mut out);
        }
        // 3. Endgame: nothing left to start, so double up on outstanding blocks.
        if out.is_empty() {
            for &piece in &started {
                for (i, block) in self
                    .active
                    .get_mut(&piece)
                    .into_iter()
                    .flat_map(|p| p.blocks.iter_mut().enumerate())
                {
                    if out.len() >= n {
                        break;
                    }
                    if let Block::Requested(peers) = block
                        && peers.len() < 2
                        && !peers.contains(&peer)
                    {
                        peers.push(peer);
                        out.push((piece, i));
                    }
                }
            }
            return out.into_iter().map(|(p, i)| self.block_ref(p, i)).collect();
        }
        out.into_iter().map(|(p, i)| self.block_ref(p, i)).collect()
    }

    fn take_free(&mut self, peer: PeerKey, pieces: &[u32], n: usize, out: &mut Vec<(u32, usize)>) {
        for &piece in pieces {
            let Some(p) = self.active.get_mut(&piece) else { continue };
            for (i, block) in p.blocks.iter_mut().enumerate() {
                if out.len() >= n {
                    return;
                }
                if *block == Block::Free {
                    *block = Block::Requested(vec![peer]);
                    out.push((piece, i));
                }
            }
        }
    }

    fn rarest_new(&self, has: &Bitfield) -> Option<u32> {
        let count = self.have.len();
        if count == 0 {
            return None;
        }
        let start = (random_u64() % u64::from(count)) as u32;
        let mut best: Option<(u32, u32)> = None;
        for k in 0..count {
            let i = (start + k) % count;
            if self.have.has(i) || !has.has(i) || self.active.contains_key(&i) {
                continue;
            }
            let avail = self.availability[i as usize];
            if best.is_none_or(|(_, a)| avail < a) {
                best = Some((i, avail));
            }
        }
        best.map(|(i, _)| i)
    }

    /// A block arrived from `peer`.
    pub fn arrived(&mut self, peer: PeerKey, piece: u32, begin: u32) -> Arrival {
        let Some(p) = self.active.get_mut(&piece) else { return Arrival::Unwanted };
        let index = (begin / BLOCK) as usize;
        match p.blocks.get(index) {
            Some(Block::Requested(peers))
                if begin.is_multiple_of(BLOCK) && peers.contains(&peer) => {}
            _ => return Arrival::Unwanted,
        }
        p.blocks[index] = Block::Received;
        if p.received() == p.blocks.len() { Arrival::PieceDone } else { Arrival::Stored }
    }

    /// The outcome of checking a piece whose blocks all arrived.
    pub fn checked(&mut self, piece: u32, ok: bool) {
        if ok {
            self.active.remove(&piece);
            self.have.set(piece);
        } else if let Some(p) = self.active.get_mut(&piece) {
            p.blocks.iter_mut().for_each(|b| *b = Block::Free);
        }
    }

    /// Forget `peer`'s request for one block (it choked us or rejected it).
    pub fn release(&mut self, peer: PeerKey, piece: u32, begin: u32) {
        if let Some(p) = self.active.get_mut(&piece)
            && let Some(block) = p.blocks.get_mut((begin / BLOCK) as usize)
        {
            Self::drop_requester(block, peer);
        }
    }

    pub fn release_peer(&mut self, peer: PeerKey) {
        for p in self.active.values_mut() {
            p.blocks.iter_mut().for_each(|b| Self::drop_requester(b, peer));
        }
    }

    fn drop_requester(block: &mut Block, peer: PeerKey) {
        if let Block::Requested(peers) = block {
            peers.retain(|&k| k != peer);
            if peers.is_empty() {
                *block = Block::Free;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::torrent::metainfo::{make_torrent, parse_torrent};

    /// 4 pieces of 40 KiB (3 blocks each: 16 + 16 + 8 KiB), the last 20 KiB.
    fn picker() -> Picker {
        let data = vec![0u8; 140 * 1024];
        let info = parse_torrent(&make_torrent("f", &[("f", &data)], 40 * 1024, "http://t/a"))
            .unwrap()
            .info;
        Picker::new(&info, Bitfield::new(info.piece_count()))
    }

    #[test]
    fn rarest_piece_first_then_finish_it() {
        let mut p = picker();
        let all = Bitfield::full(4);
        let mut not0 = Bitfield::new(4);
        for i in 1..4 {
            not0.set(i);
        }
        p.peer_joined(&all);
        p.peer_joined(&all);
        p.peer_joined(&not0);
        // Piece 0 is held by two peers, the others by three: asking peer 1 for
        // two blocks starts piece 0, the rarest.
        let first = p.pick(1, &all, 2);
        assert_eq!(first.len(), 2);
        assert_eq!(first[0].piece, 0);
        assert_eq!((first[0].begin, first[1].begin), (0, BLOCK));
        // Peer 2 continues that piece before starting another.
        let next = p.pick(2, &all, 1);
        assert_eq!(next[0], BlockRef { piece: first[0].piece, begin: 2 * BLOCK, length: 8 * 1024 });
    }

    #[test]
    fn a_piece_completes_and_a_bad_one_is_retried() {
        let mut p = picker();
        // A peer with only piece 0 (three blocks), so the pick isn't left to chance.
        let mut all = Bitfield::new(4);
        all.set(0);
        p.peer_joined(&all);
        let blocks = p.pick(1, &all, 3);
        let piece = blocks[0].piece;
        assert!(blocks.iter().all(|b| b.piece == piece));
        assert_eq!(p.arrived(1, piece, 0), Arrival::Stored);
        assert_eq!(p.arrived(2, piece, BLOCK), Arrival::Unwanted, "not asked of peer 2");
        assert_eq!(p.arrived(1, piece, BLOCK), Arrival::Stored);
        assert_eq!(p.arrived(1, piece, 2 * BLOCK), Arrival::PieceDone);
        p.checked(piece, false);
        // The failed piece's blocks are free again.
        let again = p.pick(1, &all, 3);
        assert!(again.iter().all(|b| b.piece == piece));
        for b in &again {
            p.arrived(1, b.piece, b.begin);
        }
        p.checked(piece, true);
        assert!(p.have().has(piece));
        assert_eq!(p.downloaded(), 40 * 1024);
    }

    #[test]
    fn released_blocks_go_to_other_peers_and_endgame_doubles_up() {
        let mut p = picker();
        let mut only3 = Bitfield::new(4);
        only3.set(3);
        p.peer_joined(&only3);
        p.peer_joined(&only3);
        let a = p.pick(1, &only3, 10);
        assert_eq!(a.len(), 2, "piece 3 has two blocks");
        // Nothing free is left for peer 2: endgame asks it for the same blocks.
        let b = p.pick(2, &only3, 10);
        assert_eq!(b, a);
        // A third peer gets nothing: two requesters per block at most.
        assert!(p.pick(3, &only3, 10).is_empty());
        // Peer 1 leaves; peer 2 still has them, so they aren't free yet...
        p.peer_left(1, &only3);
        assert!(p.pick(4, &only3, 10).len() == 2, "...but endgame offers them again");
        assert_eq!(p.arrived(2, 3, 0), Arrival::Stored);
        assert_eq!(p.arrived(4, 3, 0), Arrival::Unwanted, "duplicate");
    }

    #[test]
    fn interest_and_left() {
        let mut p = picker();
        let mut has = Bitfield::new(4);
        assert!(!p.wants_from(&has));
        has.set(1);
        assert!(p.wants_from(&has));
        assert_eq!(p.left(), 140 * 1024);
        for b in p.pick(1, &has, 3) {
            p.arrived(1, b.piece, b.begin);
        }
        p.checked(1, true);
        assert!(!p.wants_from(&has));
        assert_eq!(p.left(), 100 * 1024);
    }
}
