//! Bencode (BEP 3): the encoding of `.torrent` files, tracker replies and
//! extension messages.
//!
//! Decoding keeps the raw bytes of every dictionary value it is asked for,
//! because a torrent's identity (the info hash) is the SHA-1 of the `info`
//! dictionary exactly as it appears in the file.

use std::collections::BTreeMap;

/// Deepest nesting accepted, so hostile input can't exhaust the stack.
const MAX_DEPTH: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Value {
    Int(i64),
    Bytes(Vec<u8>),
    List(Vec<Value>),
    Dict(BTreeMap<Vec<u8>, Value>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DecodeError(pub String);

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "bad bencode: {}", self.0)
    }
}

fn err<T>(msg: impl Into<String>) -> Result<T, DecodeError> {
    Err(DecodeError(msg.into()))
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Bytes as UTF-8 text (lossy: torrents in the wild aren't always valid).
    pub fn as_str(&self) -> Option<String> {
        self.as_bytes().map(|b| String::from_utf8_lossy(b).into_owned())
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_dict(&self) -> Option<&BTreeMap<Vec<u8>, Value>> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// The value under `key` when this is a dictionary.
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_dict()?.get(key.as_bytes())
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_into(&mut out);
        out
    }

    fn encode_into(&self, out: &mut Vec<u8>) {
        match self {
            Value::Int(i) => {
                out.push(b'i');
                out.extend_from_slice(i.to_string().as_bytes());
                out.push(b'e');
            }
            Value::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            Value::List(items) => {
                out.push(b'l');
                for item in items {
                    item.encode_into(out);
                }
                out.push(b'e');
            }
            Value::Dict(map) => {
                // BTreeMap keeps keys in the sorted order bencode requires.
                out.push(b'd');
                for (k, v) in map {
                    Value::Bytes(k.clone()).encode_into(out);
                    v.encode_into(out);
                }
                out.push(b'e');
            }
        }
    }
}

/// Shorthand for building dictionaries in code.
pub(crate) fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dict(entries.into_iter().map(|(k, v)| (k.as_bytes().to_vec(), v)).collect())
}

pub(crate) fn bytes(b: impl Into<Vec<u8>>) -> Value {
    Value::Bytes(b.into())
}

/// Decode one value that must fill all of `input`.
pub(crate) fn decode(input: &[u8]) -> Result<Value, DecodeError> {
    let (value, used) = decode_prefix(input)?;
    if used != input.len() {
        return err("trailing data after the value");
    }
    Ok(value)
}

/// Decode one value at the start of `input`; returns it and how many bytes it took.
/// (Extension messages carry raw data right after a bencoded header.)
pub(crate) fn decode_prefix(input: &[u8]) -> Result<(Value, usize), DecodeError> {
    let mut parser = Parser { input, pos: 0, depth: 0, raw_key: None, raw: None };
    let value = parser.value()?;
    Ok((value, parser.pos))
}

/// The exact bytes of the value under top-level `key` in a bencoded dictionary,
/// e.g. the `info` dictionary of a `.torrent` file.
pub(crate) fn raw_value(input: &[u8], key: &str) -> Result<Option<Vec<u8>>, DecodeError> {
    let mut parser = Parser { input, pos: 0, depth: 0, raw_key: Some(key.as_bytes()), raw: None };
    parser.value()?;
    Ok(parser.raw)
}

struct Parser<'a> {
    input: &'a [u8],
    pos: usize,
    depth: usize,
    /// Record the raw span of this top-level key's value.
    raw_key: Option<&'a [u8]>,
    raw: Option<Vec<u8>>,
}

