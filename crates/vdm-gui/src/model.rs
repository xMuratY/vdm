//! View model for the download list.
//!
//! Plain Rust with no iced dependency, so everything here can be unit-tested.
//! [`Downloads`] mirrors the engine's state: it is seeded from stored records
//! and updated by applying engine [`Event`]s in order.

use vdm_proto::{DownloadId, DownloadState, Event, QueueId};
use vdm_store::DownloadRecord;

/// One line of the download list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: DownloadId,
    /// Display name, never empty.
    pub file_name: String,
    pub url: String,
    pub save_dir: String,
    pub state: DownloadState,
    pub downloaded: u64,
    pub total: Option<u64>,
    pub bytes_per_sec: u64,
    pub connections: u8,
    /// Unknown (`None`) until the download has started.
    pub resumable: Option<bool>,
    pub error: Option<String>,
    pub queue: QueueId,
}

impl Row {
    /// A blank queued row for tests to fill in.
    #[cfg(test)]
    pub fn for_tests() -> Self {
        Self {
            id: DownloadId(1),
            file_name: "file.bin".into(),
            url: "https://example.com/file.bin".into(),
            save_dir: ".".into(),
            state: DownloadState::Queued,
            downloaded: 0,
            total: None,
            bytes_per_sec: 0,
            connections: 1,
            resumable: None,
            error: None,
            queue: QueueId::MAIN,
        }
    }

    /// Build a row from a stored record.
    pub fn from_record(rec: &DownloadRecord) -> Self {
        let downloaded = match rec.state {
            DownloadState::Completed | DownloadState::Seeding => rec.size.unwrap_or(0),
            _ => 0,
        };
        Self {
            id: rec.id,
            file_name: name_or_fallback(&rec.file_name, &rec.url),
            url: rec.url.clone(),
            save_dir: rec.save_dir.clone(),
            state: rec.state,
            downloaded,
            total: rec.size,
            bytes_per_sec: 0,
            connections: rec.connections,
            resumable: None,
            error: rec.error.clone(),
            queue: rec.queue,
        }
    }

    /// Fraction done in `0.0..=1.0`, or `None` when the total is unknown.
    pub fn progress(&self) -> Option<f32> {
        if matches!(self.state, DownloadState::Completed | DownloadState::Seeding) {
            return Some(1.0);
        }
        let total = self.total?;
        if total == 0 {
            return Some(0.0);
        }
        let fraction = self.downloaded as f64 / total as f64;
        Some(fraction.clamp(0.0, 1.0) as f32)
    }

    /// Seconds left, rounded up. Only while downloading at a known rate with a known total.
    pub fn eta_secs(&self) -> Option<u64> {
        if self.state != DownloadState::Downloading || self.bytes_per_sec == 0 {
            return None;
        }
        let total = self.total?;
        let remaining = total.saturating_sub(self.downloaded);
        Some(remaining.div_ceil(self.bytes_per_sec))
    }

    /// Queued or downloading.
    pub fn can_pause(&self) -> bool {
        // Pausing a seeding torrent stops the seeding.
        matches!(
            self.state,
            DownloadState::Queued | DownloadState::Downloading | DownloadState::Seeding
        )
    }

    /// Paused or failed.
    pub fn can_resume(&self) -> bool {
        matches!(self.state, DownloadState::Paused | DownloadState::Failed)
    }
}

/// The whole list, ordered by id, plus the selection.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Downloads {
    rows: Vec<Row>,
    /// Selected ids, in no particular order.
    selected: Vec<DownloadId>,
    /// Where a Shift+click range starts: the last row clicked without Shift.
    anchor: Option<DownloadId>,
}

