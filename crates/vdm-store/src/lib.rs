//! SQLite persistence for VDM downloads.
//!
//! [`Store`] owns one `rusqlite` connection. It keeps one row per download in
//! `downloads` and that download's segment positions in `segments`. Files are
//! opened in WAL mode with foreign keys on, and the schema is brought up to
//! date on open by the versioned migrations in `migrate.rs`.
//!
//! The API is synchronous. Async callers should run it on a blocking thread.

mod migrate;

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use vdm_proto::{DownloadId, DownloadState, QueueId, QueueSpec, SegmentProgress, WhenDone};

/// How long a statement waits for another connection's lock before failing.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Columns read by [`decode_download`], in no particular order.
const SELECT_DOWNLOADS: &str = "SELECT id, url, save_dir, file_name, size, etag, last_modified, \
     state, error, connections, headers, speed_limit, checksum, queue_id, created_at, updated_at \
     FROM downloads";

const SELECT_QUEUES: &str =
    "SELECT id, name, max_active, speed_limit, schedule, when_done, stopped FROM queues";

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// Stored data that does not fit the schema or the Rust types.
    #[error("corrupt database: {0}")]
    Corrupt(String),
    #[error("download {0} not found")]
    NotFound(DownloadId),
    #[error("queue {0} not found")]
    QueueNotFound(u64),
    #[error("the main queue can't be deleted")]
    MainQueue,
}

/// Fields needed to create a download. The store fills in state and timestamps.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRecord {
    pub url: String,
    pub save_dir: String,
    pub file_name: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub connections: u8,
    /// Extra request headers (cookies, referrer, ...), sent again on every resume.
    pub headers: Vec<(String, String)>,
    /// Per-download speed limit in bytes per second.
    pub speed_limit: Option<u64>,
    /// Expected digest ("sha256:…"), verified after downloading.
    pub checksum: Option<String>,
    pub queue: QueueId,
}

/// A stored queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueRecord {
    pub id: QueueId,
    pub spec: QueueSpec,
    /// Stopped by the user.
    pub stopped: bool,
}

/// A stored download.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadRecord {
    pub id: DownloadId,
    pub url: String,
    pub save_dir: String,
    pub file_name: String,
    pub size: Option<u64>,
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub state: DownloadState,
    pub error: Option<String>,
    pub connections: u8,
    pub headers: Vec<(String, String)>,
    pub speed_limit: Option<u64>,
    pub checksum: Option<String>,
    pub queue: QueueId,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds.
    pub updated_at: i64,
}

