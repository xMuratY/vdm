//! Finding peers without a tracker: a read-only client of the BitTorrent DHT
//! (BEP 5, read-only per BEP 43).
//!
//! A lookup asks nodes ever closer (by XOR distance) to the info hash for
//! peers; each reply holds peers, closer nodes, or both. We never store data
//! for others, so nothing needs to listen for incoming queries.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use tokio::net::UdpSocket;

use super::bencode::{self, Value};
use super::metainfo::Hash;
use super::random_u64;
use super::wire::compact_peers_v4;

/// Well-known entry points into the DHT.
pub const BOOTSTRAP: [&str; 4] = [
    "router.bittorrent.com:6881",
    "dht.transmissionbt.com:6881",
    "router.utorrent.com:6881",
    "dht.libtorrent.org:25401",
];

/// Queries in flight at once.
const ALPHA: usize = 8;
/// A lookup ends when this many of the closest known nodes have answered.
const K: usize = 8;
const QUERY_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_NODES: usize = 1000;

#[derive(Debug, Clone)]
struct Node {
    id: Hash,
    addr: SocketAddr,
    state: NodeState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NodeState {
    New,
    Asked,
    Answered,
    Failed,
}

fn distance(a: &Hash, b: &Hash) -> Hash {
    let mut d = [0u8; 20];
    for (i, byte) in d.iter_mut().enumerate() {
        *byte = a[i] ^ b[i];
    }
    d
}

fn random_id() -> Hash {
    let mut id = [0u8; 20];
    for chunk in id.chunks_mut(8) {
        let bytes = random_u64().to_be_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    id
}

/// A get_peers query (read-only: `ro` = 1).
pub(crate) fn get_peers_query(tid: [u8; 2], our_id: &Hash, info_hash: &Hash) -> Vec<u8> {
    let args = bencode::dict([
        ("id", bencode::bytes(our_id.to_vec())),
        ("info_hash", bencode::bytes(info_hash.to_vec())),
    ]);
    bencode::dict([
        ("a", args),
        ("q", bencode::bytes("get_peers")),
        ("ro", Value::Int(1)),
        ("t", bencode::bytes(tid.to_vec())),
        ("y", bencode::bytes("q")),
    ])
    .encode()
}

/// What a node replied: its id, peers for the torrent, and closer nodes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Reply {
    pub tid: Vec<u8>,
    pub id: Option<Hash>,
    pub peers: Vec<SocketAddr>,
    pub nodes: Vec<(Hash, SocketAddr)>,
}

pub(crate) fn parse_reply(data: &[u8]) -> Option<Reply> {
    let v = bencode::decode(data).ok()?;
    if v.get("y")?.as_bytes()? != b"r" {
        return None;
    }
    let r = v.get("r")?;
    let mut reply = Reply { tid: v.get("t")?.as_bytes()?.to_vec(), ..Reply::default() };
    reply.id = r.get("id").and_then(Value::as_bytes).and_then(|b| b.try_into().ok());
    for value in r.get("values").and_then(Value::as_list).unwrap_or_default() {
        if let Some(b) = value.as_bytes() {
            reply.peers.extend(compact_peers_v4(b));
        }
    }
    if let Some(b) = r.get("nodes").and_then(Value::as_bytes) {
        for chunk in b.as_chunks::<26>().0 {
            let Ok(id) = chunk[..20].try_into() else { continue };
            if let Some(addr) = compact_peers_v4(&chunk[20..]).into_iter().next() {
                reply.nodes.push((id, addr));
            }
        }
    }
    Some(reply)
}

/// Look up peers for `info_hash`, starting from `bootstrap` (host:port),
/// reporting them to `found` as replies arrive. Stops when the closest nodes
/// have all answered or after `limit`. Returns how many peers were found.
pub(crate) async fn get_peers(
    info_hash: Hash,
    bootstrap: &[String],
    limit: Duration,
    mut found: impl FnMut(Vec<SocketAddr>),
) -> Result<usize, String> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.map_err(|e| e.to_string())?;
    let our_id = random_id();
    let deadline = Instant::now() + limit;

