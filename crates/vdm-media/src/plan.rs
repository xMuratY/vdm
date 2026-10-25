//! What to fetch for one media download, whether it came from HLS or DASH.

use url::Url;

/// How a segment is encrypted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Encryption {
    None,
    /// AES-128-CBC with PKCS#7 padding over the whole segment (HLS `METHOD=AES-128`).
    Aes128 {
        key_url: Url,
        iv: [u8; 16],
    },
}

/// One piece of a track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    pub url: Url,
    /// Part of the resource as `(offset, length)`; `None` means the whole resource.
    pub range: Option<(u64, u64)>,
    pub encryption: Encryption,
    /// Playback length in milliseconds; 0 when unknown.
    pub duration_ms: u64,
}

impl Segment {
    /// A plain, whole-resource segment.
    pub fn new(url: Url) -> Self {
        Self { url, range: None, encryption: Encryption::None, duration_ms: 0 }
    }
}

/// The file format the joined segments make.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// MPEG transport stream (`.ts`): classic HLS.
    Ts,
    /// Fragmented MP4 (`.mp4`, or `.m4a` for audio): HLS with `EXT-X-MAP`, most DASH.
    Mp4,
    /// WebM (`.webm`): some DASH.
    WebM,
    /// Raw elementary audio (`.aac`, `.mp3`, ...), named by its extension.
    Raw(&'static str),
}

impl Container {
    /// File extension for this container holding `kind`.
    pub fn extension(self, kind: TrackKind) -> &'static str {
        match (self, kind) {
            (Self::Ts, _) => "ts",
            (Self::Mp4, TrackKind::Audio) => "m4a",
            (Self::Mp4, _) => "mp4",
            (Self::WebM, _) => "webm",
            (Self::Raw(ext), _) => ext,
        }
    }
}

/// What a track carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    /// Video and audio together (most HLS).
    Muxed,
    Video,
    Audio,
}

/// Everything needed to rebuild one output file: the optional
/// initialization segment, then the media segments in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    pub kind: TrackKind,
    pub container: Container,
    pub init: Option<Segment>,
    pub segments: Vec<Segment>,
}

impl Track {
    /// Total playback length in milliseconds (0 when unknown).
    pub fn duration_ms(&self) -> u64 {
        self.segments.iter().map(|s| s.duration_ms).sum()
    }
}

/// One selectable quality of a stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    /// Bits per second (peak or average, as the stream reports it).
    pub bandwidth: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub codecs: Option<String>,
}

impl Variant {
    /// Short description such as "1080p, 5.0 Mbps" or "128 kbps".
    pub fn label(&self) -> String {
        let rate = if self.bandwidth >= 1_000_000 {
            format!("{:.1} Mbps", self.bandwidth as f64 / 1_000_000.0)
        } else {
            format!("{} kbps", self.bandwidth.div_ceil(1000))
        };
        match self.height {
            Some(h) => format!("{h}p, {rate}"),
            None => rate,
        }
    }
}

/// Which variant to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    /// Highest resolution, then highest bandwidth.
    #[default]
    Best,
    /// Lowest resolution, then lowest bandwidth.
    Worst,
    /// The best variant no taller than this many pixels; the smallest one if
    /// all are taller.
    MaxHeight(u32),
}

impl std::str::FromStr for Quality {
    type Err = String;

    /// "best", "worst", or a height like "720" / "720p".
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "best" => Ok(Self::Best),
            "worst" => Ok(Self::Worst),
            other => other
                .trim_end_matches('p')
                .parse()
                .map(Self::MaxHeight)
                .map_err(|_| format!("`{s}` is not best, worst or a height like 720")),
        }
    }
}

/// Index of the variant `quality` picks, or `None` for an empty list.
pub fn choose(variants: &[Variant], quality: Quality) -> Option<usize> {
    // Rank by height (unknown counts as 0), then bandwidth.
    let rank = |v: &Variant| (v.height.unwrap_or(0), v.bandwidth);
    let indexed = variants.iter().enumerate();
    match quality {
        Quality::Best => indexed.max_by_key(|(_, v)| rank(v)).map(|(i, _)| i),
        Quality::Worst => indexed.min_by_key(|(_, v)| rank(v)).map(|(i, _)| i),
        Quality::MaxHeight(max) => variants
            .iter()
            .enumerate()
            .filter(|(_, v)| v.height.is_some_and(|h| h <= max))
            .max_by_key(|(_, v)| rank(v))
            .map(|(i, _)| i)
            .or_else(|| choose(variants, Quality::Worst)),
    }
}

/// Whether a URL points at an HLS playlist or DASH manifest, judging by the
/// path's extension (`.m3u8`, `.m3u`, `.mpd`).
pub fn is_media_url(url: &str) -> bool {
    let Ok(url) = Url::parse(url.trim()) else { return false };
    let path = url.path().to_ascii_lowercase();
    [".m3u8", ".m3u", ".mpd"].iter().any(|ext| path.ends_with(ext))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(height: Option<u32>, bandwidth: u64) -> Variant {
        Variant { bandwidth, width: None, height, codecs: None }
    }

    #[test]
    fn choosing_quality() {
        let list = [v(Some(720), 3_000_000), v(Some(1080), 6_000_000), v(Some(360), 800_000)];
        assert_eq!(choose(&list, Quality::Best), Some(1));
        assert_eq!(choose(&list, Quality::Worst), Some(2));
        assert_eq!(choose(&list, Quality::MaxHeight(720)), Some(0));
        assert_eq!(choose(&list, Quality::MaxHeight(1000)), Some(0));
        assert_eq!(choose(&list, Quality::MaxHeight(240)), Some(2), "falls back to the smallest");
        assert_eq!(choose(&[], Quality::Best), None);
    }

    #[test]
    fn bandwidth_breaks_ties_and_unknown_heights_rank_lowest() {
        let list = [v(Some(720), 2_000_000), v(Some(720), 4_000_000), v(None, 9_000_000)];
        assert_eq!(choose(&list, Quality::Best), Some(1));
        assert_eq!(choose(&list, Quality::Worst), Some(2));
    }

    #[test]
    fn quality_from_text() {
        assert_eq!("best".parse(), Ok(Quality::Best));
        assert_eq!("Worst".parse(), Ok(Quality::Worst));
        assert_eq!("720p".parse(), Ok(Quality::MaxHeight(720)));
        assert_eq!("1080".parse(), Ok(Quality::MaxHeight(1080)));
        assert!("hd".parse::<Quality>().is_err());
    }

    #[test]
    fn labels_and_extensions() {
        assert_eq!(v(Some(1080), 5_000_000).label(), "1080p, 5.0 Mbps");
        assert_eq!(v(None, 128_000).label(), "128 kbps");
        assert_eq!(Container::Mp4.extension(TrackKind::Audio), "m4a");
        assert_eq!(Container::Mp4.extension(TrackKind::Muxed), "mp4");
        assert_eq!(Container::Ts.extension(TrackKind::Video), "ts");
        assert_eq!(Container::Raw("aac").extension(TrackKind::Audio), "aac");
    }

    #[test]
    fn media_urls() {
        assert!(is_media_url("https://cdn.example/live/master.m3u8?token=1"));
        assert!(is_media_url("https://cdn.example/v/manifest.MPD"));
        assert!(is_media_url("http://x/a.m3u"));
        assert!(!is_media_url("https://x/video.mp4"));
        assert!(!is_media_url("https://x/page?file=a.m3u8"));
        assert!(!is_media_url("not a url"));
    }
}
