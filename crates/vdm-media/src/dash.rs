//! DASH (MPD) manifests for on-demand (static) streams.
//!
//! Only the first `<Period>` is read. Joining periods would need each one's
//! timeline offset and numbering, and multi-period VOD is rare. A period's
//! length (used for segment counts and `r="-1"` repeats) comes from its
//! `duration`, else the next period's `start`, else the presentation duration.
//!
//! Not handled: a `SegmentList` with a `SegmentTimeline` (only its `duration`
//! is used), `SegmentBase` index ranges (the whole file is one segment), and
//! remote periods (`xlink:href`).

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Decoder, Reader, XmlVersion};
use url::Url;

use crate::{Container, ParseError, Segment, Track, TrackKind, Variant};

/// Upper bound on segments per track, so a hostile `r` cannot exhaust memory.
const MAX_SEGMENTS: usize = 1_000_000;

/// One selectable representation: a quality of a video or audio track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Representation {
    pub id: String,
    pub variant: Variant,
    pub track: Track,
}

/// A parsed static MPD: its video and audio representations, each list sorted
/// by bandwidth ascending. Text, image and other adaptation sets are skipped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub video: Vec<Representation>,
    pub audio: Vec<Representation>,
    /// mediaPresentationDuration in ms, if given.
    pub duration_ms: Option<u64>,
}

/// Whether the text's root element is `<MPD>`. A BOM, XML declaration,
/// comments and processing instructions before it are allowed.
pub fn is_mpd(text: &str) -> bool {
    let mut reader = Reader::from_str(strip_bom(text));
    loop {
        match reader.read_event() {
            Ok(Event::Start(e) | Event::Empty(e)) => {
                return e.local_name().as_ref() == &b"MPD"[..];
            }
            Ok(Event::Text(t)) if t.iter().all(u8::is_ascii_whitespace) => {}
            Ok(Event::Eof | Event::Text(_) | Event::GeneralRef(_) | Event::CData(_)) | Err(_) => {
                return false;
            }
            // Declaration, comments, processing instructions, DOCTYPE.
            Ok(_) => {}
        }
    }
}

/// Parses a static MPD. Relative URLs resolve against `base`, the MPD's own URL.
pub fn parse(text: &str, base: &Url) -> Result<Manifest, ParseError> {
    let root = parse_dom(text)?;
    if root.name != "MPD" {
        return Err(ParseError::new("not an MPD manifest: the root element is not <MPD>"));
    }
    if root.attr("type") == Some("dynamic") {
        return Err(ParseError::new("live DASH streams are not supported"));
    }
    let duration_ms = parse_optional_duration(root.attr("mediaPresentationDuration"))?;

    let mpd_scope = scope_base(None, &root, base)?;
    let periods: Vec<&Node> = root.children_named("Period").collect();
    let period = periods.first().copied().ok_or_else(|| ParseError::new("MPD has no <Period>"))?;

    let period_start = parse_optional_duration(period.attr("start"))?.unwrap_or(0);
    let next_start = match periods.get(1).and_then(|p| p.attr("start")) {
        Some(start) => Some(parse_duration_ms(start)?),
        None => None,
    };
    let length_ms = match period.attr("duration") {
        Some(duration) => Some(parse_duration_ms(duration)?),
        None => match next_start {
            Some(next) => Some(next.saturating_sub(period_start)),
            None => duration_ms.map(|d| d.saturating_sub(period_start)),
        },
    };

    let period_scope = scope_base(mpd_scope.as_ref(), period, base)?;
    let ctx = PeriodCtx { base, length_ms };
    let mut video = Vec::new();
    let mut audio = Vec::new();
    for set in period.children_named("AdaptationSet") {
        let set_scope = scope_base(period_scope.as_ref(), set, base)?;
        for rep in set.children_named("Representation") {
            let Some(kind) = media_kind(set, rep) else { continue };
            ensure_clear(&[period, set, rep])?;
            let rep_scope = scope_base(set_scope.as_ref(), rep, base)?;
            let representation = build_representation(set, rep, kind, rep_scope, &ctx)?;
            match kind {
                TrackKind::Audio => audio.push(representation),
                TrackKind::Video | TrackKind::Muxed => video.push(representation),
            }
        }
    }
    // Stable sort: equal bandwidths keep document order.
    video.sort_by_key(|r| r.variant.bandwidth);
    audio.sort_by_key(|r| r.variant.bandwidth);
    Ok(Manifest { video, audio, duration_ms })
}

/// The audio representation to pair with video: the highest bandwidth, or
/// the first such one in the manifest on a tie. `None` if there is no audio.
pub fn best_audio(manifest: &Manifest) -> Option<&Representation> {
    // Reversed, so that among equal bandwidths `max_by_key` keeps the first.
    manifest.audio.iter().rev().max_by_key(|r| r.variant.bandwidth)
}

// ---------------------------------------------------------------------------
// A minimal element tree, so the MPD logic can look at parents and siblings.

#[derive(Debug, Default)]
struct Node {
    name: String,
    attrs: Vec<(String, String)>,
    children: Vec<Node>,
    /// Concatenated text content (used for `BaseURL`).
    text: String,
}

impl Node {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn child<'a>(&'a self, name: &'a str) -> Option<&'a Node> {
        self.children_named(name).next()
    }

    fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.children.iter().filter(move |c| c.name == name)
    }
}

