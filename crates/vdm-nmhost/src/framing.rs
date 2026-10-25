//! Native messaging framing: every message is a `u32` byte length in native
//! byte order, followed by that many bytes of UTF-8 JSON.

use std::fmt;
use std::io::{self, Read, Write};

use serde_json::Value;

/// Largest message the browser may send us, in bytes.
pub const MAX_INCOMING: u32 = 64 * 1024 * 1024;
/// Largest message we may send to the browser, in bytes.
pub const MAX_OUTGOING: usize = 1024 * 1024;

/// The error for a length prefix above [`MAX_INCOMING`]. The body is not read,
/// so the stream cannot be resynchronised after it.
#[derive(Debug)]
pub struct FrameTooLarge {
    pub len: u32,
}

impl fmt::Display for FrameTooLarge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "incoming message of {} bytes is over the limit", self.len)
    }
}

impl std::error::Error for FrameTooLarge {}

/// True when `err` came from a length prefix above [`MAX_INCOMING`].
pub fn is_frame_too_large(err: &io::Error) -> bool {
    err.get_ref().is_some_and(|inner| inner.is::<FrameTooLarge>())
}

/// Read one message. `Ok(None)` on a clean EOF before a length prefix.
/// EOF in the middle of a message is an `UnexpectedEof` error. A body that is
/// not JSON is an `InvalidData` error, after the whole body has been consumed.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Value>> {
    let mut prefix = [0u8; 4];
    loop {
        match r.read(&mut prefix[..1]) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    r.read_exact(&mut prefix[1..])?;

    let len = u32::from_ne_bytes(prefix);
    if len > MAX_INCOMING {
        return Err(io::Error::new(io::ErrorKind::InvalidData, FrameTooLarge { len }));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body)?;
    serde_json::from_slice(&body).map(Some).map_err(|err| {
        io::Error::new(io::ErrorKind::InvalidData, format!("message is not JSON: {err}"))
    })
}

/// Write one message and flush. Nothing is written if the reply is larger
/// than [`MAX_OUTGOING`]; that is an `InvalidData` error.
pub fn write_message(w: &mut impl Write, msg: &Value) -> io::Result<()> {
    let body = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if body.len() > MAX_OUTGOING {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("reply of {} bytes is over the {MAX_OUTGOING}-byte limit", body.len()),
        ));
    }
    // At most MAX_OUTGOING, so this cannot fail.
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    w.write_all(&len.to_ne_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use serde_json::json;

    use super::*;

    fn frame(body: &[u8]) -> Vec<u8> {
        let mut bytes = (body.len() as u32).to_ne_bytes().to_vec();
        bytes.extend_from_slice(body);
        bytes
    }

    #[test]
    fn round_trip() {
        let msg = json!({"type": "ping", "n": [1, 2, 3], "text": "héllo"});
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        assert_eq!(&buf[..4], &(buf.len() as u32 - 4).to_ne_bytes());
        let got = read_message(&mut Cursor::new(buf)).unwrap();
        assert_eq!(got, Some(msg));
    }

    #[test]
    fn clean_eof_is_none() {
        assert_eq!(read_message(&mut Cursor::new(Vec::new())).unwrap(), None);
    }

    #[test]
    fn truncated_length_is_unexpected_eof() {
        let err = read_message(&mut Cursor::new(vec![1u8, 0])).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn truncated_body_is_unexpected_eof() {
        let mut bytes = 10u32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"{}");
        let err = read_message(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn oversized_length_is_invalid_data() {
        let bytes = (MAX_INCOMING + 1).to_ne_bytes().to_vec();
        let err = read_message(&mut Cursor::new(bytes)).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(is_frame_too_large(&err));
    }

    #[test]
    fn non_json_body_is_invalid_data_and_stream_stays_in_sync() {
        let mut bytes = frame(b"{not json");
        bytes.extend(frame(br#"{"type":"ping"}"#));
        let mut cursor = Cursor::new(bytes);

        let err = read_message(&mut cursor).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(!is_frame_too_large(&err));

        let next = read_message(&mut cursor).unwrap();
        assert_eq!(next, Some(json!({"type": "ping"})));
        assert_eq!(read_message(&mut cursor).unwrap(), None);
    }

    #[test]
    fn outgoing_too_large_is_an_error_and_writes_nothing() {
        let big = "x".repeat(MAX_OUTGOING);
        let mut buf = Vec::new();
        let err = write_message(&mut buf, &json!({ "big": big })).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(buf.is_empty());
    }
}
