//! The peer wire protocol (BEP 3) with the extension protocol (BEP 10),
//! metadata exchange (BEP 9) and peer exchange (BEP 11).

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::bencode::{self, Value};
use super::metainfo::Hash;

const PROTOCOL: &[u8; 19] = b"BitTorrent protocol";
pub(crate) const HANDSHAKE_LEN: usize = 68;
/// Bit 20 of the reserved bytes: the peer speaks the extension protocol.
const EXTENSION_BIT: u8 = 0x10;
/// Requests are for 16 KiB blocks; peers may refuse larger ones.
pub(crate) const BLOCK: u32 = 16 * 1024;
/// Largest message accepted: a block plus headers, or a big bitfield.
pub(crate) const MAX_MESSAGE: usize = 1 << 20;
/// Our ids for the extensions we speak (sent in our extension handshake).
pub(crate) const UT_METADATA: u8 = 1;
pub(crate) const UT_PEX: u8 = 2;
/// Metadata travels in 16 KiB pieces; a larger info dictionary is refused.
pub(crate) const METADATA_PIECE: usize = 16 * 1024;
pub(crate) const MAX_METADATA: usize = 16 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Handshake {
    pub info_hash: Hash,
    pub peer_id: [u8; 20],
    pub extensions: bool,
}

impl Handshake {
    pub fn encode(&self) -> [u8; HANDSHAKE_LEN] {
        let mut out = [0u8; HANDSHAKE_LEN];
        out[0] = 19;
        out[1..20].copy_from_slice(PROTOCOL);
        if self.extensions {
            out[25] |= EXTENSION_BIT;
        }
        out[28..48].copy_from_slice(&self.info_hash);
        out[48..68].copy_from_slice(&self.peer_id);
        out
    }

    pub fn decode(buf: &[u8; HANDSHAKE_LEN]) -> Option<Self> {
        if buf[0] != 19 || &buf[1..20] != PROTOCOL {
            return None;
        }
        Some(Self {
            info_hash: buf[28..48].try_into().ok()?,
            peer_id: buf[48..68].try_into().ok()?,
            extensions: buf[25] & EXTENSION_BIT != 0,
        })
    }
}

pub(crate) async fn write_handshake<S: AsyncWrite + Unpin>(
    stream: &mut S,
    hs: &Handshake,
) -> std::io::Result<()> {
    stream.write_all(&hs.encode()).await
}

pub(crate) async fn read_handshake<S: AsyncRead + Unpin>(
    stream: &mut S,
) -> std::io::Result<Handshake> {
    let mut buf = [0u8; HANDSHAKE_LEN];
    stream.read_exact(&mut buf).await?;
    Handshake::decode(&buf).ok_or_else(|| invalid("not a BitTorrent handshake"))
}