fn parse_dom(text: &str) -> Result<Node, ParseError> {
    let mut reader = Reader::from_str(strip_bom(text));
    let decoder = reader.decoder();
    // Index 0 is a placeholder for the document; the root element is its child.
    let mut stack = vec![Node::default()];
    loop {
        match reader.read_event().map_err(xml_error)? {
            Event::Start(e) => stack.push(element(&e, decoder)?),
            Event::Empty(e) => attach(&mut stack, element(&e, decoder)?),
            Event::End(_) => {
                if stack.len() < 2 {
                    return Err(xml_error("unexpected end tag"));
                }
                let node = stack.pop().unwrap_or_default();
                attach(&mut stack, node);
            }
            Event::Text(t) => push_text(&mut stack, &t.decode().map_err(xml_error)?),
            Event::CData(c) => push_text(&mut stack, &c.decode().map_err(xml_error)?),
            Event::GeneralRef(r) => {
                let resolved = match r.resolve_char_ref().map_err(xml_error)? {
                    Some(ch) => ch.to_string(),
                    None => {
                        let name = r.decode().map_err(xml_error)?;
                        quick_xml::escape::unescape(&format!("&{name};"))
                            .map_err(xml_error)?
                            .into_owned()
                    }
                };
                push_text(&mut stack, &resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if stack.len() != 1 {
        return Err(xml_error("unexpected end of document"));
    }
    let document = stack.pop().unwrap_or_default();
    document.children.into_iter().next().ok_or_else(|| xml_error("no root element"))
}

fn element(start: &BytesStart<'_>, decoder: Decoder) -> Result<Node, ParseError> {
    let mut node = Node { name: name_of(start.local_name().as_ref()), ..Node::default() };
    for attr in start.attributes() {
        let attr = attr.map_err(xml_error)?;
        let key = name_of(attr.key.local_name().as_ref());
        // DASH manifests are XML 1.0.
        let value = attr
            .decoded_and_normalized_value(XmlVersion::Implicit1_0, decoder)
            .map_err(xml_error)?;
        node.attrs.push((key, value.into_owned()));
    }
    Ok(node)
}

fn attach(stack: &mut [Node], node: Node) {
    if let Some(parent) = stack.last_mut() {
        parent.children.push(node);
    }
}

fn push_text(stack: &mut [Node], text: &str) {
    if let Some(top) = stack.last_mut() {
        top.text.push_str(text);
    }
}

fn name_of(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn strip_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

fn xml_error(e: impl std::fmt::Display) -> ParseError {
    ParseError::new(format!("invalid MPD XML: {e}"))
}

// ---------------------------------------------------------------------------
// Inheritance and URLs.

/// What a representation needs from the period it sits in.
struct PeriodCtx<'a> {
    base: &'a Url,
    /// The period's length in ms, if known.
    length_ms: Option<u64>,
}

/// The BaseURL in effect for `node`, given the one in effect for its parent.
/// A BaseURL child resolves against the parent's; without one the parent's
/// is kept. `None` means no BaseURL has been set anywhere above.
fn scope_base(parent: Option<&Url>, node: &Node, base: &Url) -> Result<Option<Url>, ParseError> {
    let own = node.children_named("BaseURL").map(|b| b.text.trim()).find(|t| !t.is_empty());
    match own {
        Some(text) => parent
            .unwrap_or(base)
            .join(text)
            .map(Some)
            .map_err(|e| ParseError::new(format!("bad BaseURL `{text}`: {e}"))),
        None => Ok(parent.cloned()),
    }
}

fn join(base: &Url, relative: &str) -> Result<Url, ParseError> {
    base.join(relative).map_err(|e| ParseError::new(format!("bad URL `{relative}`: {e}")))
}

/// Text, image and other non-media adaptation sets give `None`.
fn media_kind(set: &Node, rep: &Node) -> Option<TrackKind> {
    let declared =
        set.attr("contentType").or_else(|| rep.attr("mimeType").or(set.attr("mimeType")))?;
    let major = declared.split('/').next()?.trim().to_ascii_lowercase();
    match major.as_str() {
        "video" => Some(TrackKind::Video),
        "audio" => Some(TrackKind::Audio),
        _ => None,
    }
}

fn container_for(mime: Option<&str>) -> Container {
    let essence = mime.and_then(|m| m.split(';').next()).map(|m| m.trim().to_ascii_lowercase());
    match essence.as_deref() {
        Some("video/webm" | "audio/webm") => Container::WebM,
        Some("video/mp2t") => Container::Ts,
        _ => Container::Mp4,
    }
}

/// Refuses DRM. Any `ContentProtection` on the period, set or representation counts.
fn ensure_clear(nodes: &[&Node]) -> Result<(), ParseError> {
    if nodes.iter().any(|n| n.child("ContentProtection").is_some()) {
        Err(ParseError::new("DRM-protected streams are not supported"))
    } else {
        Ok(())
    }
}

fn build_representation(
    set: &Node,
    rep: &Node,
    kind: TrackKind,
    scope: Option<Url>,
    ctx: &PeriodCtx<'_>,
) -> Result<Representation, ParseError> {
    let id = rep.attr("id").unwrap_or_default().to_owned();
    let bandwidth: u64 = parse_num(rep.attr("bandwidth"), "bandwidth")?
        .ok_or_else(|| ParseError::new(format!("representation `{id}` has no bandwidth")))?;
    // Representation wins over AdaptationSet for every inherited attribute.
    let inherited = |name: &str| rep.attr(name).or_else(|| set.attr(name));
    let width: Option<u32> = parse_num(inherited("width"), "width")?;
    let height: Option<u32> = parse_num(inherited("height"), "height")?;
    let codecs = inherited("codecs").map(str::to_owned);
    let container = container_for(inherited("mimeType"));

    let (init, segments) = build_segments(set, rep, &id, bandwidth, scope, ctx)?;
    Ok(Representation {
        id,
        variant: Variant { bandwidth, width, height, codecs },
        track: Track { kind, container, init, segments },
    })
}

// ---------------------------------------------------------------------------
// Segments.

/// Builds the init segment and the media segments for one representation.
fn build_segments(
    set: &Node,
    rep: &Node,
    id: &str,
    bandwidth: u64,
    scope: Option<Url>,
    ctx: &PeriodCtx<'_>,
) -> Result<(Option<Segment>, Vec<Segment>), ParseError> {
    // A representation's own segment element replaces the set's. The exception
    // is SegmentTemplate: its attributes merge, the representation's winning.
    let own = ["SegmentTemplate", "SegmentList", "SegmentBase"]
        .iter()
        .any(|name| rep.child(name).is_some());
    let (source, fallback) = if own { (rep, Some(set)) } else { (set, None) };
    let base = scope.clone().unwrap_or_else(|| ctx.base.clone());

    if let Some(primary) = source.child("SegmentTemplate") {
        let template =
            Template { primary, fallback: fallback.and_then(|f| f.child("SegmentTemplate")) };
        return template_segments(&template, id, bandwidth, &base, ctx.length_ms);
    }
    if let Some(list) = source.child("SegmentList") {
        return list_segments(list, &base);
    }
    // SegmentBase, or nothing at all: the whole file is one segment at the BaseURL.
    let url = scope.ok_or_else(|| {
        ParseError::new(format!("representation `{id}` has no segment information or BaseURL"))
    })?;
    let duration_ms = ctx.length_ms.unwrap_or(0);
    Ok((None, vec![Segment { duration_ms, ..Segment::new(url) }]))
}

/// A SegmentTemplate, with the AdaptationSet's template as fallback for any
/// attribute or SegmentTimeline it lacks.
struct Template<'a> {
    primary: &'a Node,
    fallback: Option<&'a Node>,
}

impl Template<'_> {
    fn attr(&self, name: &str) -> Option<&str> {
        self.primary.attr(name).or_else(|| self.fallback.and_then(|f| f.attr(name)))
    }

    fn timeline(&self) -> Option<&Node> {
        self.primary
            .child("SegmentTimeline")
            .or_else(|| self.fallback.and_then(|f| f.child("SegmentTimeline")))
    }
}

/// The resolved numbers and URLs a media template is expanded with.
struct Tpl<'a> {
    media: &'a str,
    id: &'a str,
    bandwidth: u64,
    timescale: u64,
    start_number: u64,
    /// presentationTimeOffset, in timescale ticks.
    offset: u64,
    base: &'a Url,
}

impl Tpl<'_> {
    fn segment(&self, number: u64, time: u64, duration_ms: u64) -> Result<Segment, ParseError> {
        let vars =
            Vars { id: self.id, bandwidth: self.bandwidth, number: Some(number), time: Some(time) };
        let url = join(self.base, &expand(self.media, &vars)?)?;
        Ok(Segment { duration_ms, ..Segment::new(url) })
    }
}

