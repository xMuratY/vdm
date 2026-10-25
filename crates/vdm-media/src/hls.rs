//! HLS (RFC 8216) playlists, parsed from text. Master playlists list variant
//! streams and audio renditions; media playlists list segments. No networking.

use std::collections::HashMap;

use url::Url;

use crate::{Container, Encryption, ParseError, Segment, Track, TrackKind, Variant};

/// A variant stream from a master playlist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HlsVariant {
    pub variant: Variant,
    pub uri: Url,
    /// `AUDIO` group this variant's audio lives in, if it is not muxed.
    pub audio_group: Option<String>,
}

/// An `EXT-X-MEDIA` rendition (only TYPE=AUDIO ones are kept).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendition {
    pub group_id: String,
    pub name: String,
    pub language: Option<String>,
    pub uri: Option<Url>,
    pub default: bool,
}

/// A master playlist: the variant streams and the audio renditions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterPlaylist {
    pub variants: Vec<HlsVariant>,
    pub audio: Vec<Rendition>,
}

/// A media playlist as one track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPlaylist {
    pub track: Track,
    /// `EXT-X-ENDLIST` present (false = live).
    pub ended: bool,
    pub target_duration_secs: u64,
}

/// Starts with `#EXTM3U` (ignoring a UTF-8 BOM and leading whitespace).
pub fn is_playlist(text: &str) -> bool {
    body(text).starts_with("#EXTM3U")
}

/// A playlist containing `#EXT-X-STREAM-INF`.
pub fn is_master(text: &str) -> bool {
    lines(text).any(|line| tag(line, "EXT-X-STREAM-INF").is_some())
}

/// Parse a master playlist; relative URIs resolve against `base` (the playlist's URL).
pub fn parse_master(text: &str, base: &Url) -> Result<MasterPlaylist, ParseError> {
    require_playlist(text)?;
    let mut variants = Vec::new();
    let mut audio = Vec::new();
    // A STREAM-INF line waiting for the URI on the next line.
    let mut pending: Option<(Variant, Option<String>)> = None;

    for line in lines(text) {
        if let Some(attrs) = tag(line, "EXT-X-STREAM-INF") {
            if pending.is_some() {
                return Err(ParseError::new("EXT-X-STREAM-INF is not followed by a URI"));
            }
            pending = Some(parse_stream_inf(attrs)?);
        } else if let Some(attrs) = tag(line, "EXT-X-MEDIA") {
            let attrs = Attributes::parse(attrs)?;
            if attrs.get("TYPE") == Some("AUDIO") {
                audio.push(parse_rendition(&attrs, base)?);
            }
        } else if tag(line, "EXTINF").is_some() {
            return Err(ParseError::new(
                "this is a media playlist, not a master playlist (use parse_media)",
            ));
        } else if line.starts_with('#') {
            // Other tags and comments, including EXT-X-I-FRAME-STREAM-INF.
        } else if let Some((variant, audio_group)) = pending.take() {
            variants.push(HlsVariant { variant, uri: resolve(base, line)?, audio_group });
        }
    }

    if pending.is_some() {
        return Err(ParseError::new("the last EXT-X-STREAM-INF has no URI"));
    }
    if variants.is_empty() {
        return Err(ParseError::new("master playlist has no variants"));
    }
    Ok(MasterPlaylist { variants, audio })
}

