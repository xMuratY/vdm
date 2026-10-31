//! Asking trackers for peers: HTTP(S) (BEP 3, BEP 23 compact peers) and UDP (BEP 15).

use std::net::SocketAddr;
use std::time::Duration;

use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, percent_encode};
use tokio::net::UdpSocket;

use super::bencode::{self, Value};
use super::metainfo::Hash;
use super::random_u64;
use super::wire::{compact_peers_v4, compact_peers_v6};

/// Wait used when a tracker doesn't say (and the shortest we accept).
pub(crate) const DEFAULT_INTERVAL: Duration = Duration::from_secs(30 * 60);
const MIN_INTERVAL: Duration = Duration::from_secs(60);
const UDP_TIMEOUT: Duration = Duration::from_secs(5);
const UDP_TRIES: u32 = 2;
const UDP_PROTOCOL_ID: u64 = 0x0417_2710_1980;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Event {
    None,
    Started,
    Completed,
    Stopped,
}

#[derive(Debug, Clone)]
pub(crate) struct Announce {
    pub info_hash: Hash,
    pub peer_id: [u8; 20],
    /// Where we accept connections; 0 when we don't listen.
    pub port: u16,
    pub uploaded: u64,
    pub downloaded: u64,
    pub left: u64,
    pub event: Event,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Reply {
    pub interval: Duration,
    pub peers: Vec<SocketAddr>,
}

/// Announce to one tracker (http, https or udp).
pub(crate) async fn announce(
    client: &reqwest::Client,
    url: &str,
    a: &Announce,
) -> Result<Reply, String> {
    if let Some(rest) = url.strip_prefix("udp://") {
        announce_udp(rest, a).await
    } else {
        announce_http(client, url, a).await
    }
}

/// Bytes allowed unescaped in the query (RFC 3986 unreserved).
const QUERY: &AsciiSet = &NON_ALPHANUMERIC.remove(b'-').remove(b'.').remove(b'_').remove(b'~');

pub(crate) fn http_url(base: &str, a: &Announce) -> String {
    let event = match a.event {
        Event::None => "",
        Event::Started => "&event=started",
        Event::Completed => "&event=completed",
        Event::Stopped => "&event=stopped",
    };
    let sep = if base.contains('?') { '&' } else { '?' };
    format!(
        "{base}{sep}info_hash={}&peer_id={}&port={}&uploaded={}&downloaded={}&left={}&compact=1&numwant=80{event}",
        percent_encode(&a.info_hash, QUERY),
        percent_encode(&a.peer_id, QUERY),
        a.port,
        a.uploaded,
        a.downloaded,
        a.left,
    )
}

async fn announce_http(client: &reqwest::Client, url: &str, a: &Announce) -> Result<Reply, String> {
    let response = client
        .get(http_url(url, a))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !response.status().is_success() {
        return Err(format!("tracker returned {}", response.status()));
    }
    let body = response.bytes().await.map_err(|e| e.to_string())?;
    parse_http_reply(&body)
}

pub(crate) fn parse_http_reply(body: &[u8]) -> Result<Reply, String> {
    let v = bencode::decode(body).map_err(|e| e.to_string())?;
    if let Some(reason) = v.get("failure reason").and_then(Value::as_str) {
        return Err(format!("tracker says: {reason}"));
    }
    let mut peers = Vec::new();
    match v.get("peers") {
        Some(Value::Bytes(b)) => peers.extend(compact_peers_v4(b)),
        Some(Value::List(list)) => {
            // The original, non-compact form.
            for p in list {
                let ip = p.get("ip").and_then(Value::as_str).and_then(|s| s.parse().ok());
                let port =
                    p.get("port").and_then(Value::as_int).and_then(|n| u16::try_from(n).ok());
                if let (Some(ip), Some(port)) = (ip, port) {
                    peers.push(SocketAddr::new(ip, port));
                }
            }
        }
        _ => {}
    }
    if let Some(b) = v.get("peers6").and_then(Value::as_bytes) {
        peers.extend(compact_peers_v6(b));
    }
    let interval = v
        .get("interval")
        .and_then(Value::as_int)
        .and_then(|n| u64::try_from(n).ok())
        .map_or(DEFAULT_INTERVAL, Duration::from_secs);
    Ok(Reply { interval: interval.max(MIN_INTERVAL), peers })
}

async fn announce_udp(host_port: &str, a: &Announce) -> Result<Reply, String> {
    // "host:port/announce" and friends: only host and port matter.
    let host_port = host_port.split(['/', '?']).next().unwrap_or(host_port);
    let addr = tokio::net::lookup_host(host_port)
        .await
        .map_err(|e| format!("{host_port}: {e}"))?
        .next()
        .ok_or_else(|| format!("{host_port}: no address"))?;
    let bind: SocketAddr = if addr.is_ipv4() {
        "0.0.0.0:0".parse().unwrap_or(addr)
    } else {
        "[::]:0".parse().unwrap_or(addr)
    };
    let socket = UdpSocket::bind(bind).await.map_err(|e| e.to_string())?;
    socket.connect(addr).await.map_err(|e| e.to_string())?;

    let tid = random_u64() as u32;
    let mut connect = Vec::with_capacity(16);
    connect.extend_from_slice(&UDP_PROTOCOL_ID.to_be_bytes());
    connect.extend_from_slice(&0u32.to_be_bytes());
    connect.extend_from_slice(&tid.to_be_bytes());
    let reply = udp_exchange(&socket, &connect, 0, tid).await?;
    let connection_id = u64::from_be_bytes(reply[8..16].try_into().map_err(|_| "short reply")?);

    let tid = random_u64() as u32;
    let request = udp_announce_request(connection_id, tid, a);
    let reply = udp_exchange(&socket, &request, 1, tid).await?;
    parse_udp_reply(&reply, addr.is_ipv6())
}

pub(crate) fn udp_announce_request(connection_id: u64, tid: u32, a: &Announce) -> Vec<u8> {
    let event: u32 = match a.event {
        Event::None => 0,
        Event::Completed => 1,
        Event::Started => 2,
        Event::Stopped => 3,
    };
    let mut r = Vec::with_capacity(98);
    r.extend_from_slice(&connection_id.to_be_bytes());
    r.extend_from_slice(&1u32.to_be_bytes());
    r.extend_from_slice(&tid.to_be_bytes());
    r.extend_from_slice(&a.info_hash);
    r.extend_from_slice(&a.peer_id);
    r.extend_from_slice(&a.downloaded.to_be_bytes());
    r.extend_from_slice(&a.left.to_be_bytes());
    r.extend_from_slice(&a.uploaded.to_be_bytes());
    r.extend_from_slice(&event.to_be_bytes());
    r.extend_from_slice(&0u32.to_be_bytes()); // our IP: as seen by the tracker
    r.extend_from_slice(&(random_u64() as u32).to_be_bytes()); // key
    r.extend_from_slice(&(-1i32).to_be_bytes()); // as many peers as it likes
    r.extend_from_slice(&a.port.to_be_bytes());
    r
}

pub(crate) fn parse_udp_reply(reply: &[u8], ipv6: bool) -> Result<Reply, String> {
    if reply.len() < 20 {
        return Err("short announce reply".into());
    }
    let interval = u32::from_be_bytes([reply[8], reply[9], reply[10], reply[11]]);
    let peers = if ipv6 { compact_peers_v6(&reply[20..]) } else { compact_peers_v4(&reply[20..]) };
    Ok(Reply { interval: Duration::from_secs(u64::from(interval)).max(MIN_INTERVAL), peers })
}

/// Send `request` and wait for the reply to transaction `tid` with `action`.
async fn udp_exchange(
    socket: &UdpSocket,
    request: &[u8],
    action: u32,
    tid: u32,
) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; 2048];
    for _ in 0..UDP_TRIES {
        socket.send(request).await.map_err(|e| e.to_string())?;
        let deadline = tokio::time::Instant::now() + UDP_TIMEOUT;
        loop {
            let n = match tokio::time::timeout_at(deadline, socket.recv(&mut buf)).await {
                Err(_) => break, // try again
                Ok(r) => r.map_err(|e| e.to_string())?,
            };
            if n < 8 || buf[4..8] != tid.to_be_bytes() {
                continue;
            }
            let got = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
            if got == 3 {
                return Err(format!("tracker says: {}", String::from_utf8_lossy(&buf[8..n])));
            }
            if got == action {
                return Ok(buf[..n].to_vec());
            }
        }
    }
    Err("the tracker did not answer".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Announce {
        Announce {
            info_hash: [0xAB; 20],
            peer_id: *b"-VB0100-abcdefghijkl",
            port: 6881,
            uploaded: 1,
            downloaded: 2,
            left: 3,
            event: Event::Started,
        }
    }

    #[test]
    fn http_query_escapes_binary_hashes() {
        let url = http_url("http://t.example/announce?key=1", &sample());
        assert!(url.starts_with("http://t.example/announce?key=1&info_hash=%AB%AB"), "{url}");
        assert!(url.contains("&peer_id=-VB0100-abcdefghijkl&port=6881&"), "{url}");
        assert!(url.ends_with("&event=started"));
    }

    #[test]
    fn http_replies() {
        let compact = bencode::dict([
            ("interval", Value::Int(900)),
            ("peers", bencode::bytes(vec![127, 0, 0, 1, 0x1a, 0xe1])),
        ]);
        let r = parse_http_reply(&compact.encode()).unwrap();
        assert_eq!(r.interval, Duration::from_secs(900));
        assert_eq!(r.peers, ["127.0.0.1:6881".parse::<SocketAddr>().unwrap()]);

        let listed = bencode::dict([(
            "peers",
            Value::List(vec![bencode::dict([
                ("ip", bencode::bytes("10.0.0.9")),
                ("port", Value::Int(51413)),
            ])]),
        )]);
        let r = parse_http_reply(&listed.encode()).unwrap();
        assert_eq!(r.peers, ["10.0.0.9:51413".parse::<SocketAddr>().unwrap()]);
        assert_eq!(r.interval, DEFAULT_INTERVAL);

        let failed = bencode::dict([("failure reason", bencode::bytes("unregistered torrent"))]);
        assert!(parse_http_reply(&failed.encode()).unwrap_err().contains("unregistered"));
    }

    #[test]
    fn udp_messages() {
        let req = udp_announce_request(42, 7, &sample());
        assert_eq!(req.len(), 98);
        assert_eq!(&req[..8], &42u64.to_be_bytes());
        assert_eq!(&req[96..], &6881u16.to_be_bytes());

        let mut reply = Vec::new();
        reply.extend_from_slice(&1u32.to_be_bytes());
        reply.extend_from_slice(&7u32.to_be_bytes());
        reply.extend_from_slice(&1800u32.to_be_bytes());
        reply.extend_from_slice(&[0; 8]);
        reply.extend_from_slice(&[192, 168, 1, 2, 0x1a, 0xe1]);
        let r = parse_udp_reply(&reply, false).unwrap();
        assert_eq!(r.interval, Duration::from_secs(1800));
        assert_eq!(r.peers, ["192.168.1.2:6881".parse::<SocketAddr>().unwrap()]);
    }

    /// A UDP tracker on localhost, answering one connect and one announce.
    #[tokio::test]
    async fn udp_announce_against_a_local_tracker() {
        let tracker = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let port = tracker.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let mut buf = [0u8; 200];
            let (n, from) = tracker.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 16);
            let mut out = vec![0, 0, 0, 0];
            out.extend_from_slice(&buf[12..16]);
            out.extend_from_slice(&99u64.to_be_bytes());
            tracker.send_to(&out, from).await.unwrap();
            let (n, from) = tracker.recv_from(&mut buf).await.unwrap();
            assert_eq!(n, 98);
            assert_eq!(&buf[..8], &99u64.to_be_bytes());
            let mut out = vec![0, 0, 0, 1];
            out.extend_from_slice(&buf[12..16]);
            out.extend_from_slice(&120u32.to_be_bytes());
            out.extend_from_slice(&[0; 8]);
            out.extend_from_slice(&[10, 1, 2, 3, 0, 80]);
            tracker.send_to(&out, from).await.unwrap();
        });
        let client = reqwest::Client::new();
        let r = announce(&client, &format!("udp://127.0.0.1:{port}/announce"), &sample())
            .await
            .unwrap();
        assert_eq!(r.peers, ["10.1.2.3:80".parse::<SocketAddr>().unwrap()]);
        server.await.unwrap();
    }
}