fn template_segments(
    t: &Template<'_>,
    id: &str,
    bandwidth: u64,
    base: &Url,
    length_ms: Option<u64>,
) -> Result<(Option<Segment>, Vec<Segment>), ParseError> {
    let timescale: u64 = parse_num(t.attr("timescale"), "timescale")?.unwrap_or(1);
    let start_number: u64 = parse_num(t.attr("startNumber"), "startNumber")?.unwrap_or(1);
    let offset: u64 =
        parse_num(t.attr("presentationTimeOffset"), "presentationTimeOffset")?.unwrap_or(0);
    if timescale == 0 {
        return Err(ParseError::new(format!("representation `{id}`: timescale must be positive")));
    }
    let media = t.attr("media").ok_or_else(|| {
        ParseError::new(format!("representation `{id}`: SegmentTemplate has no media attribute"))
    })?;
    let tpl = Tpl { media, id, bandwidth, timescale, start_number, offset, base };

    let init = match t.attr("initialization") {
        Some(template) => {
            let vars = Vars { id, bandwidth, number: None, time: None };
            Some(Segment::new(join(base, &expand(template, &vars)?)?))
        }
        None => None,
    };
    let segments = match t.timeline() {
        Some(timeline) => timeline_segments(timeline, &tpl, length_ms)?,
        None => duration_segments(&tpl, t.attr("duration"), length_ms)?,
    };
    Ok((init, segments))
}

/// Segments from a `SegmentTimeline`. `t` defaults to the previous segment's
/// end (the presentation time offset for the first). `r` repeats the `<S>`,
/// and `r="-1"` repeats it up to the next `<S>` with a `t`, or the period end.
fn timeline_segments(
    timeline: &Node,
    tpl: &Tpl<'_>,
    length_ms: Option<u64>,
) -> Result<Vec<Segment>, ParseError> {
    let entries: Vec<&Node> = timeline.children_named("S").collect();
    let mut out: Vec<Segment> = Vec::new();
    let mut cursor = tpl.offset;
    let mut number = tpl.start_number;
    for (i, s) in entries.iter().enumerate() {
        let d: u64 = parse_num(s.attr("d"), "d")?
            .filter(|&d| d > 0)
            .ok_or_else(|| ParseError::new("SegmentTimeline <S> needs a positive d"))?;
        let r: i64 = parse_num(s.attr("r"), "r")?.unwrap_or(0);
        let start: u64 = parse_num(s.attr("t"), "t")?.unwrap_or(cursor);
        let repeats: u64 = match r {
            r if r >= 0 => r as u64 + 1,
            -1 => {
                let next_t: Option<u64> = match entries.get(i + 1) {
                    Some(next) => parse_num(next.attr("t"), "t")?,
                    None => None,
                };
                let end = next_t
                    .or_else(|| {
                        length_ms
                            .map(|ms| tpl.offset.saturating_add(ms_to_ticks(ms, tpl.timescale)))
                    })
                    .ok_or_else(|| {
                        ParseError::new(
                            "SegmentTimeline r=\"-1\" needs a known presentation duration",
                        )
                    })?;
                if start >= end { 0 } else { (end - start).div_ceil(d) }
            }
            _ => {
                return Err(ParseError::new(format!("SegmentTimeline r=\"{r}\" is not supported")));
            }
        };
        if repeats > (MAX_SEGMENTS - out.len()) as u64 {
            return Err(too_many_segments());
        }
        let duration_ms = ticks_to_ms(d, tpl.timescale);
        for j in 0..repeats {
            let time = start.saturating_add(j.saturating_mul(d));
            out.push(tpl.segment(number, time, duration_ms)?);
            number = number.saturating_add(1);
        }
        cursor = start.saturating_add(repeats.saturating_mul(d));
    }
    Ok(out)
}