fn invalid(msg: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, msg.to_owned())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Message {
    KeepAlive,
    Choke,
    Unchoke,
    Interested,
    NotInterested,
    Have(u32),
    Bitfield(Vec<u8>),
    Request {
        index: u32,
        begin: u32,
        length: u32,
    },
    Piece {
        index: u32,
        begin: u32,
        data: Vec<u8>,
    },
    Cancel {
        index: u32,
        begin: u32,
        length: u32,
    },
    /// BEP 5: the peer's DHT port.
    Port(u16),
    /// BEP 10: `id` 0 is the extension handshake.
    Extended {
        id: u8,
        payload: Vec<u8>,
    },
    Unknown(u8),
}

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut body: Vec<u8> = Vec::new();
        let id: Option<u8> = match self {
            Message::KeepAlive => None,
            Message::Choke => Some(0),
            Message::Unchoke => Some(1),
            Message::Interested => Some(2),
            Message::NotInterested => Some(3),
            Message::Have(i) => {
                body.extend_from_slice(&i.to_be_bytes());
                Some(4)
            }
            Message::Bitfield(bits) => {
                body.extend_from_slice(bits);
                Some(5)
            }
            Message::Request { index, begin, length }
            | Message::Cancel { index, begin, length } => {
                for n in [index, begin, length] {
                    body.extend_from_slice(&n.to_be_bytes());
                }
                Some(if matches!(self, Message::Request { .. }) { 6 } else { 8 })
            }
            Message::Piece { index, begin, data } => {
                body.extend_from_slice(&index.to_be_bytes());
                body.extend_from_slice(&begin.to_be_bytes());
                body.extend_from_slice(data);
                Some(7)
            }
            Message::Port(p) => {
                body.extend_from_slice(&p.to_be_bytes());
                Some(9)
            }
            Message::Extended { id, payload } => {
                body.push(*id);
                body.extend_from_slice(payload);
                Some(20)
            }
            Message::Unknown(id) => Some(*id),
        };
        let len = body.len() + usize::from(id.is_some());
        let mut out = Vec::with_capacity(4 + len);
        out.extend_from_slice(&(len as u32).to_be_bytes());
        out.extend(id);
        out.extend_from_slice(&body);
        out
    }

    /// Decode a message body (without its 4-byte length prefix).
    pub fn decode(frame: &[u8]) -> Result<Self, String> {
        let Some((&id, rest)) = frame.split_first() else { return Ok(Message::KeepAlive) };
        let u32_at = |i: usize| -> Result<u32, String> {
            rest.get(i..i + 4)
                .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                .ok_or_else(|| format!("message {id} is too short"))
        };
        let exact = |n: usize| -> Result<(), String> {
            if rest.len() == n { Ok(()) } else { Err(format!("message {id} has the wrong length")) }
        };
        Ok(match id {
            0 => Message::Choke,
            1 => Message::Unchoke,
            2 => Message::Interested,
            3 => Message::NotInterested,
            4 => {
                exact(4)?;
                Message::Have(u32_at(0)?)
            }
            5 => Message::Bitfield(rest.to_vec()),
            6 | 8 => {
                exact(12)?;
                let (index, begin, length) = (u32_at(0)?, u32_at(4)?, u32_at(8)?);
                if id == 6 {
                    Message::Request { index, begin, length }
                } else {
                    Message::Cancel { index, begin, length }
                }
            }
            7 => {
                let (index, begin) = (u32_at(0)?, u32_at(4)?);
                Message::Piece { index, begin, data: rest[8..].to_vec() }
            }
            9 => {
                exact(2)?;
                Message::Port(u16::from_be_bytes([rest[0], rest[1]]))
            }
            20 => {
                let (&ext, payload) = rest.split_first().ok_or("empty extended message")?;
                Message::Extended { id: ext, payload: payload.to_vec() }
            }
            other => Message::Unknown(other),
        })
    }
}

/// Take one complete frame (length prefix plus body) off the front of `buf`.
/// `Ok(None)`: more bytes are needed.
pub(crate) fn take_frame(buf: &mut Vec<u8>) -> Result<Option<Message>, String> {
    if buf.len() < 4 {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len > MAX_MESSAGE {
        return Err(format!("message of {len} bytes is too large"));
    }
    if buf.len() < 4 + len {
        return Ok(None);
    }
    let message = Message::decode(&buf[4..4 + len]);
    buf.drain(..4 + len);
    message.map(Some)
}

// ------------------------------------------------------------ bitfields

/// Which pieces someone has, one bit per piece, high bit first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Bitfield {
    bits: Vec<u8>,
    len: u32,
}

impl Bitfield {
    pub fn new(len: u32) -> Self {
        Self { bits: vec![0; (len as usize).div_ceil(8)], len }
    }

    pub fn full(len: u32) -> Self {
        let mut b = Self::new(len);
        for i in 0..len {
            b.set(i);
        }
        b
    }

    /// From a peer's bitfield message; spare bits past `len` must be clear.
    pub fn from_bytes(bytes: &[u8], len: u32) -> Option<Self> {
        let b = Self { bits: bytes.to_vec(), len };
        let ok = bytes.len() == (len as usize).div_ceil(8)
            && (len..b.bits.len() as u32 * 8).all(|i| !b.raw(i));
        ok.then_some(b)
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bits
    }

    pub fn len(&self) -> u32 {
        self.len
    }

    fn raw(&self, i: u32) -> bool {
        self.bits.get((i / 8) as usize).is_some_and(|b| b & (0x80 >> (i % 8)) != 0)
    }

    pub fn has(&self, i: u32) -> bool {
        i < self.len && self.raw(i)
    }

    pub fn set(&mut self, i: u32) {
        if i < self.len {
            self.bits[(i / 8) as usize] |= 0x80 >> (i % 8);
        }
    }

    pub fn count(&self) -> u32 {
        self.bits.iter().map(|b| b.count_ones()).sum()
    }

    pub fn is_complete(&self) -> bool {
        self.count() == self.len
    }
}

// ------------------------------------------------------------ extensions

/// What a peer said in its extension handshake.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct ExtHandshake {
    /// The peer's message id for ut_metadata, if it serves metadata.
    pub ut_metadata: Option<u8>,
    pub ut_pex: Option<u8>,
    pub metadata_size: Option<usize>,
    /// Requests the peer queues before dropping more (`reqq`).
    pub reqq: Option<u32>,
}