    // Bootstrap nodes have no known id yet: they're asked first.
    let mut nodes: Vec<Node> = Vec::new();
    let mut seen: HashSet<SocketAddr> = HashSet::new();
    for host in bootstrap {
        if let Ok(Ok(addrs)) =
            tokio::time::timeout(Duration::from_secs(5), tokio::net::lookup_host(host.as_str()))
                .await
        {
            for addr in addrs.filter(SocketAddr::is_ipv4) {
                if seen.insert(addr) {
                    nodes.push(Node { id: our_id, addr, state: NodeState::New });
                }
            }
        }
    }
    if nodes.is_empty() {
        return Err("no DHT bootstrap node could be reached".into());
    }

    let mut pending: HashMap<[u8; 2], (SocketAddr, Instant)> = HashMap::new();
    let mut peers: HashSet<SocketAddr> = HashSet::new();
    let mut next_tid = random_u64() as u16;
    let mut buf = vec![0u8; 4096];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        // Forget queries that went unanswered.
        pending.retain(|_, (addr, sent)| {
            let alive = now.duration_since(*sent) < QUERY_TIMEOUT;
            if !alive && let Some(n) = nodes.iter_mut().find(|n| n.addr == *addr) {
                n.state = NodeState::Failed;
            }
            alive
        });
        // Ask the closest nodes not yet asked.
        nodes.sort_by_key(|n| distance(&n.id, &info_hash));
        let mut answered_close = 0;
        let mut to_ask = Vec::new();
        for n in nodes.iter_mut() {
            match n.state {
                NodeState::Answered => answered_close += 1,
                NodeState::New if pending.len() + to_ask.len() < ALPHA => {
                    n.state = NodeState::Asked;
                    to_ask.push(n.addr);
                }
                _ => {}
            }
            if answered_close >= K {
                break;
            }
        }
        if to_ask.is_empty() && pending.is_empty() {
            break; // nothing closer left to ask
        }
        for addr in to_ask {
            next_tid = next_tid.wrapping_add(1);
            let tid = next_tid.to_be_bytes();
            if socket.send_to(&get_peers_query(tid, &our_id, &info_hash), addr).await.is_ok() {
                pending.insert(tid, (addr, Instant::now()));
            }
        }
        let wait =
            deadline.saturating_duration_since(Instant::now()).min(Duration::from_millis(500));
        let Ok(Ok((n, from))) = tokio::time::timeout(wait, socket.recv_from(&mut buf)).await else {
            continue;
        };
        let Some(reply) = parse_reply(&buf[..n]) else { continue };
        let Ok(tid) = <[u8; 2]>::try_from(reply.tid.as_slice()) else { continue };
        match pending.get(&tid) {
            Some((addr, _)) if *addr == from => {
                pending.remove(&tid);
            }
            _ => continue,
        }
        if let Some(node) = nodes.iter_mut().find(|n| n.addr == from) {
            node.state = NodeState::Answered;
            if let Some(id) = reply.id {
                node.id = id;
            }
        }
        let fresh: Vec<SocketAddr> = reply.peers.into_iter().filter(|p| peers.insert(*p)).collect();
        if !fresh.is_empty() {
            found(fresh);
        }
        for (id, addr) in reply.nodes {
            if nodes.len() < MAX_NODES && addr.port() != 0 && seen.insert(addr) {
                nodes.push(Node { id, addr, state: NodeState::New });
            }
        }
    }
    Ok(peers.len())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A fake DHT node: answers get_peers with `nodes` and/or `values`.
    pub(crate) async fn fake_node(
        id: Hash,
        nodes: Vec<(Hash, SocketAddr)>,
        values: Vec<SocketAddr>,
    ) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        tokio::spawn(async move {
            let mut buf = [0u8; 1500];
            loop {
                let Ok((n, from)) = socket.recv_from(&mut buf).await else { return };
                let Ok(q) = bencode::decode(&buf[..n]) else { continue };
                let tid = q.get("t").and_then(Value::as_bytes).unwrap_or_default().to_vec();
                let mut compact = Vec::new();
                for (nid, a) in &nodes {
                    if let SocketAddr::V4(v4) = a {
                        compact.extend_from_slice(nid);
                        compact.extend_from_slice(&v4.ip().octets());
                        compact.extend_from_slice(&v4.port().to_be_bytes());
                    }
                }
                let vals: Vec<Value> = values
                    .iter()
                    .filter_map(|a| match a {
                        SocketAddr::V4(v4) => {
                            let mut b = v4.ip().octets().to_vec();
                            b.extend_from_slice(&v4.port().to_be_bytes());
                            Some(Value::Bytes(b))
                        }
                        SocketAddr::V6(_) => None,
                    })
                    .collect();
                let r = bencode::dict([
                    ("id", bencode::bytes(id.to_vec())),
                    ("nodes", Value::Bytes(compact)),
                    ("token", bencode::bytes("tok")),
                    ("values", Value::List(vals)),
                ]);
                let reply =
                    bencode::dict([("r", r), ("t", Value::Bytes(tid)), ("y", bencode::bytes("r"))]);
                let _ = socket.send_to(&reply.encode(), from).await;
            }
        });
        addr
    }

    #[test]
    fn queries_and_replies() {
        let q = get_peers_query([1, 2], &[3; 20], &[4; 20]);
        let v = bencode::decode(&q).unwrap();
        assert_eq!(v.get("q").and_then(Value::as_bytes), Some(&b"get_peers"[..]));
        assert_eq!(v.get("ro").and_then(Value::as_int), Some(1));
        assert_eq!(
            v.get("a").and_then(|a| a.get("info_hash")).and_then(Value::as_bytes),
            Some(&[4u8; 20][..])
        );

        let mut node = vec![9u8; 20];
        node.extend_from_slice(&[10, 0, 0, 1, 0x1a, 0xe1]);
        let reply = bencode::dict([
            (
                "r",
                bencode::dict([
                    ("id", bencode::bytes(vec![7u8; 20])),
                    ("nodes", Value::Bytes(node)),
                    ("values", Value::List(vec![bencode::bytes(vec![1, 2, 3, 4, 0, 80])])),
                ]),
            ),
            ("t", bencode::bytes(vec![1, 2])),
            ("y", bencode::bytes("r")),
        ]);
        let r = parse_reply(&reply.encode()).unwrap();
        assert_eq!(r.id, Some([7; 20]));
        assert_eq!(r.peers, ["1.2.3.4:80".parse::<SocketAddr>().unwrap()]);
        assert_eq!(r.nodes, [([9; 20], "10.0.0.1:6881".parse::<SocketAddr>().unwrap())]);
        assert_eq!(parse_reply(b"d1:y1:ee"), None);
    }

    #[test]
    fn distance_is_xor() {
        let mut a = [0u8; 20];
        a[19] = 0b1010;
        let mut b = [0u8; 20];
        b[19] = 0b0110;
        assert_eq!(distance(&a, &b)[19], 0b1100);
    }

    /// Bootstrap knows only a closer node; that one knows the peer.
    #[tokio::test]
    async fn a_lookup_follows_nodes_to_peers() {
        let hash = [0x55; 20];
        let peer: SocketAddr = "10.9.8.7:51413".parse().unwrap();
        let close = fake_node([0x54; 20], Vec::new(), vec![peer]).await;
        let boot = fake_node([0x00; 20], vec![([0x54; 20], close)], Vec::new()).await;
        let mut got = Vec::new();
        let n = get_peers(hash, &[boot.to_string()], Duration::from_secs(5), |p| got.extend(p))
            .await
            .unwrap();
        assert_eq!(n, 1);
        assert_eq!(got, [peer]);
    }
}