/// Segments from a plain `duration`: the count rounds up to cover the period,
/// and the last segment is cut short at the period end.
fn duration_segments(
    tpl: &Tpl<'_>,
    duration: Option<&str>,
    length_ms: Option<u64>,
) -> Result<Vec<Segment>, ParseError> {
    let d: u64 = parse_num(duration, "duration")?.filter(|&d| d > 0).ok_or_else(|| {
        ParseError::new(format!(
            "representation `{}`: SegmentTemplate needs a SegmentTimeline or a positive duration",
            tpl.id
        ))
    })?;
    let total = length_ms.ok_or_else(|| {
        ParseError::new("cannot count segments: the presentation has no duration")
    })?;
    let count = (u128::from(total) * u128::from(tpl.timescale)).div_ceil(1000 * u128::from(d));
    if count > MAX_SEGMENTS as u128 {
        return Err(too_many_segments());
    }
    let seg_ms = ticks_to_ms(d, tpl.timescale);
    (0..count as u64)
        .map(|i| {
            let time = i.saturating_mul(d);
            let start_ms = ticks_to_ms(time, tpl.timescale);
            let duration_ms = seg_ms.min(total.saturating_sub(start_ms));
            tpl.segment(tpl.start_number.saturating_add(i), time, duration_ms)
        })
        .collect()
}

/// Segments from a `SegmentList`: an optional `Initialization`, then one
/// `SegmentURL` per segment, each with an optional byte range.
fn list_segments(list: &Node, base: &Url) -> Result<(Option<Segment>, Vec<Segment>), ParseError> {
    let timescale: u64 = parse_num(list.attr("timescale"), "timescale")?.unwrap_or(1);
    if timescale == 0 {
        return Err(ParseError::new("SegmentList timescale must be positive"));
    }
    let duration_ms = match parse_num::<u64>(list.attr("duration"), "duration")? {
        Some(d) => ticks_to_ms(d, timescale),
        None => 0,
    };
    let init = match list.child("Initialization") {
        Some(init) => Some(located(init.attr("sourceURL"), init.attr("range"), base)?),
        None => None,
    };
    let mut segments = Vec::new();
    for item in list.children_named("SegmentURL") {
        if segments.len() >= MAX_SEGMENTS {
            return Err(too_many_segments());
        }
        let mut segment = located(item.attr("media"), item.attr("mediaRange"), base)?;
        segment.duration_ms = duration_ms;
        segments.push(segment);
    }
    Ok((init, segments))
}

/// A segment at `source` (or the base when absent), limited to `range` if given.
fn located(source: Option<&str>, range: Option<&str>, base: &Url) -> Result<Segment, ParseError> {
    let url = match source {
        Some(source) => join(base, source)?,
        None => base.clone(),
    };
    let range = range.map(parse_range).transpose()?;
    Ok(Segment { range, ..Segment::new(url) })
}

fn too_many_segments() -> ParseError {
    ParseError::new(format!("too many segments (limit is {MAX_SEGMENTS})"))
}

/// `"a-b"`, inclusive on both ends, as `(offset, length)`.
fn parse_range(text: &str) -> Result<(u64, u64), ParseError> {
    let bad = || ParseError::new(format!("invalid byte range `{text}`"));
    let (first, last) = text.trim().split_once('-').ok_or_else(bad)?;
    let first: u64 = first.trim().parse().map_err(|_| bad())?;
    let last: u64 = last.trim().parse().map_err(|_| bad())?;
    if last < first {
        return Err(bad());
    }
    Ok((first, last - first + 1))
}

// ---------------------------------------------------------------------------
// Template expansion.

struct Vars<'a> {
    id: &'a str,
    bandwidth: u64,
    number: Option<u64>,
    time: Option<u64>,
}