/// Parse a media playlist into one track of the given kind.
pub fn parse_media(text: &str, base: &Url, kind: TrackKind) -> Result<MediaPlaylist, ParseError> {
    require_playlist(text)?;
    if is_master(text) {
        return Err(ParseError::new("this is a master playlist (use parse_master)"));
    }

    let mut sequence: u64 = 0;
    let mut target_duration_secs: u64 = 0;
    let mut ended = false;
    let mut key: Option<Key> = None;
    let mut init: Option<Segment> = None;
    let mut segments: Vec<Segment> = Vec::new();
    // Pending per-segment tags, consumed by the next URI line.
    let mut extinf_ms: Option<u64> = None;
    let mut byte_range: Option<(u64, Option<u64>)> = None;
    // End offset of the last byte range seen for each resource.
    let mut resource_ends: HashMap<Url, u64> = HashMap::new();

    for line in lines(text) {
        if let Some(value) = tag(line, "EXTINF") {
            extinf_ms = Some(parse_extinf(value)?);
        } else if let Some(value) = tag(line, "EXT-X-BYTERANGE") {
            byte_range = Some(parse_byterange(value)?);
        } else if let Some(value) = tag(line, "EXT-X-KEY") {
            key = parse_key(value, base)?;
        } else if let Some(value) = tag(line, "EXT-X-MAP") {
            let map = parse_map(value, base, sequence, key.as_ref(), &mut resource_ends)?;
            if init.is_none() {
                init = Some(map);
            } else if init
                .as_ref()
                .is_some_and(|first| first.url != map.url || first.range != map.range)
            {
                return Err(ParseError::new("changing initialization sections is not supported"));
            }
        } else if let Some(value) = tag(line, "EXT-X-MEDIA-SEQUENCE") {
            sequence = parse_u64(value, "EXT-X-MEDIA-SEQUENCE")?;
        } else if let Some(value) = tag(line, "EXT-X-TARGETDURATION") {
            target_duration_secs = parse_u64(value, "EXT-X-TARGETDURATION")?;
        } else if tag(line, "EXT-X-ENDLIST").is_some() {
            ended = true;
        } else if line.starts_with('#') {
            // EXT-X-DISCONTINUITY, EXT-X-VERSION, comments and unknown tags.
        } else {
            let url = resolve(base, line)?;
            let range = resolve_range(byte_range.take(), &url, &mut resource_ends)?;
            segments.push(Segment {
                encryption: encryption_for(key.as_ref(), sequence),
                duration_ms: extinf_ms.take().unwrap_or(0),
                range,
                url,
            });
            sequence = sequence.saturating_add(1);
        }
    }

    if segments.is_empty() {
        return Err(ParseError::new("media playlist has no segments"));
    }
    let container = if init.is_some() { Container::Mp4 } else { container_for(&segments[0].url) };
    Ok(MediaPlaylist {
        track: Track { kind, container, init, segments },
        ended,
        target_duration_secs,
    })
}

/// The audio rendition to pair with a variant: in its group, DEFAULT=YES first,
/// else the first with a URI. None when the group has no URI renditions (audio is muxed).
pub fn audio_for<'a>(master: &'a MasterPlaylist, variant: &HlsVariant) -> Option<&'a Rendition> {
    let group = variant.audio_group.as_deref()?;
    let candidates: Vec<&Rendition> =
        master.audio.iter().filter(|r| r.group_id == group && r.uri.is_some()).collect();
    candidates.iter().copied().find(|r| r.default).or_else(|| candidates.first().copied())
}

// ---------------------------------------------------------------------------
// Line handling

/// The playlist text without a leading BOM or whitespace.
fn body(text: &str) -> &str {
    text.trim_start_matches(|c: char| c == '\u{feff}' || c.is_whitespace())
}

/// Trimmed, non-empty lines (handles CRLF and a leading BOM).
fn lines(text: &str) -> impl Iterator<Item = &str> {
    body(text).lines().map(str::trim).filter(|line| !line.is_empty())
}

/// The value of tag `#NAME:value` (or `""` for a bare `#NAME`), if `line` is that tag.
fn tag<'a>(line: &'a str, name: &str) -> Option<&'a str> {
    let rest = line.strip_prefix('#')?.strip_prefix(name)?;
    if rest.is_empty() { Some("") } else { rest.strip_prefix(':') }
}

fn require_playlist(text: &str) -> Result<(), ParseError> {
    if is_playlist(text) {
        Ok(())
    } else {
        Err(ParseError::new("not an HLS playlist: missing #EXTM3U"))
    }
}

/// Resolve a playlist URI against the playlist's own URL.
fn resolve(base: &Url, uri: &str) -> Result<Url, ParseError> {
    if uri.is_empty() {
        return Err(ParseError::new("empty URI in playlist"));
    }
    base.join(uri).map_err(|err| ParseError::new(format!("cannot resolve URI `{uri}`: {err}")))
}

// ---------------------------------------------------------------------------
// Attribute lists

/// An attribute list such as `BANDWIDTH=1,CODECS="a,b"`, as ordered pairs.
/// Quoted values are returned without their quotes.
struct Attributes<'a>(Vec<(&'a str, &'a str)>);

impl<'a> Attributes<'a> {
    fn parse(list: &'a str) -> Result<Self, ParseError> {
        let mut pairs = Vec::new();
        let mut rest = list.trim_start();
        while !rest.is_empty() {
            let eq = rest
                .find('=')
                .ok_or_else(|| ParseError::new(format!("attribute `{rest}` has no value")))?;
            let key = rest[..eq].trim();
            if key.is_empty() {
                return Err(ParseError::new(format!("attribute with no name in `{list}`")));
            }
            rest = &rest[eq + 1..];
            let value = if let Some(quoted) = rest.strip_prefix('"') {
                let end = quoted
                    .find('"')
                    .ok_or_else(|| ParseError::new(format!("unterminated quote in `{list}`")))?;
                rest = quoted[end + 1..].trim_start();
                &quoted[..end]
            } else {
                let end = rest.find(',').unwrap_or(rest.len());
                let value = rest[..end].trim();
                rest = &rest[end..];
                value
            };
            pairs.push((key, value));
            if let Some(next) = rest.strip_prefix(',') {
                rest = next.trim_start();
            } else if !rest.is_empty() {
                return Err(ParseError::new(format!("unexpected `{rest}` in `{list}`")));
            }
        }
        Ok(Self(pairs))
    }