/// Handle to the VDM database.
#[derive(Debug)]
pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating if needed) the database file, enable WAL, foreign keys and a 5 s busy timeout, and run migrations.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")?;
        Self::with_migrations(conn)
    }

    /// Open a private in-memory database with foreign keys on and migrations applied.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        Self::with_migrations(conn)
    }

    fn with_migrations(mut conn: Connection) -> Result<Self> {
        migrate::run(&mut conn)?;
        Ok(Self { conn })
    }

    /// New downloads start in `DownloadState::Queued`.
    pub fn insert_download(&mut self, new: &NewRecord) -> Result<DownloadId> {
        let now = now_secs();
        self.conn.execute(
            "INSERT INTO downloads (url, save_dir, file_name, size, etag, last_modified, \
                 state, error, connections, headers, speed_limit, checksum, queue_id, \
                 created_at, updated_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10, ?11, ?13, ?12, ?12)",
            params![
                new.url,
                new.save_dir,
                new.file_name,
                opt_to_db(new.size, "size")?,
                new.etag,
                new.last_modified,
                state_text(DownloadState::Queued),
                i64::from(new.connections),
                headers_to_db(&new.headers)?,
                opt_to_db(new.speed_limit, "speed limit")?,
                new.checksum,
                now,
                queue_key(self.existing_queue(new.queue)?)?,
            ],
        )?;
        let raw = self.conn.last_insert_rowid();
        Ok(DownloadId(from_db_u64(raw, "id")?))
    }

    pub fn get(&self, id: DownloadId) -> Result<Option<DownloadRecord>> {
        // An id too large for SQLite cannot exist, so it is simply absent.
        let Ok(key) = db_key(id) else {
            return Ok(None);
        };
        let sql = format!("{SELECT_DOWNLOADS} WHERE id = ?1");
        self.conn.query_row(&sql, [key], |row| Ok(decode_download(row))).optional()?.transpose()
    }

    /// All downloads ordered by id.
    pub fn list(&self) -> Result<Vec<DownloadRecord>> {
        let sql = format!("{SELECT_DOWNLOADS} ORDER BY id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| Ok(decode_download(row)))?;
        collect_rows(rows)
    }

    /// Most recent (highest id) download with this url and save_dir whose state is not Completed.
    pub fn find_unfinished(&self, url: &str, save_dir: &str) -> Result<Option<DownloadRecord>> {
        let sql = format!(
            "{SELECT_DOWNLOADS} WHERE url = ?1 AND save_dir = ?2 AND state <> ?3 \
             ORDER BY id DESC LIMIT 1"
        );
        let completed = state_text(DownloadState::Completed);
        self.conn
            .query_row(&sql, params![url, save_dir, completed], |row| Ok(decode_download(row)))
            .optional()?
            .transpose()
    }

    pub fn update_state(
        &mut self,
        id: DownloadId,
        state: DownloadState,
        error: Option<&str>,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE downloads SET state = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
            params![state_text(state), error, now_secs(), db_key(id)?],
        )?;
        check_found(changed, id)
    }

    /// Record what the server reported (after a probe, or when the file changed).
    pub fn update_remote(
        &mut self,
        id: DownloadId,
        size: Option<u64>,
        etag: Option<&str>,
        last_modified: Option<&str>,
    ) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE downloads SET size = ?1, etag = ?2, last_modified = ?3, updated_at = ?4 \
             WHERE id = ?5",
            params![opt_to_db(size, "size")?, etag, last_modified, now_secs(), db_key(id)?],
        )?;
        check_found(changed, id)
    }

    pub fn set_file_name(&mut self, id: DownloadId, file_name: &str) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE downloads SET file_name = ?1, updated_at = ?2 WHERE id = ?3",
            params![file_name, now_secs(), db_key(id)?],
        )?;
        check_found(changed, id)
    }

    pub fn set_url(&mut self, id: DownloadId, url: &str) -> Result<()> {
        self.set_column(id, "url", url)
    }

    pub fn set_connections(&mut self, id: DownloadId, connections: u8) -> Result<()> {
        self.set_column(id, "connections", i64::from(connections))
    }

    pub fn set_speed_limit(&mut self, id: DownloadId, bytes_per_sec: Option<u64>) -> Result<()> {
        self.set_column(id, "speed_limit", opt_to_db(bytes_per_sec, "speed limit")?)
    }

    /// `column` is always a fixed name from this file, never user input.
    fn set_column(
        &mut self,
        id: DownloadId,
        column: &'static str,
        value: impl rusqlite::ToSql,
    ) -> Result<()> {
        let sql = format!("UPDATE downloads SET {column} = ?1, updated_at = ?2 WHERE id = ?3");
        let changed = self.conn.execute(&sql, params![value, now_secs(), db_key(id)?])?;
        check_found(changed, id)
    }

    /// Replace all segments of a download in one transaction.
    /// On error (including a CHECK violation) the previous segments are kept.
    pub fn save_segments(&mut self, id: DownloadId, segs: &[SegmentProgress]) -> Result<()> {
        let key = db_key(id)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Also bumps updated_at, and detects a missing download before touching segments.
        let changed = tx.execute(
            "UPDATE downloads SET updated_at = ?1 WHERE id = ?2",
            params![now_secs(), key],
        )?;
        check_found(changed, id)?;

        tx.execute("DELETE FROM segments WHERE download_id = ?1", [key])?;
        {
            let mut insert = tx.prepare(
                r#"INSERT INTO segments (download_id, start, "end", pos) VALUES (?1, ?2, ?3, ?4)"#,
            )?;
            for seg in segs {
                insert.execute(params![
                    key,
                    to_db(seg.start, "segment start")?,
                    to_db(seg.end, "segment end")?,
                    to_db(seg.pos, "segment pos")?,
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Segments ordered by start. An unknown id yields an empty list.
    pub fn load_segments(&self, id: DownloadId) -> Result<Vec<SegmentProgress>> {
        let Ok(key) = db_key(id) else {
            return Ok(Vec::new());
        };
        let mut stmt = self.conn.prepare(
            r#"SELECT start, "end", pos FROM segments WHERE download_id = ?1 ORDER BY start"#,
        )?;
        let rows = stmt.query_map([key], |row| Ok(decode_segment(row)))?;
        collect_rows(rows)
    }

    /// Move a download to another (existing) queue.
    pub fn set_download_queue(&mut self, id: DownloadId, queue: QueueId) -> Result<()> {
        let queue = queue_key(self.existing_queue(queue)?)?;
        self.set_column(id, "queue_id", queue)
    }

    // ------------------------------------------------------------ queues

    /// All queues, the main queue first.
    pub fn list_queues(&self) -> Result<Vec<QueueRecord>> {
        let sql = format!("{SELECT_QUEUES} ORDER BY id");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| Ok(decode_queue(row)))?;
        collect_rows(rows)
    }

    pub fn get_queue(&self, id: QueueId) -> Result<Option<QueueRecord>> {
        let Ok(key) = queue_key(id) else { return Ok(None) };
        let sql = format!("{SELECT_QUEUES} WHERE id = ?1");
        self.conn.query_row(&sql, [key], |row| Ok(decode_queue(row))).optional()?.transpose()
    }

    pub fn insert_queue(&mut self, spec: &QueueSpec) -> Result<QueueId> {
        self.conn.execute(
            "INSERT INTO queues (name, max_active, speed_limit, schedule, when_done, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                spec.name,
                spec.max_active.map(i64::from),
                opt_to_db(spec.speed_limit, "speed limit")?,
                schedule_to_db(spec)?,
                when_done_text(spec.when_done),
                now_secs(),
            ],
        )?;
        Ok(QueueId(from_db_u64(self.conn.last_insert_rowid(), "queue id")?))
    }

    pub fn update_queue(&mut self, id: QueueId, spec: &QueueSpec) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE queues SET name = ?1, max_active = ?2, speed_limit = ?3, schedule = ?4, \
             when_done = ?5 WHERE id = ?6",
            params![
                spec.name,
                spec.max_active.map(i64::from),
                opt_to_db(spec.speed_limit, "speed limit")?,
                schedule_to_db(spec)?,
                when_done_text(spec.when_done),
                queue_key(id)?,
            ],
        )?;
        if changed == 0 { Err(Error::QueueNotFound(id.0)) } else { Ok(()) }
    }

    pub fn set_queue_stopped(&mut self, id: QueueId, stopped: bool) -> Result<()> {
        let changed = self.conn.execute(
            "UPDATE queues SET stopped = ?1 WHERE id = ?2",
            params![stopped, queue_key(id)?],
        )?;
        if changed == 0 { Err(Error::QueueNotFound(id.0)) } else { Ok(()) }
    }

    /// Delete a queue; its downloads move to the main queue.
    pub fn delete_queue(&mut self, id: QueueId) -> Result<()> {
        if id == QueueId::MAIN {
            return Err(Error::MainQueue);
        }
        let key = queue_key(id)?;
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute("UPDATE downloads SET queue_id = 1 WHERE queue_id = ?1", [key])?;
        let changed = tx.execute("DELETE FROM queues WHERE id = ?1", [key])?;
        if changed == 0 {
            return Err(Error::QueueNotFound(id.0));
        }
        tx.commit()?;
        Ok(())
    }

    fn existing_queue(&self, id: QueueId) -> Result<QueueId> {
        match self.get_queue(id)? {
            Some(_) => Ok(id),
            None => Err(Error::QueueNotFound(id.0)),
        }
    }

    /// Delete a download and (via ON DELETE CASCADE) its segments.
    pub fn delete(&mut self, id: DownloadId) -> Result<()> {
        let changed = self.conn.execute("DELETE FROM downloads WHERE id = ?1", [db_key(id)?])?;
        check_found(changed, id)
    }
}