/// Our extension handshake. `metadata_size` once we have the info dictionary.
pub(crate) fn ext_handshake(metadata_size: Option<usize>, port: Option<u16>) -> Message {
    let m = bencode::dict([
        ("ut_metadata", Value::Int(i64::from(UT_METADATA))),
        ("ut_pex", Value::Int(i64::from(UT_PEX))),
    ]);
    let mut d = std::collections::BTreeMap::new();
    d.insert(b"m".to_vec(), m);
    d.insert(b"v".to_vec(), bencode::bytes(concat!("Vibe ", env!("CARGO_PKG_VERSION"))));
    d.insert(b"reqq".to_vec(), Value::Int(250));
    if let Some(size) = metadata_size {
        d.insert(b"metadata_size".to_vec(), Value::Int(size as i64));
    }
    if let Some(port) = port {
        d.insert(b"p".to_vec(), Value::Int(i64::from(port)));
    }
    Message::Extended { id: 0, payload: Value::Dict(d).encode() }
}

pub(crate) fn parse_ext_handshake(payload: &[u8]) -> Option<ExtHandshake> {
    let v = bencode::decode(payload).ok()?;
    let id = |name: &str| {
        v.get("m")?.get(name)?.as_int().and_then(|i| u8::try_from(i).ok()).filter(|&i| i != 0)
    };
    Some(ExtHandshake {
        ut_metadata: id("ut_metadata"),
        ut_pex: id("ut_pex"),
        metadata_size: v
            .get("metadata_size")
            .and_then(Value::as_int)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| n > 0 && n <= MAX_METADATA),
        reqq: v.get("reqq").and_then(Value::as_int).and_then(|n| u32::try_from(n).ok()),
    })
}

/// A BEP 9 message: request (0), data (1) or reject (2) for one metadata piece.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MetadataMsg {
    Request(u32),
    Data { piece: u32, total_size: usize, data: Vec<u8> },
    Reject(u32),
}

impl MetadataMsg {
    /// As an extended message, using the id the receiving peer asked for.
    pub fn to_message(&self, peer_id: u8) -> Message {
        let header = |kind: i64, piece: u32, size: Option<usize>| {
            let mut d = std::collections::BTreeMap::new();
            d.insert(b"msg_type".to_vec(), Value::Int(kind));
            d.insert(b"piece".to_vec(), Value::Int(i64::from(piece)));
            if let Some(size) = size {
                d.insert(b"total_size".to_vec(), Value::Int(size as i64));
            }
            Value::Dict(d).encode()
        };
        let payload = match self {
            MetadataMsg::Request(p) => header(0, *p, None),
            MetadataMsg::Reject(p) => header(2, *p, None),
            MetadataMsg::Data { piece, total_size, data } => {
                let mut out = header(1, *piece, Some(*total_size));
                out.extend_from_slice(data);
                out
            }
        };
        Message::Extended { id: peer_id, payload }
    }

    pub fn parse(payload: &[u8]) -> Option<Self> {
        let (header, used) = bencode::decode_prefix(payload).ok()?;
        let piece = u32::try_from(header.get("piece")?.as_int()?).ok()?;
        match header.get("msg_type")?.as_int()? {
            0 => Some(MetadataMsg::Request(piece)),
            1 => Some(MetadataMsg::Data {
                piece,
                total_size: usize::try_from(header.get("total_size")?.as_int()?).ok()?,
                data: payload[used..].to_vec(),
            }),
            2 => Some(MetadataMsg::Reject(piece)),
            _ => None,
        }
    }
}

/// Peers a BEP 11 message adds (IPv4 and IPv6).
pub(crate) fn parse_pex(payload: &[u8]) -> Vec<SocketAddr> {
    let Ok(v) = bencode::decode(payload) else { return Vec::new() };
    let mut out = Vec::new();
    if let Some(b) = v.get("added").and_then(Value::as_bytes) {
        out.extend(compact_peers_v4(b));
    }
    if let Some(b) = v.get("added6").and_then(Value::as_bytes) {
        out.extend(compact_peers_v6(b));
    }
    out
}

/// 6-byte entries: IPv4 address and port.
pub(crate) fn compact_peers_v4(b: &[u8]) -> Vec<SocketAddr> {
    b.chunks_exact(6)
        .map(|c| {
            let ip = Ipv4Addr::new(c[0], c[1], c[2], c[3]);
            SocketAddr::new(IpAddr::V4(ip), u16::from_be_bytes([c[4], c[5]]))
        })
        .filter(|a| a.port() != 0)
        .collect()
}