impl Downloads {
    /// Seed the list from stored records. Rows are ordered by id.
    pub fn from_records(recs: &[DownloadRecord]) -> Self {
        let mut rows: Vec<Row> = recs.iter().map(Row::from_record).collect();
        rows.sort_by_key(|row| row.id);
        Self { rows, selected: Vec::new(), anchor: None }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn get(&self, id: DownloadId) -> Option<&Row> {
        self.position(id).ok().map(|i| &self.rows[i])
    }

    /// Apply one engine event. Events for unknown ids are ignored, except `Added`.
    pub fn apply(&mut self, event: &Event) {
        match event {
            Event::Moved { id, queue } => {
                if let Some(row) = self.row_mut(*id) {
                    row.queue = *queue;
                }
            }
            Event::QueueChanged { .. }
            | Event::QueueRemoved { .. }
            | Event::QueueFinished { .. } => {}
            Event::Added { id, file_name, url, save_dir, queue } => {
                let row = Row {
                    id: *id,
                    file_name: name_or_fallback(file_name, url),
                    url: url.clone(),
                    save_dir: save_dir.clone(),
                    state: DownloadState::Queued,
                    downloaded: 0,
                    total: None,
                    bytes_per_sec: 0,
                    connections: 0,
                    resumable: None,
                    error: None,
                    queue: *queue,
                };
                match self.position(*id) {
                    Ok(i) => self.rows[i] = row,
                    Err(i) => self.rows.insert(i, row),
                }
            }
            Event::StateChanged { id, state } => {
                if let Some(row) = self.row_mut(*id) {
                    row.state = *state;
                    if *state != DownloadState::Downloading {
                        row.bytes_per_sec = 0;
                    }
                    if matches!(state, DownloadState::Queued | DownloadState::Downloading) {
                        row.error = None;
                    }
                }
            }
            Event::Started { id, file_name, total, resumable, connections, resumed_from } => {
                if let Some(row) = self.row_mut(*id) {
                    row.file_name = name_or_fallback(file_name, &row.url);
                    row.total = *total;
                    row.resumable = Some(*resumable);
                    row.connections = *connections;
                    row.downloaded = *resumed_from;
                }
            }
            Event::Progress { id, downloaded, total, bytes_per_sec, .. } => {
                if let Some(row) = self.row_mut(*id) {
                    row.downloaded = *downloaded;
                    row.total = *total;
                    row.bytes_per_sec = *bytes_per_sec;
                }
            }
            Event::Failed { id, error } => {
                if let Some(row) = self.row_mut(*id) {
                    row.error = Some(error.clone());
                    row.state = DownloadState::Failed;
                    row.bytes_per_sec = 0;
                }
            }
            Event::Completed { id, path } => {
                if let Some(row) = self.row_mut(*id) {
                    row.state = DownloadState::Completed;
                    row.bytes_per_sec = 0;
                    let total = row.total.unwrap_or(row.downloaded);
                    row.total = Some(total);
                    row.downloaded = total;
                    if let Some(name) = path.rsplit(['/', '\\']).next().filter(|n| !n.is_empty()) {
                        row.file_name = name.to_owned();
                    }
                }
            }
            Event::Removed { id } => {
                if let Ok(i) = self.position(*id) {
                    self.rows.remove(i);
                }
                self.selected.retain(|s| s != id);
                if self.anchor == Some(*id) {
                    self.anchor = None;
                }
            }
        }
    }

    /// Select only this row (a plain click). An id that is not in the list,
    /// or `None`, clears the selection.
    pub fn select(&mut self, id: Option<DownloadId>) {
        let id = id.filter(|id| self.position(*id).is_ok());
        self.selected = id.into_iter().collect();
        self.anchor = id;
    }

    /// Add or remove one row (Ctrl+click).
    pub fn toggle(&mut self, id: DownloadId) {
        if self.position(id).is_err() {
            return;
        }
        if let Some(i) = self.selected.iter().position(|s| *s == id) {
            self.selected.remove(i);
        } else {
            self.selected.push(id);
        }
        self.anchor = Some(id);
    }

    /// Select the rows from the anchor to `id` (Shift+click), in `order`, the
    /// order the rows are shown in. Without an anchor in `order`, selects `id` only.
    pub fn select_range(&mut self, order: &[DownloadId], id: DownloadId) {
        let end = order.iter().position(|o| *o == id);
        let start = self.anchor.and_then(|a| order.iter().position(|o| *o == a));
        match (start, end) {
            (Some(a), Some(b)) => {
                let (lo, hi) = (a.min(b), a.max(b));
                self.selected = order[lo..=hi].to_vec();
            }
            _ => {
                let anchor = self.anchor;
                self.select(Some(id));
                self.anchor = anchor.or(Some(id));
            }
        }
    }

    /// Select exactly these rows (Ctrl+A on what's shown).
    pub fn select_all(&mut self, ids: &[DownloadId]) {
        self.selected = ids.iter().copied().filter(|id| self.position(*id).is_ok()).collect();
    }

    /// Drop selected rows that `keep` rejects (e.g. hidden by a filter).
    pub fn retain_selection(&mut self, keep: impl Fn(&Row) -> bool) {
        let rows = &self.rows;
        self.selected
            .retain(|id| rows.binary_search_by_key(id, |row| row.id).is_ok_and(|i| keep(&rows[i])));
    }

    pub fn is_selected(&self, id: DownloadId) -> bool {
        self.selected.contains(&id)
    }

    /// The selected rows, in list order.
    pub fn selection(&self) -> Vec<&Row> {
        self.rows.iter().filter(|row| self.selected.contains(&row.id)).collect()
    }

    /// The selected row when exactly one is selected.
    pub fn selected(&self) -> Option<&Row> {
        match self.selected.as_slice() {
            [id] => self.get(*id),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn selected_id(&self) -> Option<DownloadId> {
        self.selected().map(|row| row.id)
    }

    /// Seconds until every running download of known size finishes, at the
    /// current total speed. None when nothing measurable is running.
    pub fn total_eta_secs(&self) -> Option<u64> {
        let speed = self.total_speed();
        if speed == 0 {
            return None;
        }
        let remaining: Vec<u64> = self
            .rows
            .iter()
            .filter(|row| row.state == DownloadState::Downloading)
            .filter_map(|row| row.total.map(|t| t.saturating_sub(row.downloaded)))
            .collect();
        if remaining.is_empty() {
            return None;
        }
        Some(remaining.iter().sum::<u64>().div_ceil(speed))
    }

    /// Sum of `bytes_per_sec` over the rows that are downloading.
    pub fn total_speed(&self) -> u64 {
        self.rows
            .iter()
            .filter(|row| row.state == DownloadState::Downloading)
            .fold(0, |sum, row| sum.saturating_add(row.bytes_per_sec))
    }

    /// Downloads in `queue` that haven't finished (waiting, running or paused).
    pub fn unfinished_in(&self, queue: QueueId) -> usize {
        self.rows.iter().filter(|r| r.queue == queue && !r.state.is_terminal()).count()
    }

    pub fn count(&self, state: DownloadState) -> usize {
        self.rows.iter().filter(|row| row.state == state).count()
    }

    /// Index of the row with this id, or where it would be inserted.
    fn position(&self, id: DownloadId) -> Result<usize, usize> {
        self.rows.binary_search_by_key(&id, |row| row.id)
    }

    fn row_mut(&mut self, id: DownloadId) -> Option<&mut Row> {
        let i = self.position(id).ok()?;
        self.rows.get_mut(i)
    }
}

/// `512 B`, `1.5 KiB`, `10.0 MiB`, `2.3 GiB`.
pub fn fmt_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 { format!("{n} B") } else { format!("{v:.1} {}", UNITS[unit]) }
}

/// `fmt_bytes(n)` followed by `/s`, or an empty string for 0.
pub fn fmt_speed(bytes_per_sec: u64) -> String {
    if bytes_per_sec == 0 { String::new() } else { format!("{}/s", fmt_bytes(bytes_per_sec)) }
}

/// `45s`, `3m 05s`, `1h 02m`, `2d 03h`.
pub fn fmt_eta(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 3600;
    const DAY: u64 = 86_400;
    if secs < MINUTE {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m {:02}s", secs / MINUTE, secs % MINUTE)
    } else if secs < DAY {
        format!("{}h {:02}m", secs / HOUR, (secs % HOUR) / MINUTE)
    } else {
        format!("{}d {:02}h", secs / DAY, (secs % DAY) / HOUR)
    }
}

/// Short label for a state.
pub fn state_label(state: DownloadState) -> String {
    match state {
        DownloadState::Queued => crate::i18n::t!("state.queued"),
        DownloadState::Probing => crate::i18n::t!("state.probing"),
        DownloadState::Downloading => crate::i18n::t!("state.downloading"),
        DownloadState::Paused => crate::i18n::t!("state.paused"),
        DownloadState::Verifying => crate::i18n::t!("state.verifying"),
        DownloadState::Completed => crate::i18n::t!("state.completed"),
        DownloadState::Failed => crate::i18n::t!("state.failed"),
        DownloadState::Seeding => crate::i18n::t!("state.seeding"),
    }
}

/// Name to show for a URL before the server's name is known: the last non-empty
/// path segment (query and fragment stripped, `%XX` escapes decoded when they
/// form valid UTF-8), else the host, else the whole string.
pub fn display_name(url: &str) -> String {
    let without_query = url.split(['?', '#']).next().unwrap_or(url);
    let (has_scheme, rest) = match without_query.find("://") {
        Some(i) => (true, &without_query[i + 3..]),
        None => (false, without_query),
    };
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    // Drop `user:password@` so credentials never show up in the name.
    let host =
        if has_scheme { authority.rsplit('@').next().unwrap_or(authority) } else { authority };

    if let Some(segment) = path.split('/').rev().find(|s| !s.is_empty()) {
        return percent_decode(segment);
    }
    if host.is_empty() { url.to_owned() } else { host.to_owned() }
}

/// Decode `%XX` escapes. Returns the input unchanged if the decoded bytes are not valid UTF-8.
fn percent_decode(segment: &str) -> String {
    let bytes = segment.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(hi), Some(lo)) = (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
        {
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| segment.to_owned())
}

fn hex_digit(byte: u8) -> Option<u8> {
    char::from(byte).to_digit(16).and_then(|d| u8::try_from(d).ok())
}

/// `name` when it is not blank, else a name derived from the URL.
fn name_or_fallback(name: &str, url: &str) -> String {
    if !name.trim().is_empty() {
        return name.to_owned();
    }
    let derived = display_name(url);
    if derived.is_empty() { "download".to_owned() } else { derived }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: u64, name: &str, state: DownloadState, size: Option<u64>) -> DownloadRecord {
        DownloadRecord {
            id: DownloadId(id),
            url: format!("https://example.com/files/{id}.zip"),
            save_dir: "C:\\dl".to_owned(),
            file_name: name.to_owned(),
            size,
            etag: None,
            last_modified: None,
            state,
            error: None,
            connections: 4,
            headers: Vec::new(),
            speed_limit: None,
            checksum: None,
            queue: QueueId::MAIN,
            created_at: 0,
            updated_at: 0,
        }
    }

    fn added(id: u64, name: &str) -> Event {
        Event::Added {
            id: DownloadId(id),
            file_name: name.to_owned(),
            url: format!("https://example.com/{name}"),
            save_dir: "C:\\dl".to_owned(),
            queue: QueueId::MAIN,
        }
    }

    fn state(id: u64, state: DownloadState) -> Event {
        Event::StateChanged { id: DownloadId(id), state }
    }

    fn progress(id: u64, downloaded: u64, total: Option<u64>, bytes_per_sec: u64) -> Event {
        Event::Progress {
            id: DownloadId(id),
            downloaded,
            total,
            bytes_per_sec,
            segments: Vec::new(),
            connections: 0,
        }
    }

    fn assert_close(actual: Option<f32>, expected: f32) {
        let actual = actual.expect("expected Some progress");
        assert!((actual - expected).abs() < 1e-6, "got {actual}, expected {expected}");
    }

    fn ids(downloads: &Downloads) -> Vec<u64> {
        downloads.rows().iter().map(|row| row.id.0).collect()
    }

    #[test]
    fn from_record_completed_uses_size_as_downloaded() {
        let row = Row::from_record(&record(1, "a.zip", DownloadState::Completed, Some(500)));
        assert_eq!(row.downloaded, 500);
        assert_eq!(row.total, Some(500));
        assert_eq!(row.bytes_per_sec, 0);
        assert_eq!(row.resumable, None);
        assert_eq!(row.connections, 4);
        assert_eq!(row.file_name, "a.zip");
    }

    #[test]
    fn from_record_not_completed_has_no_downloaded_bytes() {
        let row = Row::from_record(&record(1, "a.zip", DownloadState::Paused, Some(500)));
        assert_eq!(row.downloaded, 0);
        assert_eq!(row.total, Some(500));
    }

    #[test]
    fn from_record_empty_name_uses_display_name() {
        let row = Row::from_record(&record(3, "", DownloadState::Queued, None));
        assert_eq!(row.file_name, "3.zip");
        assert_eq!(row.url, "https://example.com/files/3.zip");
    }

    #[test]
    fn from_records_orders_by_id() {
        let recs = [
            record(9, "i", DownloadState::Queued, None),
            record(2, "b", DownloadState::Queued, None),
        ];
        let d = Downloads::from_records(&recs);
        assert_eq!(ids(&d), [2, 9]);
        assert_eq!(d.selected_id(), None);
    }

    #[test]
    fn added_inserts_queued_row_keeping_id_order() {
        let mut d = Downloads::default();
        d.apply(&added(5, "five.zip"));
        d.apply(&added(2, "two.zip"));
        d.apply(&added(9, "nine.zip"));
        d.apply(&added(3, "three.zip"));
        assert_eq!(ids(&d), [2, 3, 5, 9]);

        let row = d.get(DownloadId(3)).unwrap();
        assert_eq!(row.state, DownloadState::Queued);
        assert_eq!(row.connections, 0);
        assert_eq!(row.total, None);
        assert_eq!(row.file_name, "three.zip");
        assert_eq!(row.save_dir, "C:\\dl");
    }

    #[test]
    fn added_with_existing_id_replaces_the_row() {
        let mut d = Downloads::default();
        d.apply(&added(1, "old.zip"));
        d.apply(&state(1, DownloadState::Paused));
        d.apply(&added(1, "new.zip"));
        assert_eq!(ids(&d), [1]);
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.file_name, "new.zip");
        assert_eq!(row.state, DownloadState::Queued);
    }