/// Build a record from a `SELECT_DOWNLOADS` row. Conversion errors are
/// returned as `Corrupt`, so callers must unwrap the nested `Result`.
fn decode_download(row: &Row<'_>) -> Result<DownloadRecord> {
    Ok(DownloadRecord {
        id: DownloadId(from_db_u64(row.get("id")?, "id")?),
        url: row.get("url")?,
        save_dir: row.get("save_dir")?,
        file_name: row.get("file_name")?,
        size: from_db_opt(row.get("size")?, "size")?,
        etag: row.get("etag")?,
        last_modified: row.get("last_modified")?,
        state: state_from_text(&row.get::<_, String>("state")?)?,
        error: row.get("error")?,
        connections: connections_from_db(row.get("connections")?)?,
        headers: headers_from_db(&row.get::<_, String>("headers")?)?,
        speed_limit: from_db_opt(row.get("speed_limit")?, "speed limit")?,
        checksum: row.get("checksum")?,
        queue: QueueId(from_db_u64(row.get("queue_id")?, "queue id")?),
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn decode_queue(row: &Row<'_>) -> Result<QueueRecord> {
    let schedule: Option<String> = row.get("schedule")?;
    let schedule = schedule
        .map(|text| serde_json::from_str(&text))
        .transpose()
        .map_err(|e| Error::Corrupt(format!("queue schedule: {e}")))?;
    let max_active: Option<i64> = row.get("max_active")?;
    Ok(QueueRecord {
        id: QueueId(from_db_u64(row.get("id")?, "queue id")?),
        spec: QueueSpec {
            name: row.get("name")?,
            max_active: max_active
                .map(|n| u32::try_from(n).map_err(|_| Error::Corrupt(format!("max_active {n}"))))
                .transpose()?,
            speed_limit: from_db_opt(row.get("speed_limit")?, "speed limit")?,
            schedule,
            when_done: when_done_from_text(&row.get::<_, String>("when_done")?)?,
        },
        stopped: row.get("stopped")?,
    })
}

fn schedule_to_db(spec: &QueueSpec) -> Result<Option<String>> {
    spec.schedule
        .as_ref()
        .map(serde_json::to_string)
        .transpose()
        .map_err(|e| Error::Corrupt(format!("queue schedule: {e}")))
}

fn when_done_text(when: WhenDone) -> &'static str {
    match when {
        WhenDone::Nothing => "nothing",
        WhenDone::Exit => "exit",
        WhenDone::Sleep => "sleep",
        WhenDone::Hibernate => "hibernate",
        WhenDone::ShutDown => "shut_down",
    }
}

fn when_done_from_text(text: &str) -> Result<WhenDone> {
    Ok(match text {
        "nothing" => WhenDone::Nothing,
        "exit" => WhenDone::Exit,
        "sleep" => WhenDone::Sleep,
        "hibernate" => WhenDone::Hibernate,
        "shut_down" => WhenDone::ShutDown,
        other => return Err(Error::Corrupt(format!("unknown when_done `{other}`"))),
    })
}

fn queue_key(id: QueueId) -> Result<i64> {
    i64::try_from(id.0).map_err(|_| Error::QueueNotFound(id.0))
}

fn decode_segment(row: &Row<'_>) -> Result<SegmentProgress> {
    Ok(SegmentProgress {
        start: from_db_u64(row.get("start")?, "segment start")?,
        end: from_db_u64(row.get("end")?, "segment end")?,
        pos: from_db_u64(row.get("pos")?, "segment pos")?,
    })
}

/// Turn `query_map` output (row errors and conversion errors) into one `Result`.
fn collect_rows<T>(rows: impl Iterator<Item = rusqlite::Result<Result<T>>>) -> Result<Vec<T>> {
    rows.map(|row| row.map_err(Error::from).and_then(|v| v)).collect()
}

/// Text stored in `downloads.state`. Same names as the serde form in `vdm-proto`.
fn state_text(state: DownloadState) -> &'static str {
    match state {
        DownloadState::Queued => "queued",
        DownloadState::Probing => "probing",
        DownloadState::Downloading => "downloading",
        DownloadState::Paused => "paused",
        DownloadState::Verifying => "verifying",
        DownloadState::Completed => "completed",
        DownloadState::Failed => "failed",
    }
}

fn state_from_text(text: &str) -> Result<DownloadState> {
    Ok(match text {
        "queued" => DownloadState::Queued,
        "probing" => DownloadState::Probing,
        "downloading" => DownloadState::Downloading,
        "paused" => DownloadState::Paused,
        "verifying" => DownloadState::Verifying,
        "completed" => DownloadState::Completed,
        "failed" => DownloadState::Failed,
        other => return Err(Error::Corrupt(format!("unknown download state `{other}`"))),
    })
}

/// Row key for `id`. A value too large for SQLite cannot match any row.
fn db_key(id: DownloadId) -> Result<i64> {
    i64::try_from(id.0).map_err(|_| Error::NotFound(id))
}

/// Map "no rows changed" to `NotFound`.
fn check_found(changed: usize, id: DownloadId) -> Result<()> {
    if changed == 0 { Err(Error::NotFound(id)) } else { Ok(()) }
}

fn to_db(value: u64, what: &str) -> Result<i64> {
    i64::try_from(value)
        .map_err(|_| Error::Corrupt(format!("{what} {value} does not fit in an INTEGER")))
}

fn opt_to_db(value: Option<u64>, what: &str) -> Result<Option<i64>> {
    value.map(|v| to_db(v, what)).transpose()
}

fn from_db_u64(value: i64, what: &str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Corrupt(format!("{what} {value} is negative")))
}

fn from_db_opt(value: Option<i64>, what: &str) -> Result<Option<u64>> {
    value.map(|v| from_db_u64(v, what)).transpose()
}

fn connections_from_db(value: i64) -> Result<u8> {
    u8::try_from(value).map_err(|_| Error::Corrupt(format!("connections {value} out of range")))
}

fn headers_to_db(headers: &[(String, String)]) -> Result<String> {
    serde_json::to_string(headers).map_err(|e| Error::Corrupt(format!("headers: {e}")))
}

fn headers_from_db(text: &str) -> Result<Vec<(String, String)>> {
    serde_json::from_str(text).map_err(|e| Error::Corrupt(format!("headers: {e}")))
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| i64::try_from(d.as_secs()).ok())
        .unwrap_or(0)
}