/// 18-byte entries: IPv6 address and port.
pub(crate) fn compact_peers_v6(b: &[u8]) -> Vec<SocketAddr> {
    b.chunks_exact(18)
        .filter_map(|c| {
            let ip: [u8; 16] = c[..16].try_into().ok()?;
            let addr =
                SocketAddr::new(IpAddr::V6(Ipv6Addr::from(ip)), u16::from_be_bytes([c[16], c[17]]));
            (addr.port() != 0).then_some(addr)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_round_trip() {
        let hs = Handshake { info_hash: [1; 20], peer_id: [2; 20], extensions: true };
        let bytes = hs.encode();
        assert_eq!(&bytes[1..20], PROTOCOL);
        assert_eq!(Handshake::decode(&bytes), Some(hs));
        let mut bad = bytes;
        bad[0] = 18;
        assert_eq!(Handshake::decode(&bad), None);
    }

    #[test]
    fn messages_round_trip_through_frames() {
        let msgs = vec![
            Message::KeepAlive,
            Message::Unchoke,
            Message::Have(7),
            Message::Bitfield(vec![0b1010_0000]),
            Message::Request { index: 1, begin: 16384, length: 16384 },
            Message::Piece { index: 1, begin: 0, data: vec![9; 10] },
            Message::Cancel { index: 2, begin: 3, length: 4 },
            Message::Port(6881),
            Message::Extended { id: 3, payload: b"d1:ai1ee".to_vec() },
        ];
        let mut buf: Vec<u8> = msgs.iter().flat_map(Message::encode).collect();
        // Split mid-frame: the first take only gets what is complete.
        let tail = buf.split_off(buf.len() - 3);
        let mut got = Vec::new();
        while let Some(m) = take_frame(&mut buf).unwrap() {
            got.push(m);
        }
        assert_eq!(got.len(), msgs.len() - 1);
        buf.extend(tail);
        got.push(take_frame(&mut buf).unwrap().unwrap());
        assert_eq!(got, msgs);
        assert!(buf.is_empty());
    }

    #[test]
    fn oversized_and_short_messages_are_errors() {
        let mut huge = (MAX_MESSAGE as u32 + 1).to_be_bytes().to_vec();
        assert!(take_frame(&mut huge).is_err());
        assert!(Message::decode(&[4, 0, 0]).is_err());
        assert!(Message::decode(&[6, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn bitfields() {
        let mut b = Bitfield::new(10);
        b.set(0);
        b.set(9);
        b.set(10); // out of range: ignored
        assert_eq!(b.bytes(), &[0x80, 0x40]);
        assert_eq!(b.count(), 2);
        assert!(b.has(9) && !b.has(10));
        assert!(Bitfield::from_bytes(&[0xff, 0xc0], 10).unwrap().is_complete());
        assert!(Bitfield::from_bytes(&[0xff, 0xe0], 10).is_none(), "spare bit set");
        assert!(Bitfield::from_bytes(&[0xff], 10).is_none(), "too short");
        assert!(Bitfield::full(9).is_complete());
    }

    #[test]
    fn extension_handshake_and_metadata_messages() {
        let Message::Extended { id: 0, payload } = ext_handshake(Some(500), Some(6881)) else {
            panic!()
        };
        let parsed = parse_ext_handshake(&payload).unwrap();
        assert_eq!(parsed.ut_metadata, Some(UT_METADATA));
        assert_eq!(parsed.metadata_size, Some(500));

        let data = MetadataMsg::Data { piece: 1, total_size: 20000, data: vec![5; 3616] };
        let Message::Extended { id: 9, payload } = data.to_message(9) else { panic!() };
        assert_eq!(MetadataMsg::parse(&payload), Some(data));
        let Message::Extended { payload, .. } = MetadataMsg::Request(2).to_message(1) else {
            panic!()
        };
        assert_eq!(MetadataMsg::parse(&payload), Some(MetadataMsg::Request(2)));
    }

    #[test]
    fn peer_exchange_and_compact_peers() {
        let added = [10, 0, 0, 1, 0x1a, 0xe1, 10, 0, 0, 2, 0, 0];
        let msg = bencode::dict([("added", bencode::bytes(added.to_vec()))]).encode();
        assert_eq!(parse_pex(&msg), ["10.0.0.1:6881".parse::<SocketAddr>().unwrap()]);
        let mut v6 = vec![0u8; 16];
        v6[15] = 1;
        v6.extend_from_slice(&[0x1a, 0xe1]);
        assert_eq!(compact_peers_v6(&v6), ["[::1]:6881".parse::<SocketAddr>().unwrap()]);
    }
}