impl Parser<'_> {
    fn peek(&self) -> Result<u8, DecodeError> {
        self.input.get(self.pos).copied().ok_or_else(|| DecodeError("unexpected end".into()))
    }

    fn value(&mut self) -> Result<Value, DecodeError> {
        match self.peek()? {
            b'i' => self.int(),
            b'0'..=b'9' => self.bytes().map(Value::Bytes),
            b'l' => self.nested(|p| {
                let mut items = Vec::new();
                while p.peek()? != b'e' {
                    items.push(p.value()?);
                }
                Ok(Value::List(items))
            }),
            b'd' => self.nested(|p| {
                let top = p.depth == 1;
                let mut map = BTreeMap::new();
                while p.peek()? != b'e' {
                    let key = p.bytes()?;
                    let start = p.pos;
                    let value = p.value()?;
                    if top && p.raw_key == Some(key.as_slice()) {
                        p.raw = Some(p.input[start..p.pos].to_vec());
                    }
                    map.insert(key, value);
                }
                Ok(Value::Dict(map))
            }),
            other => err(format!("unexpected byte {other:#04x} at {}", self.pos)),
        }
    }

    fn nested(
        &mut self,
        body: impl FnOnce(&mut Self) -> Result<Value, DecodeError>,
    ) -> Result<Value, DecodeError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return err("nested too deeply");
        }
        self.pos += 1; // 'l' or 'd'
        let value = body(self)?;
        self.pos += 1; // 'e'
        self.depth -= 1;
        Ok(value)
    }

    fn int(&mut self) -> Result<Value, DecodeError> {
        self.pos += 1; // 'i'
        let end = self.find(b'e')?;
        let text = std::str::from_utf8(&self.input[self.pos..end])
            .map_err(|_| DecodeError("integer is not text".into()))?;
        let canonical =
            text == "0" || (!text.starts_with('0') && !text.starts_with("-0") && !text.is_empty());
        let value = text.parse::<i64>().ok().filter(|_| canonical);
        let Some(value) = value else { return err(format!("bad integer `{text}`")) };
        self.pos = end + 1;
        Ok(Value::Int(value))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, DecodeError> {
        let colon = self.find(b':')?;
        let len = std::str::from_utf8(&self.input[self.pos..colon])
            .ok()
            .and_then(|t| t.parse::<usize>().ok())
            .ok_or_else(|| DecodeError("bad string length".into()))?;
        let start = colon + 1;
        let end = start.checked_add(len).filter(|&e| e <= self.input.len());
        let Some(end) = end else { return err("string runs past the end") };
        self.pos = end;
        Ok(self.input[start..end].to_vec())
    }

    fn find(&self, byte: u8) -> Result<usize, DecodeError> {
        self.input[self.pos..]
            .iter()
            .position(|&b| b == byte)
            .map(|i| self.pos + i)
            .ok_or_else(|| DecodeError("unexpected end".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let v = dict([
            ("announce", bytes("http://t/a")),
            ("list", Value::List(vec![Value::Int(-3), bytes("x")])),
            ("n", Value::Int(0)),
        ]);
        let enc = v.encode();
        assert_eq!(enc, b"d8:announce10:http://t/a4:listli-3e1:xe1:ni0ee");
        assert_eq!(decode(&enc).unwrap(), v);
    }

    #[test]
    fn rejects_malformed_input() {
        for bad in [
            &b"i01e"[..],
            b"i-0e",
            b"ie",
            b"5:abc",
            b"l",
            b"d1:a",
            b"x",
            b"i1ei2e",
            b"99999999999999999999999:x",
        ] {
            assert!(decode(bad).is_err(), "{:?}", String::from_utf8_lossy(bad));
        }
        let deep = format!("{}{}", "l".repeat(200), "e".repeat(200));
        assert!(decode(deep.as_bytes()).is_err());
    }

    #[test]
    fn raw_value_is_byte_exact() {
        // Keys out of order and an int written differently would change the hash
        // if the info dict were re-encoded instead of taken as-is.
        let input = b"d4:infod1:bi1e1:ai2ee3:zzzi9ee";
        assert_eq!(raw_value(input, "info").unwrap().unwrap(), b"d1:bi1e1:ai2ee");
        assert_eq!(raw_value(input, "nope").unwrap(), None);
        // Nested keys with the same name don't count.
        assert_eq!(raw_value(b"d1:xd4:infoi1eee", "info").unwrap(), None);
    }

    #[test]
    fn prefix_decoding_reports_the_length() {
        let (v, used) = decode_prefix(b"d1:ai1eeRAWDATA").unwrap();
        assert_eq!(v.get("a").and_then(Value::as_int), Some(1));
        assert_eq!(used, 8);
    }
}