    fn get(&self, key: &str) -> Option<&'a str> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    fn required(&self, key: &str, tag: &str) -> Result<&'a str, ParseError> {
        self.get(key).ok_or_else(|| ParseError::new(format!("{tag} has no {key} attribute")))
    }
}

// ---------------------------------------------------------------------------
// Master playlist tags

fn parse_stream_inf(list: &str) -> Result<(Variant, Option<String>), ParseError> {
    let attrs = Attributes::parse(list)?;
    let bandwidth = parse_u64(attrs.required("BANDWIDTH", "EXT-X-STREAM-INF")?, "BANDWIDTH")?;
    let (width, height) = attrs.get("RESOLUTION").and_then(parse_resolution).unzip();
    let variant =
        Variant { bandwidth, width, height, codecs: attrs.get("CODECS").map(str::to_owned) };
    Ok((variant, attrs.get("AUDIO").map(str::to_owned)))
}

/// `WxH`. A malformed value is treated as absent rather than failing the playlist.
fn parse_resolution(value: &str) -> Option<(u32, u32)> {
    let (width, height) = value.split_once(['x', 'X'])?;
    Some((width.trim().parse().ok()?, height.trim().parse().ok()?))
}

fn parse_rendition(attrs: &Attributes<'_>, base: &Url) -> Result<Rendition, ParseError> {
    Ok(Rendition {
        group_id: attrs.required("GROUP-ID", "EXT-X-MEDIA")?.to_owned(),
        name: attrs.required("NAME", "EXT-X-MEDIA")?.to_owned(),
        language: attrs.get("LANGUAGE").map(str::to_owned),
        uri: attrs.get("URI").map(|uri| resolve(base, uri)).transpose()?,
        default: attrs.get("DEFAULT") == Some("YES"),
    })
}

// ---------------------------------------------------------------------------
// Media playlist tags

/// An encryption key in effect: the AES-128 key URL and an explicit IV, if any.
#[derive(Debug, Clone)]
struct Key {
    url: Url,
    iv: Option<[u8; 16]>,
}

/// `EXT-X-KEY`. `None` means `METHOD=NONE`: segments are clear again.
fn parse_key(value: &str, base: &Url) -> Result<Option<Key>, ParseError> {
    let attrs = Attributes::parse(value)?;
    match attrs.required("METHOD", "EXT-X-KEY")? {
        "NONE" => Ok(None),
        "AES-128" => {
            if let Some(format) = attrs.get("KEYFORMAT").filter(|f| *f != "identity") {
                return Err(ParseError::new(format!("key format `{format}` is not supported")));
            }
            let uri = attrs.required("URI", "EXT-X-KEY")?;
            let iv = attrs.get("IV").map(parse_iv).transpose()?;
            Ok(Some(Key { url: resolve(base, uri)?, iv }))
        }
        other => Err(ParseError::new(format!("{other} encryption is not supported"))),
    }
}

/// The encryption of a segment with media sequence number `sequence`. Without an
/// explicit IV, the sequence number is the IV (RFC 8216 section 5.2).
fn encryption_for(key: Option<&Key>, sequence: u64) -> Encryption {
    match key {
        None => Encryption::None,
        Some(key) => Encryption::Aes128 {
            key_url: key.url.clone(),
            iv: key.iv.unwrap_or_else(|| u128::from(sequence).to_be_bytes()),
        },
    }
}

/// `EXT-X-MAP`: the initialization section.
fn parse_map(
    value: &str,
    base: &Url,
    sequence: u64,
    key: Option<&Key>,
    resource_ends: &mut HashMap<Url, u64>,
) -> Result<Segment, ParseError> {
    let attrs = Attributes::parse(value)?;
    let url = resolve(base, attrs.required("URI", "EXT-X-MAP")?)?;
    let pending = attrs.get("BYTERANGE").map(parse_byterange).transpose()?;
    let range = resolve_range(pending, &url, resource_ends)?;
    Ok(Segment { encryption: encryption_for(key, sequence), duration_ms: 0, range, url })
}

