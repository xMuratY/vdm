//! The details panel's live data: each download's piece map, recent speeds
//! and activity log, built from engine events as they arrive.
//!
//! Plain Rust (no iced), so it is unit-tested. The drawings (piece map and
//! speed graph) are SVG documents built here from plain colors.

use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use chrono::{DateTime, Local};
use vdm_proto::{DownloadId, DownloadState, Event, QueueId, SegmentProgress};

use crate::i18n::t;
use crate::model::fmt_bytes;

/// Seconds of speed history the graph shows.
pub const HISTORY: usize = 90;
/// Activity entries kept per download.
const LOG_LIMIT: usize = 100;
const SAMPLE: Duration = Duration::from_secs(1);

/// Something that happened to a download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Added,
    Started { total: Option<u64>, resumable: bool, connections: u8, resumed_from: u64 },
    State(DownloadState),
    Moved(QueueId),
    Failed(String),
    Completed(String),
}

impl Activity {
    /// A sentence for the log. `queue_name` names a queue by id.
    pub fn describe(&self, queue_name: impl Fn(QueueId) -> String) -> String {
        match self {
            Activity::Added => t!("details.log_added"),
            Activity::Started { total, resumable, connections, resumed_from } => {
                let mut parts = Vec::new();
                parts.push(match total {
                    Some(size) => t!("details.log_size", size = fmt_bytes(*size)),
                    None => t!("details.log_size_unknown"),
                });
                parts.push(if *resumable {
                    t!("details.log_connections", count = connections)
                } else {
                    t!("details.log_no_resume")
                });
                if *resumed_from > 0 {
                    parts.push(t!("details.log_resumed", size = fmt_bytes(*resumed_from)));
                }
                parts.join(" ")
            }
            Activity::State(DownloadState::Queued) => t!("details.log_queued"),
            Activity::State(DownloadState::Probing) => t!("details.log_probing"),
            Activity::State(DownloadState::Paused) => t!("details.log_paused"),
            Activity::State(DownloadState::Verifying) => t!("details.log_verifying"),
            Activity::State(state) => crate::model::state_label(*state),
            Activity::Moved(queue) => t!("details.log_moved", queue = queue_name(*queue)),
            Activity::Failed(error) => t!("details.log_failed", error = error),
            Activity::Completed(path) => t!("details.log_completed", path = path),
        }
    }

    /// Failures stand out in the log.
    pub fn is_failure(&self) -> bool {
        matches!(self, Activity::Failed(_))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub at: DateTime<Local>,
    pub what: Activity,
}

/// What the panel knows about one download beyond its list row.
#[derive(Debug, Clone, Default)]
pub struct Live {
    /// The file's pieces, in order; empty for single-connection downloads.
    pub segments: Vec<SegmentProgress>,
    /// Connections transferring right now.
    pub connections: u8,
    /// One speed per second, oldest first, at most [`HISTORY`].
    pub speeds: VecDeque<u64>,
    last_sample: Option<Instant>,
    /// Newest last, at most [`LOG_LIMIT`].
    pub log: VecDeque<Entry>,
}

impl Live {
    fn note(&mut self, at: DateTime<Local>, what: Activity) {
        if self.log.len() == LOG_LIMIT {
            self.log.pop_front();
        }
        self.log.push_back(Entry { at, what });
    }

    /// Record the speed once a second; seconds without updates count as 0.
    fn sample(&mut self, bytes_per_sec: u64, now: Instant) {
        let gap = match self.last_sample {
            None => 1,
            Some(last) => {
                let elapsed = now.saturating_duration_since(last);
                if elapsed < SAMPLE {
                    // Keep the latest reading for this second.
                    if let Some(newest) = self.speeds.back_mut() {
                        *newest = bytes_per_sec;
                    }
                    return;
                }
                (elapsed.as_secs() as usize).clamp(1, HISTORY)
            }
        };
        for _ in 1..gap {
            self.push_speed(0);
        }
        self.push_speed(bytes_per_sec);
        self.last_sample = Some(now);
    }