    #[test]
    fn events_for_unknown_ids_are_ignored() {
        let mut d = Downloads::from_records(&[record(1, "a.zip", DownloadState::Paused, Some(10))]);
        let before = d.clone();
        let events = [
            state(99, DownloadState::Downloading),
            Event::Started {
                id: DownloadId(99),
                file_name: "x".to_owned(),
                total: Some(1),
                resumable: true,
                connections: 2,
                resumed_from: 0,
            },
            progress(99, 1, Some(1), 1),
            Event::Failed { id: DownloadId(99), error: "boom".to_owned() },
            Event::Completed { id: DownloadId(99), path: "C:\\x.zip".to_owned() },
            Event::Removed { id: DownloadId(99) },
        ];
        for event in &events {
            d.apply(event);
        }
        assert_eq!(d, before);
    }

    #[test]
    fn state_changed_resets_speed_and_clears_error_when_active() {
        let mut d = Downloads::from_records(&[record(1, "a.zip", DownloadState::Failed, Some(10))]);
        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&progress(1, 2, Some(10), 500));
        assert_eq!(d.get(DownloadId(1)).unwrap().bytes_per_sec, 500);

        d.apply(&Event::Failed { id: DownloadId(1), error: "boom".to_owned() });
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.error.as_deref(), Some("boom"));
        assert_eq!(row.state, DownloadState::Failed);
        assert_eq!(row.bytes_per_sec, 0);

        d.apply(&state(1, DownloadState::Paused));
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.state, DownloadState::Paused);
        assert_eq!(row.error.as_deref(), Some("boom"), "pausing keeps the error");
    }

    #[test]
    fn started_sets_name_total_resumable_connections_and_resume_offset() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&Event::Started {
            id: DownloadId(1),
            file_name: "real.zip".to_owned(),
            total: Some(1000),
            resumable: true,
            connections: 8,
            resumed_from: 200,
        });
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.file_name, "real.zip");
        assert_eq!(row.total, Some(1000));
        assert_eq!(row.resumable, Some(true));
        assert_eq!(row.connections, 8);
        assert_eq!(row.downloaded, 200);
    }

    #[test]
    fn progress_sets_downloaded_total_and_speed() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&progress(1, 300, Some(1000), 120));
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.downloaded, 300);
        assert_eq!(row.total, Some(1000));
        assert_eq!(row.bytes_per_sec, 120);
    }

    #[test]
    fn failed_sets_error_state_and_zero_speed() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&progress(1, 10, Some(100), 50));
        d.apply(&Event::Failed { id: DownloadId(1), error: "timeout".to_owned() });
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.state, DownloadState::Failed);
        assert_eq!(row.error.as_deref(), Some("timeout"));
        assert_eq!(row.bytes_per_sec, 0);
        assert_eq!(row.downloaded, 10, "failure keeps the bytes already downloaded");
    }

    #[test]
    fn completed_sets_total_name_and_progress() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&progress(1, 400, Some(1000), 100));
        d.apply(&Event::Completed { id: DownloadId(1), path: "C:\\dl\\real.zip".to_owned() });
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.state, DownloadState::Completed);
        assert_eq!(row.bytes_per_sec, 0);
        assert_eq!(row.total, Some(1000));
        assert_eq!(row.downloaded, 1000);
        assert_eq!(row.file_name, "real.zip");
    }

    #[test]
    fn completed_with_unknown_total_uses_downloaded_and_splits_both_separators() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&progress(1, 500, None, 0));
        d.apply(&Event::Completed { id: DownloadId(1), path: "/home/me/dl/out.bin".to_owned() });
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.total, Some(500));
        assert_eq!(row.downloaded, 500);
        assert_eq!(row.file_name, "out.bin");
    }

    #[test]
    fn completed_with_empty_last_component_keeps_name() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&Event::Completed { id: DownloadId(1), path: "C:\\dl\\".to_owned() });
        assert_eq!(d.get(DownloadId(1)).unwrap().file_name, "a.zip");
    }

    #[test]
    fn removed_drops_row_and_clears_selection_when_selected() {
        let mut d = Downloads::from_records(&[
            record(1, "a.zip", DownloadState::Paused, None),
            record(2, "b.zip", DownloadState::Paused, None),
        ]);
        d.select(Some(DownloadId(2)));
        d.apply(&Event::Removed { id: DownloadId(2) });
        assert_eq!(ids(&d), [1]);
        assert_eq!(d.selected_id(), None);
        assert!(d.selected().is_none());
    }

    #[test]
    fn moved_rows_change_queue() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        assert_eq!(d.get(DownloadId(1)).unwrap().queue, QueueId::MAIN);
        assert_eq!(d.unfinished_in(QueueId::MAIN), 1);
        d.apply(&Event::Moved { id: DownloadId(1), queue: QueueId(2) });
        assert_eq!(d.get(DownloadId(1)).unwrap().queue, QueueId(2));
        assert_eq!(d.unfinished_in(QueueId::MAIN), 0);
        assert_eq!(d.unfinished_in(QueueId(2)), 1);
    }

    #[test]
    fn removed_keeps_selection_of_other_rows() {
        let mut d = Downloads::from_records(&[
            record(1, "a.zip", DownloadState::Paused, None),
            record(2, "b.zip", DownloadState::Paused, None),
        ]);
        d.select(Some(DownloadId(1)));
        d.apply(&Event::Removed { id: DownloadId(2) });
        assert_eq!(d.selected_id(), Some(DownloadId(1)));
    }

    fn three() -> Downloads {
        Downloads::from_records(&[
            record(1, "a.zip", DownloadState::Paused, None),
            record(2, "b.zip", DownloadState::Paused, None),
            record(3, "c.zip", DownloadState::Paused, None),
        ])
    }

    fn picked(d: &Downloads) -> Vec<u64> {
        d.selection().iter().map(|r| r.id.0).collect()
    }

    #[test]
    fn ctrl_click_toggles_rows() {
        let mut d = three();
        d.select(Some(DownloadId(1)));
        d.toggle(DownloadId(3));
        assert_eq!(picked(&d), [1, 3]);
        assert_eq!(d.selected(), None, "two rows: no single selection");
        d.toggle(DownloadId(1));
        assert_eq!(picked(&d), [3]);
        assert_eq!(d.selected_id(), Some(DownloadId(3)));
        d.toggle(DownloadId(9));
        assert_eq!(picked(&d), [3]);
    }

    #[test]
    fn shift_click_selects_a_range_in_display_order() {
        let mut d = three();
        let order = [DownloadId(3), DownloadId(2), DownloadId(1)];
        d.select(Some(DownloadId(3)));
        d.select_range(&order, DownloadId(2));
        assert_eq!(picked(&d), [2, 3]);
        // The anchor stays put, so the range can shrink or flip.
        d.select_range(&order, DownloadId(1));
        assert_eq!(picked(&d), [1, 2, 3]);
        d.select_range(&order, DownloadId(3));
        assert_eq!(picked(&d), [3]);
    }

    #[test]
    fn select_all_and_filtering() {
        let mut d = three();
        d.select_all(&[DownloadId(1), DownloadId(2), DownloadId(9)]);
        assert_eq!(picked(&d), [1, 2]);
        d.retain_selection(|row| row.id != DownloadId(1));
        assert_eq!(picked(&d), [2]);
        d.apply(&Event::Removed { id: DownloadId(2) });
        assert!(d.selection().is_empty());
    }

    #[test]
    fn total_time_left_covers_every_running_download() {
        let mut d = Downloads::from_records(&[
            record(1, "a.zip", DownloadState::Queued, Some(1000)),
            record(2, "b.zip", DownloadState::Queued, Some(3000)),
            record(3, "c.zip", DownloadState::Queued, None),
        ]);
        assert_eq!(d.total_eta_secs(), None);
        for (id, done, speed) in [(1, 500, 100), (2, 1000, 150), (3, 10, 50)] {
            let row = d.row_mut(DownloadId(id)).unwrap();
            row.state = DownloadState::Downloading;
            row.downloaded = done;
            row.bytes_per_sec = speed;
        }
        // 500 + 2000 bytes left (c.zip has no size) at 300 B/s.
        assert_eq!(d.total_eta_secs(), Some(9));
    }

    #[test]
    fn select_ignores_unknown_ids() {
        let mut d = Downloads::from_records(&[record(1, "a.zip", DownloadState::Paused, None)]);
        d.select(Some(DownloadId(7)));
        assert_eq!(d.selected_id(), None);

        d.select(Some(DownloadId(1)));
        assert_eq!(d.selected_id(), Some(DownloadId(1)));
        assert_eq!(d.selected().unwrap().file_name, "a.zip");

        d.select(None);
        assert_eq!(d.selected_id(), None);
    }

    #[test]
    fn lifecycle_tracks_progress_and_eta() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        assert_eq!(d.get(DownloadId(1)).unwrap().progress(), None, "total unknown while queued");

        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&Event::Started {
            id: DownloadId(1),
            file_name: "real.zip".to_owned(),
            total: Some(1000),
            resumable: true,
            connections: 4,
            resumed_from: 200,
        });
        let row = d.get(DownloadId(1)).unwrap();
        assert_close(row.progress(), 0.2);
        assert_eq!(row.eta_secs(), None, "no speed yet");

        d.apply(&progress(1, 600, Some(1000), 100));
        let row = d.get(DownloadId(1)).unwrap();
        assert_close(row.progress(), 0.6);
        assert_eq!(row.eta_secs(), Some(4));

        d.apply(&progress(1, 950, Some(1000), 300));
        let row = d.get(DownloadId(1)).unwrap();
        assert_close(row.progress(), 0.95);
        assert_eq!(row.eta_secs(), Some(1), "50 bytes at 300 B/s rounds up to 1 s");

        d.apply(&state(1, DownloadState::Verifying));
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.bytes_per_sec, 0);
        assert_eq!(row.eta_secs(), None);

        d.apply(&Event::Completed { id: DownloadId(1), path: "C:\\dl\\real.zip".to_owned() });
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.state, DownloadState::Completed);
        assert_close(row.progress(), 1.0);
        assert_eq!(row.downloaded, 1000);
        assert_eq!(row.file_name, "real.zip");
        assert_eq!(row.eta_secs(), None);
    }

    #[test]
    fn failed_then_resume_clears_the_error() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&Event::Failed { id: DownloadId(1), error: "network down".to_owned() });
        let row = d.get(DownloadId(1)).unwrap();
        assert!(row.can_resume());
        assert_eq!(row.error.as_deref(), Some("network down"));

        d.apply(&state(1, DownloadState::Queued));
        let row = d.get(DownloadId(1)).unwrap();
        assert_eq!(row.error, None);
        assert_eq!(row.state, DownloadState::Queued);
    }

    #[test]
    fn total_speed_sums_only_downloading_rows() {
        let mut d = Downloads::from_records(&[
            record(1, "a", DownloadState::Queued, None),
            record(2, "b", DownloadState::Queued, None),
            record(3, "c", DownloadState::Queued, None),
        ]);
        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&progress(1, 0, Some(10), 100));
        d.apply(&state(2, DownloadState::Downloading));
        d.apply(&progress(2, 0, Some(10), 250));
        // Speed reported while queued does not count.
        d.apply(&progress(3, 0, Some(10), 999));

        assert_eq!(d.total_speed(), 350);
        assert_eq!(d.count(DownloadState::Downloading), 2);
        assert_eq!(d.count(DownloadState::Queued), 1);
        assert_eq!(d.count(DownloadState::Completed), 0);
    }

    #[test]
    fn progress_with_unknown_total_is_none() {
        let row = Row::from_record(&record(1, "a", DownloadState::Downloading, None));
        assert_eq!(row.progress(), None);
    }

    #[test]
    fn progress_with_zero_total() {
        let queued = Row::from_record(&record(1, "a", DownloadState::Queued, Some(0)));
        assert_eq!(queued.progress(), Some(0.0));
        let completed = Row::from_record(&record(1, "a", DownloadState::Completed, Some(0)));
        assert_eq!(completed.progress(), Some(1.0));
    }

    #[test]
    fn progress_is_clamped_to_one() {
        let row = Row::from_record(&record(1, "a", DownloadState::Downloading, Some(10)));
        let over = Row { downloaded: 25, ..row };
        assert_eq!(over.progress(), Some(1.0));
    }

    #[test]
    fn completed_with_unknown_total_reports_full_progress() {
        let mut d = Downloads::default();
        d.apply(&added(1, "a.zip"));
        d.apply(&progress(1, 500, None, 0));
        assert_eq!(d.get(DownloadId(1)).unwrap().progress(), None);
        d.apply(&Event::Completed { id: DownloadId(1), path: "C:\\dl\\a.zip".to_owned() });
        assert_close(d.get(DownloadId(1)).unwrap().progress(), 1.0);
    }

    #[test]
    fn eta_rounds_up() {
        let mut d = Downloads::from_records(&[record(1, "a", DownloadState::Queued, None)]);
        d.apply(&state(1, DownloadState::Downloading));
        d.apply(&progress(1, 0, Some(1001), 1000));
        assert_eq!(d.get(DownloadId(1)).unwrap().eta_secs(), Some(2));

        d.apply(&progress(1, 0, Some(1000), 1000));
        assert_eq!(d.get(DownloadId(1)).unwrap().eta_secs(), Some(1));

        d.apply(&progress(1, 2000, Some(1000), 1000));
        assert_eq!(d.get(DownloadId(1)).unwrap().eta_secs(), Some(0));
    }

    #[test]
    fn eta_requires_downloading_speed_and_total() {
        let row = Row {
            state: DownloadState::Paused,
            total: Some(100),
            bytes_per_sec: 10,
            ..Row::from_record(&record(1, "a", DownloadState::Paused, Some(100)))
        };
        assert_eq!(row.eta_secs(), None);

        let row = Row { state: DownloadState::Downloading, total: None, ..row };
        assert_eq!(row.eta_secs(), None);
    }

    #[test]
    fn can_pause_and_can_resume_for_every_state() {
        let cases = [
            (DownloadState::Queued, true, false),
            (DownloadState::Probing, false, false),
            (DownloadState::Downloading, true, false),
            (DownloadState::Paused, false, true),
            (DownloadState::Verifying, false, false),
            (DownloadState::Completed, false, false),
            (DownloadState::Failed, false, true),
        ];
        for (state, pause, resume) in cases {
            let row = Row::from_record(&record(1, "a", state, None));
            assert_eq!(row.can_pause(), pause, "can_pause for {state:?}");
            assert_eq!(row.can_resume(), resume, "can_resume for {state:?}");
        }
    }

    #[test]
    fn fmt_bytes_boundaries() {
        assert_eq!(fmt_bytes(0), "0 B");
        assert_eq!(fmt_bytes(512), "512 B");
        assert_eq!(fmt_bytes(1023), "1023 B");
        assert_eq!(fmt_bytes(1024), "1.0 KiB");
        assert_eq!(fmt_bytes(1536), "1.5 KiB");
        assert_eq!(fmt_bytes(10 * 1024 * 1024), "10.0 MiB");
        assert_eq!(fmt_bytes(2 * 1024 * 1024 * 1024), "2.0 GiB");
        assert_eq!(fmt_bytes(1 << 50), "1024.0 TiB");
    }

    #[test]
    fn fmt_speed_appends_per_second_or_is_empty() {
        assert_eq!(fmt_speed(0), "");
        assert_eq!(fmt_speed(512), "512 B/s");
        assert_eq!(fmt_speed(1536), "1.5 KiB/s");
    }

    #[test]
    fn fmt_eta_boundaries() {
        assert_eq!(fmt_eta(0), "0s");
        assert_eq!(fmt_eta(45), "45s");
        assert_eq!(fmt_eta(59), "59s");
        assert_eq!(fmt_eta(60), "1m 00s");
        assert_eq!(fmt_eta(185), "3m 05s");
        assert_eq!(fmt_eta(3599), "59m 59s");
        assert_eq!(fmt_eta(3600), "1h 00m");
        assert_eq!(fmt_eta(3720), "1h 02m");
        assert_eq!(fmt_eta(86_399), "23h 59m");
        assert_eq!(fmt_eta(86_400), "1d 00h");
        assert_eq!(fmt_eta(183_600), "2d 03h");
    }

    #[test]
    fn state_labels() {
        assert_eq!(state_label(DownloadState::Queued), "Queued");
        assert_eq!(state_label(DownloadState::Probing), "Probing");
        assert_eq!(state_label(DownloadState::Downloading), "Downloading");
        assert_eq!(state_label(DownloadState::Paused), "Paused");
        assert_eq!(state_label(DownloadState::Verifying), "Verifying");
        assert_eq!(state_label(DownloadState::Completed), "Completed");
        assert_eq!(state_label(DownloadState::Failed), "Failed");
    }

    #[test]
    fn display_name_from_url_path() {
        assert_eq!(display_name("https://x.com/a/b%20c.zip?x=1#y"), "b c.zip");
        assert_eq!(display_name("https://x.com/dir/file.zip"), "file.zip");
        assert_eq!(display_name("https://x.com/dir/"), "dir");
    }

    #[test]
    fn display_name_falls_back_to_host_then_whole_string() {
        assert_eq!(display_name("https://x.com/"), "x.com");
        assert_eq!(display_name("https://x.com"), "x.com");
        assert_eq!(display_name("https://user:secret@x.com/?q=1"), "x.com");
        assert_eq!(display_name("not a url"), "not a url");
        assert_eq!(display_name(""), "");
    }

    #[test]
    fn display_name_keeps_invalid_escapes() {
        assert_eq!(display_name("https://x.com/100%zz.zip"), "100%zz.zip");
        assert_eq!(display_name("https://x.com/a%2"), "a%2");
        assert_eq!(display_name("https://x.com/a%FFb.zip"), "a%FFb.zip");
        assert_eq!(display_name("https://x.com/%E4%BD%A0.txt"), "你.txt");
    }
}