/// Expands `$...$` identifiers in a DASH URL template. `$$` is a literal `$`.
fn expand(template: &str, vars: &Vars<'_>) -> Result<String, ParseError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(pos) = rest.find('$') {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 1..];
        if let Some(tail) = after.strip_prefix('$') {
            out.push('$');
            rest = tail;
            continue;
        }
        let Some(end) = after.find('$') else {
            return Err(ParseError::new(format!("unterminated `$` in `{template}`")));
        };
        out.push_str(&identifier(&after[..end], vars, template)?);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// One identifier such as `Number`, or `Number%05d` with a zero-padded width.
fn identifier(token: &str, vars: &Vars<'_>, template: &str) -> Result<String, ParseError> {
    let (name, width) = match token.split_once('%') {
        Some((name, spec)) => (name, width_of(spec, template)?),
        None => (token, 0),
    };
    let value = match name {
        "RepresentationID" => return Ok(vars.id.to_owned()),
        "Bandwidth" => vars.bandwidth,
        "Number" => vars.number.ok_or_else(|| unavailable(name, template))?,
        "Time" => vars.time.ok_or_else(|| unavailable(name, template))?,
        other => {
            return Err(ParseError::new(format!(
                "unknown template variable `${other}$` in `{template}`"
            )));
        }
    };
    Ok(format!("{value:0width$}"))
}

/// The width in `%0Nd`, `%Nd` or `%d` (the part after `%`).
fn width_of(spec: &str, template: &str) -> Result<usize, ParseError> {
    let bad = || ParseError::new(format!("unsupported format `%{spec}` in `{template}`"));
    let digits = spec.strip_suffix('d').ok_or_else(bad)?;
    let digits = digits.strip_prefix('0').unwrap_or(digits);
    if digits.is_empty() { Ok(0) } else { digits.parse().map_err(|_| bad()) }
}

fn unavailable(name: &str, template: &str) -> ParseError {
    ParseError::new(format!("`${name}$` is not valid here (in `{template}`)"))
}

// ---------------------------------------------------------------------------
// Numbers and durations.

/// Parses an optional numeric attribute, failing if it is present but malformed.
fn parse_num<T: std::str::FromStr>(raw: Option<&str>, name: &str) -> Result<Option<T>, ParseError> {
    raw.map(|s| {
        s.trim()
            .parse::<T>()
            .map_err(|_| ParseError::new(format!("`{name}` is not a valid number: `{s}`")))
    })
    .transpose()
}

fn parse_optional_duration(raw: Option<&str>) -> Result<Option<u64>, ParseError> {
    raw.map(parse_duration_ms).transpose()
}

/// Converts timescale ticks to ms, rounding down.
fn ticks_to_ms(ticks: u64, timescale: u64) -> u64 {
    u64::try_from(u128::from(ticks) * 1000 / u128::from(timescale)).unwrap_or(u64::MAX)
}

fn ms_to_ticks(ms: u64, timescale: u64) -> u64 {
    u64::try_from(u128::from(ms) * u128::from(timescale) / 1000).unwrap_or(u64::MAX)
}

const DAY_MS: u128 = 86_400_000;
/// Years and months have no fixed length; these approximations are used.
const DATE_UNITS: [(char, u128); 4] =
    [('Y', 365 * DAY_MS), ('M', 30 * DAY_MS), ('W', 7 * DAY_MS), ('D', DAY_MS)];
const TIME_UNITS: [(char, u128); 3] = [('H', 3_600_000), ('M', 60_000), ('S', 1_000)];

/// Parses an ISO 8601 duration such as `PT1H2M3.5S` or `P0Y0M0DT0H3M30.000S`
/// into milliseconds. Years count as 365 days and months as 30.
fn parse_duration_ms(text: &str) -> Result<u64, ParseError> {
    let invalid = || ParseError::new(format!("invalid ISO 8601 duration `{text}`"));
    let body = text.trim().strip_prefix('P').ok_or_else(invalid)?;
    let (date, time) = match body.split_once('T') {
        Some((date, time)) => (date, Some(time)),
        None => (body, None),
    };
    let (mut total, mut count) = components(date, &DATE_UNITS).ok_or_else(invalid)?;
    if let Some(time) = time {
        // "PT" and "P1DT" are invalid: a T must be followed by a time component.
        let (ms, n) = components(time, &TIME_UNITS).ok_or_else(invalid)?;
        if n == 0 {
            return Err(invalid());
        }
        total = total.checked_add(ms).ok_or_else(invalid)?;
        count += n;
    }
    if count == 0 {
        return Err(invalid());
    }
    u64::try_from(total).map_err(|_| invalid())
}

/// Sums `<number><designator>` pairs. Designators must appear in the order
/// given, each at most once. Returns the total and the number of components.
fn components(part: &str, units: &[(char, u128)]) -> Option<(u128, usize)> {
    let mut total: u128 = 0;
    let mut count = 0;
    let mut next = 0;
    let mut number = String::new();
    for c in part.chars() {
        if c.is_ascii_digit() || c == '.' || c == ',' {
            number.push(c);
            continue;
        }
        let index = units.iter().position(|(d, _)| *d == c)?;
        if index < next || number.is_empty() {
            return None;
        }
        total = total.checked_add(decimal_ms(&number, units[index].1)?)?;
        number.clear();
        next = index + 1;
        count += 1;
    }
    if !number.is_empty() {
        return None;
    }
    Some((total, count))
}

/// `number` times `unit_ms`, where `number` may have a fraction (`3.5`, `1,5`).
fn decimal_ms(number: &str, unit_ms: u128) -> Option<u128> {
    let (whole, fraction) = number.split_once(['.', ',']).unwrap_or((number, ""));
    let all_digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
        return None;
    }
    let whole_ms = whole.parse::<u128>().ok()?.checked_mul(unit_ms)?;
    let fraction_ms = if fraction.is_empty() {
        0
    } else {
        let scale = 10u128.checked_pow(u32::try_from(fraction.len()).ok()?)?;
        fraction.parse::<u128>().ok()?.checked_mul(unit_ms)? / scale
    };
    whole_ms.checked_add(fraction_ms)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://cdn.example.com/video/manifest.mpd").unwrap()
    }

    /// A static MPD with a 10 s presentation around `body`.
    fn mpd(body: &str) -> String {
        mpd_for("PT10S", body)
    }

    fn mpd_for(duration: &str, body: &str) -> String {
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="{duration}" profiles="urn:mpeg:dash:profile:isoff-on-demand:2011">
{body}
</MPD>"#
        )
    }

    fn urls(track: &Track) -> Vec<String> {
        track.segments.iter().map(|s| s.url.to_string()).collect()
    }

    #[test]
    fn iso_8601_durations() {
        assert_eq!(parse_duration_ms("PT634.566S"), Ok(634_566));
        assert_eq!(parse_duration_ms("P0Y0M0DT0H3M30.000S"), Ok(210_000));
        assert_eq!(parse_duration_ms("PT1H2M3.5S"), Ok(3_723_500));
        assert_eq!(parse_duration_ms("P1DT12H"), Ok(129_600_000));
        assert_eq!(parse_duration_ms("P2W"), Ok(1_209_600_000));
        assert_eq!(parse_duration_ms("PT90M"), Ok(5_400_000));
        assert_eq!(parse_duration_ms("PT0.5S"), Ok(500));
        assert_eq!(parse_duration_ms("PT1,25S"), Ok(1_250));
        assert_eq!(parse_duration_ms("P1M"), Ok(30 * 86_400_000));
    }

    #[test]
    fn invalid_iso_8601_durations() {
        for bad in [
            "", "P", "PT", "P1DT", "T1S", "1H", "-PT1S", "PT1.5", "PT5X", "PT1H1H", "PT1M1H",
            "P1H", "PTS", "PT1.5.5S", "PT-1S", "PT.5S",
        ] {
            assert!(parse_duration_ms(bad).is_err(), "`{bad}` should be rejected");
        }
    }

    #[test]
    fn recognises_mpd_roots() {
        assert!(is_mpd(r#"<?xml version="1.0"?><MPD xmlns="urn:mpeg:dash:schema:mpd:2011"/>"#));
        assert!(is_mpd(
            "\u{feff}<!-- generated --><?xml version=\"1.0\"?>\n<MPD type=\"static\"></MPD>"
        ));
        assert!(is_mpd("<MPD/>"));
        assert!(!is_mpd("<?xml version=\"1.0\"?><svg xmlns=\"http://www.w3.org/2000/svg\"/>"));
        assert!(!is_mpd("<!-- <MPD> --> <Foo/>"));
        assert!(!is_mpd("#EXTM3U\n#EXT-X-VERSION:3"));
        assert!(!is_mpd("<html><body>MPD</body></html>"));
        assert!(!is_mpd(""));
    }

    #[test]
    fn timeline_with_repeats_negative_repeat_and_implicit_times() {
        let text = mpd(r#"
<Period>
 <AdaptationSet mimeType="video/mp4" contentType="video">
  <Representation id="v1" bandwidth="1000000" width="1280" height="720" codecs="avc1.64001f">
   <SegmentTemplate timescale="1000" initialization="init-$RepresentationID$.mp4" media="seg-$RepresentationID$-$Time$.m4s">
    <SegmentTimeline>
     <S t="0" d="2000" r="1"/>
     <S d="3000"/>
     <S d="2000" r="-1"/>
    </SegmentTimeline>
   </SegmentTemplate>
  </Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        let rep = &m.video[0];
        assert_eq!(rep.id, "v1");
        assert_eq!(rep.variant.width, Some(1280));
        assert_eq!(rep.variant.height, Some(720));
        assert_eq!(rep.variant.codecs.as_deref(), Some("avc1.64001f"));
        assert_eq!(rep.track.container, Container::Mp4);
        let init = rep.track.init.as_ref().unwrap();
        assert_eq!(init.url.as_str(), "https://cdn.example.com/video/init-v1.mp4");
        assert_eq!(
            urls(&rep.track),
            [
                "https://cdn.example.com/video/seg-v1-0.m4s",
                "https://cdn.example.com/video/seg-v1-2000.m4s",
                "https://cdn.example.com/video/seg-v1-4000.m4s",
                "https://cdn.example.com/video/seg-v1-7000.m4s",
                "https://cdn.example.com/video/seg-v1-9000.m4s",
            ]
        );
        let durations: Vec<u64> = rep.track.segments.iter().map(|s| s.duration_ms).collect();
        assert_eq!(durations, [2000, 2000, 3000, 2000, 2000]);
        assert_eq!(rep.track.duration_ms(), 11_000);
    }

    #[test]
    fn duration_template_counts_up_rounds_up_and_formats_numbers() {
        // 10 s in 3 s segments: 4 segments, the last one 1 s long.
        let text = mpd(r#"
<Period>
 <AdaptationSet contentType="audio" mimeType="audio/mp4">
  <Representation id="a" bandwidth="128000">
   <SegmentTemplate timescale="1" duration="3" startNumber="7" media="a/$Number%05d$.m4s"/>
  </Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        assert!(m.video.is_empty());
        let track = &m.audio[0].track;
        assert_eq!(track.kind, TrackKind::Audio);
        assert_eq!(track.container, Container::Mp4);
        assert_eq!(
            urls(track),
            [
                "https://cdn.example.com/video/a/00007.m4s",
                "https://cdn.example.com/video/a/00008.m4s",
                "https://cdn.example.com/video/a/00009.m4s",
                "https://cdn.example.com/video/a/00010.m4s",
            ]
        );
        let durations: Vec<u64> = track.segments.iter().map(|s| s.duration_ms).collect();
        assert_eq!(durations, [3000, 3000, 3000, 1000]);
        assert_eq!(track.duration_ms(), 10_000);
    }

    #[test]
    fn template_placeholders_and_escapes() {
        let text = mpd(r#"
<Period>
 <AdaptationSet mimeType="video/mp4">
  <Representation id="hd" bandwidth="2500000">
   <SegmentTemplate timescale="1" duration="10" media="$RepresentationID$/$Bandwidth$/$$raw/$Number$.m4s"/>
  </Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        assert_eq!(
            urls(&m.video[0].track),
            ["https://cdn.example.com/video/hd/2500000/$raw/1.m4s"]
        );

        let vars = Vars { id: "r", bandwidth: 42, number: Some(3), time: None };
        assert_eq!(
            expand("$RepresentationID$-$Bandwidth%04d$-$Number%03d$-$$", &vars).unwrap(),
            "r-0042-003-$"
        );
        assert!(expand("$Time$", &vars).is_err(), "no time without a timeline");
        assert!(expand("$Number", &vars).is_err(), "unterminated");
        assert!(expand("$Nope$", &vars).is_err(), "unknown variable");
        assert!(expand("$Number%x$", &vars).is_err(), "bad format");
    }

    #[test]
    fn representation_inherits_from_adaptation_set_and_merges_templates() {
        let text = mpd(r#"
<Period>
 <AdaptationSet mimeType="video/mp4" codecs="avc1.4d401f" width="640" height="360">
  <SegmentTemplate timescale="1000" duration="2000" initialization="init-$RepresentationID$.mp4" media="set-$Number$.m4s"/>
  <Representation id="hi" bandwidth="2000000" width="1920" height="1080" codecs="avc1.640028">
   <SegmentTemplate media="hi-$Number$.m4s"/>
  </Representation>
  <Representation id="lo" bandwidth="500000"/>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        let ids: Vec<&str> = m.video.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["lo", "hi"], "sorted by bandwidth");

        let lo = &m.video[0];
        assert_eq!(lo.variant.codecs.as_deref(), Some("avc1.4d401f"));
        assert_eq!((lo.variant.width, lo.variant.height), (Some(640), Some(360)));
        assert_eq!(
            lo.track.init.as_ref().unwrap().url.as_str(),
            "https://cdn.example.com/video/init-lo.mp4"
        );
        assert_eq!(lo.track.segments.len(), 5);
        assert_eq!(lo.track.segments[0].url.as_str(), "https://cdn.example.com/video/set-1.m4s");

        let hi = &m.video[1];
        assert_eq!(hi.variant.codecs.as_deref(), Some("avc1.640028"));
        assert_eq!((hi.variant.width, hi.variant.height), (Some(1920), Some(1080)));
        assert_eq!(
            hi.track.init.as_ref().unwrap().url.as_str(),
            "https://cdn.example.com/video/init-hi.mp4",
            "initialization comes from the AdaptationSet's template"
        );
        assert_eq!(
            urls(&hi.track),
            (1..=5)
                .map(|n| format!("https://cdn.example.com/video/hi-{n}.m4s"))
                .collect::<Vec<_>>()
        );
        assert!(hi.track.segments.iter().all(|s| s.duration_ms == 2000));
    }

    #[test]
    fn base_urls_resolve_level_by_level() {
        let text = mpd(r#"
<BaseURL>media/</BaseURL>
<Period>
 <BaseURL>p1/</BaseURL>
 <AdaptationSet mimeType="video/mp4">
  <BaseURL>../shared/</BaseURL>
  <Representation id="a" bandwidth="1000">
   <BaseURL>/abs/a/</BaseURL>
   <SegmentBase indexRange="0-99"/>
  </Representation>
  <Representation id="b" bandwidth="2000">
   <SegmentTemplate duration="5" media="b-$Number$.m4s"/>
  </Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        // Absolute path in the representation's BaseURL.
        assert_eq!(urls(&m.video[0].track), ["https://cdn.example.com/abs/a/"]);
        // MPD -> Period -> AdaptationSet: /video/media/p1/ then ../shared/.
        assert_eq!(
            urls(&m.video[1].track),
            [
                "https://cdn.example.com/video/media/shared/b-1.m4s",
                "https://cdn.example.com/video/media/shared/b-2.m4s",
            ]
        );
    }

    #[test]
    fn base_url_without_trailing_slash_is_replaced_by_the_next_relative_url() {
        // RFC 3986: "cdn" names a file, so "x/" replaces it. "cdn/" is a directory.
        let no_slash = mpd(r#"
<BaseURL>cdn</BaseURL>
<Period><AdaptationSet mimeType="video/mp4"><Representation id="r" bandwidth="1">
 <BaseURL>x/</BaseURL><SegmentBase/>
</Representation></AdaptationSet></Period>"#);
        let m =
            parse(&no_slash, &Url::parse("https://h.example/dir/manifest.mpd").unwrap()).unwrap();
        assert_eq!(urls(&m.video[0].track), ["https://h.example/dir/x/"]);

        let with_slash = mpd(r#"
<BaseURL>cdn/</BaseURL>
<Period><AdaptationSet mimeType="video/mp4"><Representation id="r" bandwidth="1">
 <BaseURL>x</BaseURL><SegmentBase/>
</Representation></AdaptationSet></Period>"#);
        let m =
            parse(&with_slash, &Url::parse("https://h.example/dir/manifest.mpd").unwrap()).unwrap();
        assert_eq!(urls(&m.video[0].track), ["https://h.example/dir/cdn/x"]);
    }

    #[test]
    fn segment_list_with_byte_ranges() {
        let text = mpd(r#"
<Period>
 <AdaptationSet mimeType="video/mp4">
  <Representation id="l" bandwidth="800000">
   <BaseURL>files/</BaseURL>
   <SegmentList timescale="1000" duration="2000">
    <Initialization sourceURL="init.mp4" range="0-999"/>
    <SegmentURL media="seg1.m4s" mediaRange="1000-1999"/>
    <SegmentURL media="seg2.m4s" mediaRange="2000-2499"/>
   </SegmentList>
  </Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        let track = &m.video[0].track;
        let init = track.init.as_ref().unwrap();
        assert_eq!(init.url.as_str(), "https://cdn.example.com/video/files/init.mp4");
        assert_eq!(init.range, Some((0, 1000)));
        assert_eq!(track.segments.len(), 2);
        assert_eq!(track.segments[0].url.as_str(), "https://cdn.example.com/video/files/seg1.m4s");
        assert_eq!(track.segments[0].range, Some((1000, 1000)));
        assert_eq!(track.segments[1].range, Some((2000, 500)));
        assert_eq!(track.duration_ms(), 4000);
    }

    #[test]
    fn segment_base_is_one_whole_file() {
        let text = mpd(r#"
<Period>
 <AdaptationSet mimeType="video/mp4">
  <Representation id="s" bandwidth="300000">
   <BaseURL>movie.mp4</BaseURL>
   <SegmentBase indexRange="800-1200"><Initialization range="0-799"/></SegmentBase>
  </Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        let track = &m.video[0].track;
        assert!(track.init.is_none());
        assert_eq!(track.segments.len(), 1);
        assert_eq!(track.segments[0].url.as_str(), "https://cdn.example.com/video/movie.mp4");
        assert_eq!(track.segments[0].range, None);
        assert_eq!(track.segments[0].duration_ms, 10_000);
    }

    #[test]
    fn text_and_image_adaptation_sets_are_skipped() {
        let text = mpd(r#"
<Period>
 <AdaptationSet contentType="text" mimeType="application/mp4">
  <Representation id="sub" bandwidth="1000" codecs="wvtt"><BaseURL>sub.mp4</BaseURL></Representation>
 </AdaptationSet>
 <AdaptationSet contentType="image" mimeType="image/jpeg">
  <Representation id="thumbs" bandwidth="2000"><SegmentTemplate media="t-$Number$.jpg" duration="1"/></Representation>
 </AdaptationSet>
 <AdaptationSet mimeType="video/mp4">
  <Representation id="v" bandwidth="5000"><BaseURL>v.mp4</BaseURL></Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        assert_eq!(m.video.len(), 1);
        assert_eq!(m.video[0].id, "v");
        assert!(m.audio.is_empty());
    }

    #[test]
    fn webm_and_ts_containers() {
        let text = mpd(r#"
<Period>
 <AdaptationSet contentType="video">
  <Representation id="w" mimeType="video/webm" bandwidth="1000"><BaseURL>w.webm</BaseURL></Representation>
  <Representation id="t" mimeType="video/mp2t" bandwidth="2000"><BaseURL>t.ts</BaseURL></Representation>
 </AdaptationSet>
 <AdaptationSet contentType="audio">
  <Representation id="o" mimeType="audio/webm" bandwidth="3000"><BaseURL>o.webm</BaseURL></Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        assert_eq!(m.video[0].track.container, Container::WebM);
        assert_eq!(m.video[1].track.container, Container::Ts);
        assert_eq!(m.audio[0].track.container, Container::WebM);
    }

    #[test]
    fn dynamic_manifests_are_rejected() {
        let text =
            r#"<MPD type="dynamic" availabilityStartTime="2024-01-01T00:00:00Z"><Period/></MPD>"#;
        let err = parse(text, &base()).unwrap_err();
        assert_eq!(err.0, "live DASH streams are not supported");
    }

    #[test]
    fn drm_is_rejected_wherever_it_is_declared() {
        let on_set = mpd(r#"
<Period><AdaptationSet mimeType="video/mp4">
 <ContentProtection schemeIdUri="urn:mpeg:dash:mp4protection:2011" value="cenc"/>
 <Representation id="v" bandwidth="1"><BaseURL>v.mp4</BaseURL></Representation>
</AdaptationSet></Period>"#);
        assert_eq!(
            parse(&on_set, &base()).unwrap_err().0,
            "DRM-protected streams are not supported"
        );

        let on_rep = mpd(r#"
<Period><AdaptationSet mimeType="video/mp4">
 <Representation id="v" bandwidth="1"><ContentProtection schemeIdUri="urn:uuid:x"/><BaseURL>v.mp4</BaseURL></Representation>
</AdaptationSet></Period>"#);
        assert_eq!(
            parse(&on_rep, &base()).unwrap_err().0,
            "DRM-protected streams are not supported"
        );
    }

    #[test]
    fn missing_bandwidth_is_an_error() {
        let text = mpd(r#"
<Period><AdaptationSet mimeType="video/mp4">
 <Representation id="nobw"><BaseURL>v.mp4</BaseURL></Representation>
</AdaptationSet></Period>"#);
        let err = parse(&text, &base()).unwrap_err();
        assert!(err.0.contains("no bandwidth"), "{err}");
    }

    #[test]
    fn representation_without_segments_or_base_is_an_error() {
        let text = mpd(r#"
<Period><AdaptationSet mimeType="video/mp4">
 <Representation id="bare" bandwidth="1"/>
</AdaptationSet></Period>"#);
        let err = parse(&text, &base()).unwrap_err();
        assert!(err.0.contains("no segment information"), "{err}");
    }

    #[test]
    fn best_audio_is_the_highest_bandwidth() {
        let text = mpd(r#"
<Period>
 <AdaptationSet contentType="audio" mimeType="audio/mp4">
  <Representation id="a64" bandwidth="64000"><BaseURL>a64.m4a</BaseURL></Representation>
  <Representation id="a128" bandwidth="128000"><BaseURL>a128.m4a</BaseURL></Representation>
  <Representation id="a96" bandwidth="96000"><BaseURL>a96.m4a</BaseURL></Representation>
 </AdaptationSet>
</Period>"#);
        let m = parse(&text, &base()).unwrap();
        assert_eq!(best_audio(&m).map(|r| r.id.as_str()), Some("a128"));

        let no_audio = parse(&mpd("<Period/>"), &base()).unwrap();
        assert!(best_audio(&no_audio).is_none());
    }

    #[test]
    fn only_the_first_period_is_used() {
        let text = mpd_for(
            "PT60S",
            r#"
<Period id="1" start="PT0S">
 <AdaptationSet mimeType="video/mp4">
  <Representation id="p1" bandwidth="1000">
   <SegmentTemplate timescale="1" duration="10" media="p1-$Number$.m4s"/>
  </Representation>
 </AdaptationSet>
</Period>
<Period id="2" start="PT30S">
 <AdaptationSet mimeType="video/mp4">
  <Representation id="p2" bandwidth="2000">
   <SegmentTemplate timescale="1" duration="10" media="p2-$Number$.m4s"/>
  </Representation>
 </AdaptationSet>
</Period>"#,
        );
        let m = parse(&text, &base()).unwrap();
        assert_eq!(m.duration_ms, Some(60_000));
        assert_eq!(m.video.len(), 1);
        assert_eq!(m.video[0].id, "p1");
        // The first period lasts until the second starts: 30 s, so 3 segments.
        assert_eq!(m.video[0].track.segments.len(), 3);
        assert_eq!(m.video[0].track.duration_ms(), 30_000);
    }

    #[test]
    fn duration_template_needs_a_known_duration() {
        let text = r#"<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static"><Period><AdaptationSet mimeType="video/mp4"><Representation id="v" bandwidth="1"><SegmentTemplate timescale="1" duration="2" media="s$Number$.m4s"/></Representation></AdaptationSet></Period></MPD>"#;
        let err = parse(text, &base()).unwrap_err();
        assert!(err.0.contains("duration"), "{err}");
    }

    #[test]
    fn runaway_repeat_counts_are_refused() {
        let text = mpd(r#"
<Period><AdaptationSet mimeType="video/mp4">
 <Representation id="v" bandwidth="1"><SegmentTemplate media="s-$Number$.m4s">
  <SegmentTimeline><S d="1" r="5000000"/></SegmentTimeline>
 </SegmentTemplate></Representation>
</AdaptationSet></Period>"#);
        let err = parse(&text, &base()).unwrap_err();
        assert!(err.0.starts_with("too many segments"), "{err}");
    }

    #[test]
    fn malformed_xml_and_other_roots_are_errors() {
        assert!(parse("<MPD type=\"static\"><Period>", &base()).is_err());
        assert!(parse("<svg/>", &base()).is_err());
        assert!(parse("", &base()).is_err());
    }

    #[test]
    fn parse_range_is_inclusive() {
        assert_eq!(parse_range("0-999"), Ok((0, 1000)));
        assert_eq!(parse_range("1000-1999"), Ok((1000, 1000)));
        assert!(parse_range("5-4").is_err());
        assert!(parse_range("5").is_err());
    }
}