    fn push_speed(&mut self, speed: u64) {
        if self.speeds.len() == HISTORY {
            self.speeds.pop_front();
        }
        self.speeds.push_back(speed);
    }

    pub fn peak(&self) -> u64 {
        self.speeds.iter().copied().max().unwrap_or(0)
    }

    /// Average over the seconds that moved data.
    pub fn average(&self) -> u64 {
        let moving: Vec<u64> = self.speeds.iter().copied().filter(|&s| s > 0).collect();
        if moving.is_empty() { 0 } else { moving.iter().sum::<u64>() / moving.len() as u64 }
    }
}

/// Live data for every download in the list.
#[derive(Debug, Clone, Default)]
pub struct Tracker {
    live: HashMap<DownloadId, Live>,
}

impl Tracker {
    pub fn get(&self, id: DownloadId) -> Option<&Live> {
        self.live.get(&id)
    }

    /// Apply one engine event, seen at `now` (wall clock `at`).
    pub fn apply(&mut self, event: &Event, now: Instant, at: DateTime<Local>) {
        let (id, what) = match event {
            Event::Added { id, .. } => (*id, Some(Activity::Added)),
            Event::Moved { id, queue } => (*id, Some(Activity::Moved(*queue))),
            Event::StateChanged { id, state } => {
                // Started, Failed and Completed say more than their states.
                let shown = !matches!(
                    state,
                    DownloadState::Downloading | DownloadState::Failed | DownloadState::Completed
                );
                (*id, shown.then_some(Activity::State(*state)))
            }
            Event::Started { id, total, resumable, connections, resumed_from, .. } => {
                let live = self.live.entry(*id).or_default();
                live.segments.clear();
                live.connections = *connections;
                let what = Activity::Started {
                    total: *total,
                    resumable: *resumable,
                    connections: *connections,
                    resumed_from: *resumed_from,
                };
                (*id, Some(what))
            }
            Event::Progress { id, bytes_per_sec, segments, connections, .. } => {
                let live = self.live.entry(*id).or_default();
                live.segments.clone_from(segments);
                if !segments.is_empty() {
                    live.connections = *connections;
                }
                live.sample(*bytes_per_sec, now);
                (*id, None)
            }
            Event::Failed { id, error } => {
                if let Some(live) = self.live.get_mut(id) {
                    live.connections = 0;
                }
                (*id, Some(Activity::Failed(error.clone())))
            }
            Event::Completed { id, path } => {
                if let Some(live) = self.live.get_mut(id) {
                    live.connections = 0;
                    for s in &mut live.segments {
                        s.pos = s.end;
                    }
                }
                (*id, Some(Activity::Completed(path.clone())))
            }
            Event::Removed { id } => {
                self.live.remove(id);
                return;
            }
            Event::QueueChanged { .. }
            | Event::QueueRemoved { .. }
            | Event::QueueFinished { .. } => {
                return;
            }
        };
        if let Some(what) = what {
            self.live.entry(id).or_default().note(at, what);
        }
        // A download that stopped has no connections.
        if let Event::StateChanged { id, state } = event
            && *state != DownloadState::Downloading
            && let Some(live) = self.live.get_mut(id)
        {
            live.connections = 0;
        }
    }
}

/// A plain color, so drawing doesn't need iced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

/// `#rrggbb` for an SVG attribute.
pub fn hex(c: Rgb) -> String {
    format!("#{:02x}{:02x}{:02x}", c.0, c.1, c.2)
}

/// Colors for the drawings.
#[derive(Debug, Clone, Copy)]
pub struct Palette {
    pub track: Rgb,
    pub fill_a: Rgb,
    pub fill_b: Rgb,
    pub gap: Rgb,
}

/// The piece map: a `width`×`height` bar where each piece fills as its
/// connection writes. Without pieces, one bar shows `fraction` (if known).
pub fn piece_map(
    segments: &[SegmentProgress],
    fraction: Option<f32>,
    width: f32,
    height: f32,
    p: &Palette,
) -> String {
    let mut svg = svg_open(width, height, p);
    let r = (height / 2.0).min(5.0);
    let _ = write!(
        svg,
        r#"<clipPath id="c"><rect width="{width}" height="{height}" rx="{r}"/></clipPath><g clip-path="url(#c)"><rect width="{width}" height="{height}" fill="{}"/>"#,
        hex(p.track)
    );
    let total = segments.last().map_or(0, |s| s.end);
    if total > 0 {
        let scale = f64::from(width) / total as f64;
        for s in segments {
            let x = s.start as f64 * scale;
            let w = (s.pos - s.start) as f64 * scale;
            if w > 0.0 {
                let _ = write!(
                    svg,
                    r#"<rect x="{x:.2}" width="{w:.2}" height="{height}" fill="url(#g)"/>"#
                );
            }
        }
        // Thin gaps between pieces, when there's room for them (a torrent's
        // hundreds of cells would otherwise be all gap).
        let room = f64::from(width) / segments.len() as f64 >= 6.0;
        for s in segments.iter().skip(1).filter(|_| room) {
            let x = s.start as f64 * scale;
            let _ = write!(
                svg,
                r#"<rect x="{:.2}" width="1.5" height="{height}" fill="{}"/>"#,
                x - 0.75,
                hex(p.gap)
            );
        }
    } else if let Some(f) = fraction {
        let w = f32::clamp(f, 0.0, 1.0) * width;
        let _ = write!(svg, r#"<rect width="{w:.2}" height="{height}" fill="url(#g)"/>"#);
    }
    svg.push_str("</g></svg>");
    svg
}

/// The speed graph: an area chart of `speeds` (one per second, oldest first),
/// scaled to the peak and drawn right-aligned over [`HISTORY`] seconds.
pub fn speed_graph(speeds: &VecDeque<u64>, width: f32, height: f32, p: &Palette) -> String {
    let mut svg = svg_open(width, height, p);
    let peak = speeds.iter().copied().max().unwrap_or(0);
    // Faint guides at the top, middle and bottom.
    for y in [0.5, height / 2.0, height - 0.5] {
        let _ = write!(
            svg,
            r#"<line x1="0" x2="{width}" y1="{y}" y2="{y}" stroke="{}" stroke-width="1"/>"#,
            hex(p.track)
        );
    }
    if peak > 0 && speeds.len() > 1 {
        let step = f64::from(width) / (HISTORY - 1) as f64;
        let x0 = f64::from(width) - step * (speeds.len() - 1) as f64;
        let top = 3.0;
        let usable = f64::from(height) - top - 1.0;
        let points: Vec<(f64, f64)> = speeds
            .iter()
            .enumerate()
            .map(|(i, &s)| {
                let y = top + usable * (1.0 - s as f64 / peak as f64);
                (x0 + step * i as f64, y)
            })
            .collect();
        let line: String =
            points.iter().map(|(x, y)| format!("{x:.1},{y:.1}")).collect::<Vec<_>>().join(" ");
        let _ = write!(
            svg,
            r#"<polygon points="{x0:.1},{height} {line} {width},{height}" fill="url(#g)" fill-opacity="0.22"/><polyline points="{line}" fill="none" stroke="url(#g)" stroke-width="2" stroke-linejoin="round" stroke-linecap="round"/>"#
        );
    }
    svg.push_str("</svg>");
    svg
}

fn svg_open(width: f32, height: f32, p: &Palette) -> String {
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}"><defs><linearGradient id="g" gradientUnits="userSpaceOnUse" x1="0" y1="0" x2="{width}" y2="0"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient></defs>"#,
        hex(p.fill_a),
        hex(p.fill_b)
    )
}