/// `EXT-X-BYTERANGE` value `n[@o]`: length and optional offset.
fn parse_byterange(value: &str) -> Result<(u64, Option<u64>), ParseError> {
    let (length, offset) = match value.split_once('@') {
        Some((length, offset)) => (length, Some(offset)),
        None => (value, None),
    };
    let length = parse_u64(length, "byte range length")?;
    let offset = offset.map(|o| parse_u64(o, "byte range offset")).transpose()?;
    Ok((length, offset))
}

/// The `(offset, length)` range of a resource, filling in a missing offset from
/// the previous range of the same resource. Records the new end offset.
fn resolve_range(
    pending: Option<(u64, Option<u64>)>,
    url: &Url,
    resource_ends: &mut HashMap<Url, u64>,
) -> Result<Option<(u64, u64)>, ParseError> {
    let Some((length, offset)) = pending else {
        return Ok(None);
    };
    let offset = match offset {
        Some(offset) => offset,
        None => *resource_ends.get(url).ok_or_else(|| {
            ParseError::new(format!("EXT-X-BYTERANGE has no offset and {url} has no earlier range"))
        })?,
    };
    let end = offset
        .checked_add(length)
        .ok_or_else(|| ParseError::new(format!("byte range overflows in {url}")))?;
    resource_ends.insert(url.clone(), end);
    Ok(Some((offset, length)))
}

/// `EXTINF` value `duration[,title]`, in milliseconds (rounded).
fn parse_extinf(value: &str) -> Result<u64, ParseError> {
    let seconds = value.split_once(',').map_or(value, |(duration, _)| duration).trim();
    let seconds: f64 = seconds
        .parse()
        .map_err(|_| ParseError::new(format!("invalid EXTINF duration `{seconds}`")))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(ParseError::new(format!("invalid EXTINF duration `{seconds}`")));
    }
    Ok((seconds * 1000.0).round() as u64)
}

/// A 16-byte IV: `0x` followed by up to 32 hex digits, left-padded with zeros.
fn parse_iv(value: &str) -> Result<[u8; 16], ParseError> {
    let invalid = || ParseError::new(format!("invalid IV `{value}`"));
    let hex = value.strip_prefix("0x").or_else(|| value.strip_prefix("0X")).ok_or_else(invalid)?;
    if hex.is_empty() || hex.len() > 32 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    let padded = format!("{hex:0>32}");
    let mut iv = [0u8; 16];
    for (i, byte) in iv.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&padded[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
    }
    Ok(iv)
}

fn parse_u64(value: &str, what: &str) -> Result<u64, ParseError> {
    value.trim().parse().map_err(|_| ParseError::new(format!("invalid {what}: `{value}`")))
}

