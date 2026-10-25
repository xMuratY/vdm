//! Schema migrations, tracked with `PRAGMA user_version`.

use rusqlite::{Connection, TransactionBehavior};

use crate::{Error, Result};

/// One SQL batch per schema version. Batch `i` upgrades version `i` to `i + 1`.
/// Only ever append here; editing a shipped batch changes what old databases mean.
const MIGRATIONS: &[&str] = &[
    r#"
    CREATE TABLE downloads (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        url           TEXT NOT NULL,
        save_dir      TEXT NOT NULL,
        file_name     TEXT NOT NULL,
        size          INTEGER,
        etag          TEXT,
        last_modified TEXT,
        state         TEXT NOT NULL,
        error         TEXT,
        connections   INTEGER NOT NULL,
        created_at    INTEGER NOT NULL,
        updated_at    INTEGER NOT NULL
    );
    CREATE INDEX downloads_url_save_dir ON downloads (url, save_dir);

    CREATE TABLE segments (
        download_id INTEGER NOT NULL REFERENCES downloads (id) ON DELETE CASCADE,
        start       INTEGER NOT NULL,
        "end"       INTEGER NOT NULL,
        pos         INTEGER NOT NULL,
        PRIMARY KEY (download_id, start),
        CHECK (start < "end" AND pos >= start AND pos <= "end")
    );
"#,
    r#"
    ALTER TABLE downloads ADD COLUMN headers TEXT NOT NULL DEFAULT '[]';
    ALTER TABLE downloads ADD COLUMN speed_limit INTEGER;
"#,
    r#"
    ALTER TABLE downloads ADD COLUMN checksum TEXT;
"#,
    // Named queues. Queue 1 is the main queue and always exists. (A column
    // added with ALTER TABLE can't carry a REFERENCES clause with a non-NULL
    // default, so the store keeps downloads.queue_id valid itself.)
    r#"
    CREATE TABLE queues (
        id          INTEGER PRIMARY KEY AUTOINCREMENT,
        name        TEXT NOT NULL,
        max_active  INTEGER,
        speed_limit INTEGER,
        schedule    TEXT,
        when_done   TEXT NOT NULL DEFAULT 'nothing',
        stopped     INTEGER NOT NULL DEFAULT 0,
        created_at  INTEGER NOT NULL
    );
    INSERT INTO queues (id, name, created_at) VALUES (1, 'Main', 0);
    ALTER TABLE downloads ADD COLUMN queue_id INTEGER NOT NULL DEFAULT 1;
"#,
];

/// Bring the schema up to date. Safe to call on every open.
pub(crate) fn run(conn: &mut Connection) -> Result<()> {
    // Fast path: nothing to do, so no write lock is taken.
    if user_version(conn)? == MIGRATIONS.len() {
        return Ok(());
    }

    loop {
        // Re-read the version under the write lock, so two processes cannot apply the same batch.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version = user_version(&tx)?;
        if version > MIGRATIONS.len() {
            return Err(Error::Corrupt("database is from a newer version".into()));
        }
        let Some(sql) = MIGRATIONS.get(version) else {
            return Ok(());
        };
        tx.execute_batch(sql)?;
        // The list is tiny, so the cast cannot truncate.
        tx.pragma_update(None, "user_version", (version + 1) as i64)?;
        tx.commit()?;
    }
}

fn user_version(conn: &Connection) -> Result<usize> {
    let v: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    usize::try_from(v).map_err(|_| Error::Corrupt(format!("invalid user_version {v}")))
}
