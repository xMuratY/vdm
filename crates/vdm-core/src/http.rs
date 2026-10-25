//! Small HTTP helpers shared by probing and downloading.

use std::time::Duration;

use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

use crate::error::{Error, Result};

/// A parsed `Content-Range` response header (RFC 9110 §14.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ContentRange {
    /// `bytes start-end/total`; `total` is `None` for `/*`.
    Bytes { start: u64, end: u64, total: Option<u64> },
    /// `bytes */total`, sent with `416`.
    Unsatisfied { total: u64 },
}

pub(crate) fn parse_content_range(value: &str) -> Option<ContentRange> {
    let rest = value.trim().strip_prefix("bytes")?.trim_start();
    let (range, total) = rest.split_once('/')?;
    let total = total.trim();
    if range.trim() == "*" {
        return Some(ContentRange::Unsatisfied { total: total.parse().ok()? });
    }
    let (start, end) = range.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    let total = if total == "*" { None } else { Some(total.parse::<u64>().ok()?) };
    if end < start || total.is_some_and(|t| end >= t) {
        return None;
    }
    Some(ContentRange::Bytes { start, end, total })
}

pub(crate) fn content_range(headers: &HeaderMap) -> Option<ContentRange> {
    parse_content_range(header_str(headers, header::CONTENT_RANGE)?)
}

pub(crate) fn header_str(headers: &HeaderMap, name: HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim).filter(|v| !v.is_empty())
}

pub(crate) fn header_string(headers: &HeaderMap, name: HeaderName) -> Option<String> {
    header_str(headers, name).map(str::to_owned)
}

pub(crate) fn content_length(headers: &HeaderMap) -> Option<u64> {
    header_str(headers, header::CONTENT_LENGTH)?.parse().ok()
}

/// `Retry-After` in seconds. HTTP-date values are ignored.
pub(crate) fn retry_after(headers: &HeaderMap) -> Option<Duration> {
    header_str(headers, header::RETRY_AFTER)?.parse().ok().map(Duration::from_secs)
}

/// Strong ETags can be used with `If-Range`; weak ones (`W/"..."`) cannot.
pub(crate) fn is_strong_etag(etag: &str) -> bool {
    !etag.starts_with("W/")
}

/// Turn user-supplied `(name, value)` pairs into a header map.
pub(crate) fn build_headers(pairs: &[(String, String)]) -> Result<HeaderMap> {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        let n = HeaderName::from_bytes(name.trim().as_bytes())
            .map_err(|_| Error::InvalidHeader(name.clone()))?;
        let v =
            HeaderValue::from_str(value.trim()).map_err(|_| Error::InvalidHeader(name.clone()))?;
        map.append(n, v);
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ContentRange::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(
            parse_content_range("bytes 0-0/1000"),
            Some(Bytes { start: 0, end: 0, total: Some(1000) })
        );
        assert_eq!(
            parse_content_range("bytes 10-19/*"),
            Some(Bytes { start: 10, end: 19, total: None })
        );
        assert_eq!(parse_content_range("bytes */0"), Some(Unsatisfied { total: 0 }));
        assert_eq!(parse_content_range("bytes 5-1/10"), None);
        assert_eq!(parse_content_range("bytes 0-10/10"), None);
        assert_eq!(parse_content_range("items 0-1/2"), None);
        assert_eq!(parse_content_range("bytes 0-1"), None);
    }

    #[test]
    fn header_building() {
        let pairs = vec![("Referer".to_owned(), " https://a/ ".to_owned())];
        assert_eq!(build_headers(&pairs).unwrap()["referer"], "https://a/");
        assert!(build_headers(&[("bad name".to_owned(), "x".to_owned())]).is_err());
    }
}