/// The container implied by a segment's file extension (query string ignored).
fn container_for(url: &Url) -> Container {
    let name = url.path().rsplit('/').next().unwrap_or("");
    let ext = name.rsplit_once('.').map_or("", |(_, ext)| ext).to_ascii_lowercase();
    match ext.as_str() {
        "aac" => Container::Raw("aac"),
        "mp3" => Container::Raw("mp3"),
        "m4s" | "mp4" | "m4a" | "m4v" => Container::Mp4,
        "webm" => Container::WebM,
        _ => Container::Ts,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/hls/show/master.m3u8").unwrap()
    }

    fn media(text: &str) -> Result<MediaPlaylist, ParseError> {
        parse_media(text, &base(), TrackKind::Muxed)
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    #[test]
    fn attribute_parser_handles_quotes_hex_and_empty_values() {
        let attrs = Attributes::parse(
            r#"BANDWIDTH=1280000,CODECS="avc1.4d401f,mp4a.40.2",NAME="a, b",X=0x1A2B,EMPTY="""#,
        )
        .unwrap();
        assert_eq!(
            attrs.0,
            vec![
                ("BANDWIDTH", "1280000"),
                ("CODECS", "avc1.4d401f,mp4a.40.2"),
                ("NAME", "a, b"),
                ("X", "0x1A2B"),
                ("EMPTY", ""),
            ]
        );
        assert_eq!(attrs.get("CODECS"), Some("avc1.4d401f,mp4a.40.2"));
        assert_eq!(attrs.get("MISSING"), None);

        assert!(Attributes::parse("").unwrap().0.is_empty());
        assert!(Attributes::parse("NOVALUE").is_err());
        assert!(Attributes::parse(r#"A="open"#).is_err());
        assert!(Attributes::parse(r#"A="x"B=1"#).is_err());
        assert_eq!(Attributes::parse("A=1, B=2").unwrap().get("B"), Some("2"));
    }

    #[test]
    fn iv_parsing() {
        let mut expected = [0u8; 16];
        expected[15] = 0x2a;
        assert_eq!(parse_iv("0x2a").unwrap(), expected);
        assert_eq!(parse_iv("0X2A").unwrap(), expected);
        assert_eq!(parse_iv("0x0000000000000000000000000000002A").unwrap(), expected);
        assert!(parse_iv("2a").is_err());
        assert!(parse_iv("0x").is_err());
        assert!(parse_iv("0xZZ").is_err());
        assert!(parse_iv("0x1").is_ok());
        assert!(parse_iv(&format!("0x{}", "1".repeat(33))).is_err());
    }

    #[test]
    fn master_with_variants_audio_groups_and_iframes() {
        let text = "\u{feff}#EXTM3U\r\n\
            #EXT-X-VERSION:6\r\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,AUTOSELECT=YES,URI=\"audio/en.m3u8\"\r\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aac\",NAME=\"Deutsch\",LANGUAGE=\"de\",DEFAULT=NO,URI=\"audio/de.m3u8\"\r\n\
            #EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\",URI=\"subs/en.m3u8\"\r\n\
            #EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=86000,RESOLUTION=1920x1080,URI=\"iframe/1080.m3u8\"\r\n\
            #EXT-X-STREAM-INF:BANDWIDTH=800000,AVERAGE-BANDWIDTH=700000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\",AUDIO=\"aac\"\r\n\
            360/index.m3u8\r\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2800000,RESOLUTION=1280X720,CODECS=\"avc1.64001f,mp4a.40.2\",AUDIO=\"aac\"\r\n\
            \r\n\
            # a comment between tag and URI\r\n\
            720/index.m3u8\r\n\
            #EXT-X-STREAM-INF:BANDWIDTH=5000000,RESOLUTION=1920x1080,CODECS=\"avc1.640028\"\r\n\
            https://cdn.other.net/1080/index.m3u8\r\n";
        assert!(is_playlist(text));
        assert!(is_master(text));
        let master = parse_master(text, &base()).unwrap();

        assert_eq!(master.variants.len(), 3);
        let first = &master.variants[0];
        assert_eq!(first.variant.bandwidth, 800_000);
        assert_eq!((first.variant.width, first.variant.height), (Some(640), Some(360)));
        assert_eq!(first.variant.codecs.as_deref(), Some("avc1.42e01e,mp4a.40.2"));
        assert_eq!(first.audio_group.as_deref(), Some("aac"));
        assert_eq!(first.uri, url("https://cdn.example.com/hls/show/360/index.m3u8"));

        let second = &master.variants[1];
        assert_eq!((second.variant.width, second.variant.height), (Some(1280), Some(720)));

        let third = &master.variants[2];
        assert_eq!(third.audio_group, None);
        assert_eq!(third.uri, url("https://cdn.other.net/1080/index.m3u8"));
        assert!(master.variants.iter().all(|v| !v.uri.as_str().contains("iframe")));

        assert_eq!(master.audio.len(), 2, "subtitles are not kept");
        let german = &master.audio[1];
        assert_eq!(german.group_id, "aac");
        assert_eq!(german.name, "Deutsch");
        assert_eq!(german.language.as_deref(), Some("de"));
        assert!(!german.default);
        assert_eq!(german.uri, Some(url("https://cdn.example.com/hls/show/audio/de.m3u8")));
    }

    #[test]
    fn audio_for_prefers_default_then_first_with_uri() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"x\",DEFAULT=YES,URI=\"x.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"y\",URI=\"y.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"b\",NAME=\"no-uri\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"b\",NAME=\"first\",URI=\"b1.m3u8\"\n\
            #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"b\",NAME=\"second\",URI=\"b2.m3u8\"\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1000,AUDIO=\"a\"\n\
            v.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2000,AUDIO=\"b\"\n\
            w.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=3000,AUDIO=\"c\"\n\
            z.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4000\n\
            muxed.m3u8\n";
        let master = parse_master(text, &base()).unwrap();
        let [a, b, c, muxed] = master.variants.as_slice() else { panic!("four variants") };

        assert_eq!(audio_for(&master, a).map(|r| r.name.as_str()), Some("x"));
        assert_eq!(audio_for(&master, b).map(|r| r.name.as_str()), Some("first"));
        assert_eq!(audio_for(&master, c), None, "group has no renditions");
        assert_eq!(audio_for(&master, muxed), None, "no AUDIO attribute: muxed");
    }

    #[test]
    fn master_rejects_missing_bandwidth_missing_uri_and_no_variants() {
        let no_bandwidth = "#EXTM3U\n#EXT-X-STREAM-INF:RESOLUTION=1280x720\nhi.m3u8\n";
        let err = parse_master(no_bandwidth, &base()).unwrap_err();
        assert!(err.0.contains("BANDWIDTH"), "{err}");

        let no_uri = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\n";
        assert!(parse_master(no_uri, &base()).is_err());

        let no_variants = "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"n\"\n";
        assert!(parse_master(no_variants, &base()).is_err());

        let bad_bandwidth = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=fast\nhi.m3u8\n";
        assert!(parse_master(bad_bandwidth, &base()).is_err());
    }

    #[test]
    fn relative_absolute_and_parent_uris_resolve_against_the_playlist() {
        let base = Url::parse("https://cdn.example.com/live/a/b/master.m3u8").unwrap();
        let text = "#EXTM3U\n\
            #EXT-X-STREAM-INF:BANDWIDTH=1\n\
            ../x/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=2\n\
            /root/index.m3u8\n\
            #EXT-X-STREAM-INF:BANDWIDTH=3\n\
            https://other.example.org/abs.m3u8?token=1\n\
            #EXT-X-STREAM-INF:BANDWIDTH=4\n\
            sub/dir/index.m3u8\n";
        let master = parse_master(text, &base).unwrap();
        let uris: Vec<&str> = master.variants.iter().map(|v| v.uri.as_str()).collect();
        assert_eq!(
            uris,
            [
                "https://cdn.example.com/live/a/x/index.m3u8",
                "https://cdn.example.com/root/index.m3u8",
                "https://other.example.org/abs.m3u8?token=1",
                "https://cdn.example.com/live/a/b/sub/dir/index.m3u8",
            ]
        );
    }

    #[test]
    fn vod_media_playlist() {
        let text = "#EXTM3U\n\
            #EXT-X-VERSION:3\n\
            #EXT-X-PLAYLIST-TYPE:VOD\n\
            #EXT-X-TARGETDURATION:10\n\
            #EXT-X-MEDIA-SEQUENCE:0\n\
            #EXT-X-PROGRAM-DATE-TIME:2024-01-01T00:00:00Z\n\
            #EXTINF:9.009,\n\
            seg0.ts\n\
            #EXT-X-DISCONTINUITY\n\
            #EXTINF:10.0,Title here\n\
            seg1.ts?token=abc\n\
            #EXTINF:4.5,\n\
            seg2.ts\n\
            #EXT-X-ENDLIST\n";
        let playlist = media(text).unwrap();
        assert!(playlist.ended);
        assert_eq!(playlist.target_duration_secs, 10);
        let track = &playlist.track;
        assert_eq!(track.kind, TrackKind::Muxed);
        assert_eq!(track.container, Container::Ts);
        assert!(track.init.is_none());
        assert_eq!(track.segments.len(), 3);
        let durations: Vec<u64> = track.segments.iter().map(|s| s.duration_ms).collect();
        assert_eq!(durations, [9009, 10_000, 4500]);
        assert_eq!(track.duration_ms(), 23_509);
        assert_eq!(
            track.segments[1].url,
            url("https://cdn.example.com/hls/show/seg1.ts?token=abc")
        );
        assert!(track.segments.iter().all(|s| s.range.is_none()));
        assert!(track.segments.iter().all(|s| s.encryption == Encryption::None));
    }

    #[test]
    fn live_playlist_without_endlist_is_not_ended() {
        let text = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:6\n\
            #EXT-X-MEDIA-SEQUENCE:120\n\
            #EXTINF:6.000,\n\
            chunk120.ts\n\
            #EXTINF:6.000,\n\
            chunk121.ts\n";
        let playlist = media(text).unwrap();
        assert!(!playlist.ended);
        assert_eq!(playlist.target_duration_secs, 6);
        assert_eq!(playlist.track.segments.len(), 2);
    }

    #[test]
    fn byte_ranges_with_and_without_explicit_offsets() {
        let text = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:10\n\
            #EXTINF:10.0,\n\
            #EXT-X-BYTERANGE:1000@0\n\
            media.ts\n\
            #EXTINF:10.0,\n\
            #EXT-X-BYTERANGE:1000\n\
            media.ts\n\
            #EXTINF:10.0,\n\
            #EXT-X-BYTERANGE:500@5000\n\
            media.ts\n\
            #EXTINF:10.0,\n\
            #EXT-X-BYTERANGE:200\n\
            media.ts\n\
            #EXTINF:10.0,\n\
            other.ts\n\
            #EXT-X-ENDLIST\n";
        let track = media(text).unwrap().track;
        let ranges: Vec<Option<(u64, u64)>> = track.segments.iter().map(|s| s.range).collect();
        assert_eq!(
            ranges,
            [Some((0, 1000)), Some((1000, 1000)), Some((5000, 500)), Some((5500, 200)), None]
        );
    }

    #[test]
    fn byte_range_without_offset_needs_an_earlier_range_of_the_same_resource() {
        let first = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\n#EXT-X-BYTERANGE:1000\nmedia.ts\n#EXT-X-ENDLIST\n";
        let err = media(first).unwrap_err();
        assert!(err.0.contains("no earlier range"), "{err}");

        let other_resource = "#EXTM3U\n#EXTINF:10,\n#EXT-X-BYTERANGE:1000@0\na.ts\n#EXTINF:10,\n#EXT-X-BYTERANGE:1000\nb.ts\n";
        assert!(media(other_resource).is_err());
    }

    #[test]
    fn aes128_with_explicit_and_implicit_iv_rotation_and_none() {
        let text = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:10\n\
            #EXT-X-MEDIA-SEQUENCE:7\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"key.bin\"\n\
            #EXTINF:10.0,\n\
            seg7.ts\n\
            #EXTINF:10.0,\n\
            seg8.ts\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"https://keys.example.com/k2\",IV=0x0000000000000000000000000000ABCD\n\
            #EXTINF:10.0,\n\
            seg9.ts\n\
            #EXT-X-KEY:METHOD=NONE\n\
            #EXTINF:10.0,\n\
            seg10.ts\n\
            #EXT-X-ENDLIST\n";
        let track = media(text).unwrap().track;
        assert_eq!(track.segments.len(), 4);

        let key_url = url("https://cdn.example.com/hls/show/key.bin");
        let mut iv7 = [0u8; 16];
        iv7[15] = 7;
        let mut iv8 = [0u8; 16];
        iv8[15] = 8;
        assert_eq!(
            track.segments[0].encryption,
            Encryption::Aes128 { key_url: key_url.clone(), iv: iv7 }
        );
        assert_eq!(track.segments[1].encryption, Encryption::Aes128 { key_url, iv: iv8 });

        let mut explicit = [0u8; 16];
        explicit[14] = 0xAB;
        explicit[15] = 0xCD;
        assert_eq!(
            track.segments[2].encryption,
            Encryption::Aes128 { key_url: url("https://keys.example.com/k2"), iv: explicit }
        );
        assert_eq!(track.segments[3].encryption, Encryption::None);
    }

    #[test]
    fn implicit_iv_is_the_big_endian_sequence_number_for_later_segments() {
        // Sequence 258 for the first segment, so the second one is 259 = 0x0103.
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA-SEQUENCE:258\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n\
            #EXTINF:4,\n\
            a.ts\n\
            #EXTINF:4,\n\
            b.ts\n";
        let track = media(text).unwrap().track;
        let Encryption::Aes128 { iv, .. } = &track.segments[1].encryption else {
            panic!("AES-128 expected")
        };
        assert_eq!(iv[..14], [0u8; 14]);
        assert_eq!(iv[14..], [0x01, 0x03]);
    }

    #[test]
    fn aes128_errors() {
        let no_uri = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128\n#EXTINF:4,\na.ts\n";
        assert!(media(no_uri).is_err());

        let bad_iv = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",IV=0xGG\n#EXTINF:4,\na.ts\n";
        assert!(media(bad_iv).is_err());

        let key_format = "#EXTM3U\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\",KEYFORMAT=\"com.apple.streamingkeydelivery\"\n#EXTINF:4,\na.ts\n";
        let err = media(key_format).unwrap_err();
        assert!(err.0.contains("not supported"), "{err}");
    }

    #[test]
    fn sample_aes_is_rejected() {
        for method in ["SAMPLE-AES", "SAMPLE-AES-CTR"] {
            let text = format!("#EXTM3U\n#EXT-X-KEY:METHOD={method},URI=\"k\"\n#EXTINF:4,\na.ts\n");
            let err = media(&text).unwrap_err();
            assert_eq!(err.0, format!("{method} encryption is not supported"));
        }
    }

    #[test]
    fn map_with_byterange_makes_an_mp4_track_with_init() {
        let text = "#EXTM3U\n\
            #EXT-X-TARGETDURATION:6\n\
            #EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"720@0\"\n\
            #EXTINF:6.0,\n\
            seg1.m4s\n\
            #EXTINF:6.0,\n\
            seg2.m4s\n\
            #EXT-X-ENDLIST\n";
        let track = media(text).unwrap().track;
        assert_eq!(track.container, Container::Mp4);
        let init = track.init.expect("init section");
        assert_eq!(init.url, url("https://cdn.example.com/hls/show/init.mp4"));
        assert_eq!(init.range, Some((0, 720)));
        assert_eq!(init.duration_ms, 0);
        assert_eq!(init.encryption, Encryption::None);
        assert_eq!(track.segments.len(), 2);
    }

    #[test]
    fn map_takes_the_key_in_effect_and_repeating_the_same_map_is_fine() {
        let text = "#EXTM3U\n\
            #EXT-X-MEDIA-SEQUENCE:3\n\
            #EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n\
            #EXTINF:6,\n\
            s3.m4s\n\
            #EXT-X-MAP:URI=\"init.mp4\"\n\
            #EXTINF:6,\n\
            s4.m4s\n";
        let track = media(text).unwrap().track;
        let init = track.init.expect("init section");
        assert_eq!(
            init.encryption,
            Encryption::Aes128 {
                key_url: url("https://cdn.example.com/hls/show/k"),
                iv: u128::from(3u64).to_be_bytes(),
            }
        );
    }

    #[test]
    fn changing_the_map_is_an_error() {
        let text = "#EXTM3U\n\
            #EXT-X-MAP:URI=\"init1.mp4\"\n\
            #EXTINF:6,\n\
            s1.m4s\n\
            #EXT-X-MAP:URI=\"init2.mp4\"\n\
            #EXTINF:6,\n\
            s2.m4s\n";
        let err = media(text).unwrap_err();
        assert_eq!(err.0, "changing initialization sections is not supported");
    }

    #[test]
    fn audio_only_aac_and_other_extensions_pick_containers() {
        let aac = "#EXTM3U\n#EXT-X-TARGETDURATION:10\n#EXTINF:10,\naudio/chunk_1.aac\n#EXTINF:10,\naudio/chunk_2.aac\n#EXT-X-ENDLIST\n";
        let playlist = parse_media(aac, &base(), TrackKind::Audio).unwrap();
        assert_eq!(playlist.track.kind, TrackKind::Audio);
        assert_eq!(playlist.track.container, Container::Raw("aac"));
        assert!(playlist.ended);

        let cases = [
            ("seg.mp3", Container::Raw("mp3")),
            ("seg.m4s", Container::Mp4),
            ("seg.M4A", Container::Mp4),
            ("seg.webm", Container::WebM),
            ("seg", Container::Ts),
            ("seg.bin", Container::Ts),
        ];
        for (name, expected) in cases {
            let text = format!("#EXTM3U\n#EXTINF:4,\n{name}\n");
            let track = parse_media(&text, &base(), TrackKind::Muxed).unwrap().track;
            assert_eq!(track.container, expected, "{name}");
        }
    }

    #[test]
    fn crlf_and_bom_are_handled() {
        let text = "\u{feff}#EXTM3U\r\n#EXT-X-TARGETDURATION:10\r\n#EXTINF:9.5,\r\na.ts\r\n#EXT-X-ENDLIST\r\n";
        assert!(is_playlist(text));
        let playlist = media(text).unwrap();
        assert!(playlist.ended);
        assert_eq!(playlist.track.segments[0].duration_ms, 9500);
        assert_eq!(playlist.track.segments[0].url, url("https://cdn.example.com/hls/show/a.ts"));
    }

    #[test]
    fn playlist_and_master_detection() {
        assert!(is_playlist("  \n#EXTM3U\n"));
        assert!(!is_playlist("<html>#EXTM3U</html>"));
        assert!(!is_master("#EXTM3U\n#EXT-X-I-FRAME-STREAM-INF:BANDWIDTH=1,URI=\"x\"\n"));
    }

    #[test]
    fn master_given_to_parse_media_and_media_given_to_parse_master_are_errors() {
        let master = "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nv.m3u8\n";
        assert!(media(master).is_err());

        let media_text = "#EXTM3U\n#EXTINF:4,\na.ts\n";
        assert!(parse_master(media_text, &base()).is_err());
    }

    #[test]
    fn media_errors() {
        assert!(media("#EXTM3U\n#EXT-X-TARGETDURATION:10\n").is_err(), "no segments");
        assert!(media("#EXTM3U\n#EXTINF:-1,\na.ts\n").is_err(), "negative duration");
        assert!(media("a.ts\n#EXTINF:4,\n").is_err(), "not a playlist");
    }

    #[test]
    fn unknown_tags_are_ignored() {
        let text = "#EXTM3U\n#EXT-X-FOO:bar=1\n#EXT-X-SKIP:SKIPPED-SEGMENTS=2\n#EXTINF:4,\na.ts\n";
        let track = media(text).unwrap().track;
        assert_eq!(track.segments.len(), 1);
    }
}