/// "Referer" etc. from a download's saved request headers.
pub fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// The URL with any password in it replaced by dots.
pub fn mask_url(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_owned() };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, path) = rest.split_at(end);
    match authority.rsplit_once('@').and_then(|(info, host)| Some((info.split_once(':')?.0, host)))
    {
        Some((user, host)) => format!("{scheme}://{user}:\u{2022}\u{2022}\u{2022}@{host}{path}"),
        None => url.to_owned(),
    }
}

/// Unix seconds as local date and time, e.g. "2026-10-25 14:03".
pub fn local_time(unix: i64) -> String {
    DateTime::from_timestamp(unix, 0)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// How many cookies a `Cookie` header holds (their values stay hidden).
pub fn cookie_count(cookie: &str) -> usize {
    cookie.split(';').filter(|c| c.contains('=')).count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at() -> DateTime<Local> {
        Local::now()
    }

    fn progress(id: u64, speed: u64, segments: Vec<SegmentProgress>, connections: u8) -> Event {
        Event::Progress {
            id: DownloadId(id),
            downloaded: 0,
            total: None,
            bytes_per_sec: speed,
            segments,
            connections,
        }
    }

    fn seg(start: u64, end: u64, pos: u64) -> SegmentProgress {
        SegmentProgress { start, end, pos }
    }

    #[test]
    fn speeds_are_sampled_once_a_second_with_gaps_as_zero() {
        let mut t = Tracker::default();
        let start = Instant::now();
        t.apply(&progress(1, 100, vec![], 0), start, at());
        t.apply(&progress(1, 150, vec![], 0), start + Duration::from_millis(300), at());
        t.apply(&progress(1, 200, vec![], 0), start + Duration::from_millis(1100), at());
        t.apply(&progress(1, 50, vec![], 0), start + Duration::from_millis(4200), at());
        let live = t.get(DownloadId(1)).unwrap();
        assert_eq!(live.speeds, [150, 200, 0, 0, 50]);
        assert_eq!(live.peak(), 200);
        assert_eq!(live.average(), 133);
    }

    #[test]
    fn history_is_capped() {
        let mut t = Tracker::default();
        let start = Instant::now();
        for i in 0..200u64 {
            t.apply(&progress(1, i, vec![], 0), start + Duration::from_secs(i), at());
        }
        let live = t.get(DownloadId(1)).unwrap();
        assert_eq!(live.speeds.len(), HISTORY);
        assert_eq!(live.speeds.back(), Some(&199));
        // A long pause clears the graph rather than overflowing it.
        t.apply(&progress(1, 7, vec![], 0), start + Duration::from_secs(10_000), at());
        let live = t.get(DownloadId(1)).unwrap();
        assert_eq!(live.speeds.len(), HISTORY);
        assert_eq!(live.peak(), 7);
    }

    #[test]
    fn pieces_and_connections_follow_progress() {
        let mut t = Tracker::default();
        let id = DownloadId(3);
        let started = Event::Started {
            id,
            file_name: "f".into(),
            total: Some(100),
            resumable: true,
            connections: 4,
            resumed_from: 0,
        };
        t.apply(&started, Instant::now(), at());
        assert_eq!(t.get(id).unwrap().connections, 4);
        t.apply(&progress(3, 10, vec![seg(0, 50, 20), seg(50, 100, 60)], 2), Instant::now(), at());
        assert_eq!(t.get(id).unwrap().connections, 2);
        assert_eq!(t.get(id).unwrap().segments.len(), 2);

        t.apply(&Event::Completed { id, path: "f".into() }, Instant::now(), at());
        let live = t.get(id).unwrap();
        assert_eq!(live.connections, 0);
        assert!(live.segments.iter().all(|s| s.pos == s.end));

        t.apply(&Event::Removed { id }, Instant::now(), at());
        assert!(t.get(id).is_none());
    }

    #[test]
    fn the_log_skips_states_other_events_describe() {
        let mut t = Tracker::default();
        let id = DownloadId(1);
        let now = Instant::now();
        for state in [DownloadState::Probing, DownloadState::Downloading, DownloadState::Paused] {
            t.apply(&Event::StateChanged { id, state }, now, at());
        }
        t.apply(&Event::Failed { id, error: "boom".into() }, now, at());
        t.apply(&Event::StateChanged { id, state: DownloadState::Failed }, now, at());
        let kinds: Vec<_> = t.get(id).unwrap().log.iter().map(|e| e.what.clone()).collect();
        assert_eq!(
            kinds,
            [
                Activity::State(DownloadState::Probing),
                Activity::State(DownloadState::Paused),
                Activity::Failed("boom".into()),
            ]
        );
        assert!(kinds[2].is_failure());
    }

    #[test]
    fn the_log_is_capped() {
        let mut t = Tracker::default();
        let id = DownloadId(1);
        for _ in 0..(LOG_LIMIT + 10) {
            let ev = Event::StateChanged { id, state: DownloadState::Paused };
            t.apply(&ev, Instant::now(), at());
        }
        assert_eq!(t.get(id).unwrap().log.len(), LOG_LIMIT);
    }

    #[test]
    fn descriptions_read_naturally() {
        let started = Activity::Started {
            total: Some(2048),
            resumable: true,
            connections: 8,
            resumed_from: 1024,
        };
        assert_eq!(
            started.describe(|_| String::new()),
            "Server reports 2.0 KiB. Connections: 8. Resuming at 1.0 KiB."
        );
        let single =
            Activity::Started { total: None, resumable: false, connections: 1, resumed_from: 0 };
        assert_eq!(
            single.describe(|_| String::new()),
            "The server didn't report the size. The server can't resume, so one connection is used."
        );
        assert_eq!(
            Activity::Moved(QueueId(2)).describe(|_| "Overnight".into()),
            "Moved to Overnight."
        );
    }

    fn palette() -> Palette {
        let c = Rgb(1, 2, 3);
        Palette { track: c, fill_a: c, fill_b: c, gap: c }
    }

    #[test]
    fn the_piece_map_draws_each_piece() {
        let svg = piece_map(&[seg(0, 50, 25), seg(50, 100, 100)], None, 200.0, 10.0, &palette());
        assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
        assert!(svg.contains(r#"<rect x="0.00" width="50.00""#), "{svg}");
        assert!(svg.contains(r#"<rect x="100.00" width="100.00""#), "{svg}");
        // One gap, between the two pieces.
        assert_eq!(svg.matches(r#"width="1.5""#).count(), 1);

        // Too many cells for gaps: just the fills.
        let many: Vec<_> = (0..100).map(|i| seg(i * 10, i * 10 + 10, i * 10 + 10)).collect();
        let dense = piece_map(&many, None, 200.0, 10.0, &palette());
        assert_eq!(dense.matches(r#"width="1.5""#).count(), 0);
        assert_eq!(dense.matches(r#"fill="url(#g)""#).count(), 100);

        let plain = piece_map(&[], Some(0.5), 200.0, 10.0, &palette());
        assert!(plain.contains(r#"<rect width="100.00""#), "{plain}");
        assert_eq!(hex(Rgb(255, 16, 0)), "#ff1000");
    }

    #[test]
    fn the_speed_graph_scales_to_its_peak() {
        let speeds: VecDeque<u64> = [0, 50, 100].into();
        let svg = speed_graph(&speeds, 178.0, 40.0, &palette());
        assert!(svg.contains("<polyline"), "{svg}");
        // Right-aligned: the newest point is at the right edge, the peak at the top.
        assert!(svg.contains("178.0,3.0"), "{svg}");
        let empty = speed_graph(&VecDeque::new(), 178.0, 40.0, &palette());
        assert!(!empty.contains("<polyline"));
    }

    #[test]
    fn request_headers_are_found_and_cookies_counted() {
        let headers = vec![("Referer".to_owned(), "https://a/".to_owned())];
        assert_eq!(header(&headers, "referer"), Some("https://a/"));
        assert_eq!(header(&headers, "Cookie"), None);
        assert_eq!(cookie_count("a=1; b=2; c"), 2);
    }

    #[test]
    fn passwords_in_addresses_are_hidden() {
        assert_eq!(
            mask_url("ftp://me:secret@files.example.org/a@b.zip"),
            "ftp://me:\u{2022}\u{2022}\u{2022}@files.example.org/a@b.zip"
        );
        assert_eq!(mask_url("https://me@example.com/x"), "https://me@example.com/x");
        assert_eq!(mask_url("https://example.com/x?a=b:c@d"), "https://example.com/x?a=b:c@d");
        assert_eq!(local_time(0).len(), "1970-01-01 00:00".len());
    }
}
