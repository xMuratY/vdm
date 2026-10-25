use std::path::Path;

use vdm_proto::{
    DownloadId, DownloadState, QueueId, QueueSpec, ScheduleSpec, SegmentProgress, WhenDone,
};
use vdm_store::{Error, NewRecord, Store};

fn new_record(url: &str, save_dir: &str) -> NewRecord {
    NewRecord {
        url: url.into(),
        save_dir: save_dir.into(),
        file_name: "file.zip".into(),
        size: None,
        etag: None,
        last_modified: None,
        connections: 4,
        headers: Vec::new(),
        speed_limit: None,
        checksum: None,
        queue: QueueId::MAIN,
    }
}

fn seg(start: u64, end: u64, pos: u64) -> SegmentProgress {
    SegmentProgress { start, end, pos }
}

/// Raw connection to the same file, for corrupting rows behind the store's back.
fn raw(path: &Path) -> rusqlite::Connection {
    rusqlite::Connection::open(path).unwrap()
}

#[test]
fn insert_then_get_round_trips_all_fields() {
    let mut store = Store::open_in_memory().unwrap();
    let new = NewRecord {
        url: "https://example.com/a.zip".into(),
        save_dir: "C:/Downloads".into(),
        file_name: "a.zip".into(),
        size: Some(1 << 40),
        etag: Some("\"abc\"".into()),
        last_modified: Some("Wed, 21 Oct 2015 07:28:00 GMT".into()),
        connections: 8,
        headers: vec![("Cookie".into(), "a=b".into())],
        speed_limit: Some(50_000),
        checksum: None,
        queue: QueueId::MAIN,
    };
    let id = store.insert_download(&new).unwrap();
    let rec = store.get(id).unwrap().expect("inserted row exists");

    assert_eq!(rec.id, id);
    assert_eq!(rec.url, new.url);
    assert_eq!(rec.save_dir, new.save_dir);
    assert_eq!(rec.file_name, new.file_name);
    assert_eq!(rec.size, new.size);
    assert_eq!(rec.etag, new.etag);
    assert_eq!(rec.last_modified, new.last_modified);
    assert_eq!(rec.connections, 8);
    assert_eq!(rec.state, DownloadState::Queued);
    assert_eq!(rec.error, None);
    assert_eq!(rec.created_at, rec.updated_at);
}

#[test]
fn list_is_ordered_by_id() {
    let mut store = Store::open_in_memory().unwrap();
    let ids: Vec<DownloadId> = (0..3)
        .map(|i| store.insert_download(&new_record(&format!("https://x/{i}"), "d")).unwrap())
        .collect();
    let listed: Vec<DownloadId> = store.list().unwrap().iter().map(|r| r.id).collect();
    assert_eq!(listed, ids);
    assert!(ids.windows(2).all(|w| w[0] < w[1]));
}

#[test]
fn get_missing_is_none() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.get(DownloadId(999)).unwrap(), None);
    assert_eq!(store.get(DownloadId(u64::MAX)).unwrap(), None);
}

#[test]
fn update_state_persists_every_state() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    let created = store.get(id).unwrap().unwrap().created_at;

    let states = [
        DownloadState::Queued,
        DownloadState::Probing,
        DownloadState::Downloading,
        DownloadState::Paused,
        DownloadState::Verifying,
        DownloadState::Completed,
        DownloadState::Failed,
    ];
    for state in states {
        store.update_state(id, state, None).unwrap();
        let rec = store.get(id).unwrap().unwrap();
        assert_eq!(rec.state, state);
        assert_eq!(rec.error, None);
        assert!(rec.updated_at >= created);
    }

    store.update_state(id, DownloadState::Failed, Some("boom")).unwrap();
    assert_eq!(store.get(id).unwrap().unwrap().error.as_deref(), Some("boom"));
    store.update_state(id, DownloadState::Paused, None).unwrap();
    assert_eq!(store.get(id).unwrap().unwrap().error, None);
}

#[test]
fn update_remote_persists() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    let before = store.get(id).unwrap().unwrap();

    store.update_remote(id, Some(10), Some("W/\"x\""), None).unwrap();
    let after = store.get(id).unwrap().unwrap();
    assert_eq!(after.size, Some(10));
    assert_eq!(after.etag.as_deref(), Some("W/\"x\""));
    assert_eq!(after.last_modified, None);
    assert!(after.updated_at >= before.created_at);
    assert_eq!(after.created_at, before.created_at);
}

#[test]
fn set_file_name_persists() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    store.set_file_name(id, "renamed.iso").unwrap();
    let rec = store.get(id).unwrap().unwrap();
    assert_eq!(rec.file_name, "renamed.iso");
    assert!(rec.updated_at >= rec.created_at);
}

#[test]
fn mutating_missing_id_is_not_found() {
    let mut store = Store::open_in_memory().unwrap();
    for missing in [DownloadId(42), DownloadId(u64::MAX)] {
        assert!(matches!(
            store.update_state(missing, DownloadState::Paused, None),
            Err(Error::NotFound(id)) if id == missing
        ));
        assert!(matches!(
            store.update_remote(missing, Some(1), None, None),
            Err(Error::NotFound(id)) if id == missing
        ));
        assert!(matches!(
            store.set_file_name(missing, "x"),
            Err(Error::NotFound(id)) if id == missing
        ));
        assert!(matches!(
            store.save_segments(missing, &[seg(0, 1, 0)]),
            Err(Error::NotFound(id)) if id == missing
        ));
        assert!(matches!(
            store.delete(missing),
            Err(Error::NotFound(id)) if id == missing
        ));
    }
}

#[test]
fn save_segments_replaces_previous_and_load_sorts_by_start() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();

    store.save_segments(id, &[seg(0, 50, 10), seg(50, 100, 60)]).unwrap();

    // Replacement: only the new segments remain. Input is out of order on purpose.
    store.save_segments(id, &[seg(70, 100, 80), seg(0, 70, 5)]).unwrap();
    assert_eq!(store.load_segments(id).unwrap(), vec![seg(0, 70, 5), seg(70, 100, 80)]);

    store.save_segments(id, &[seg(0, 100, 100)]).unwrap();
    assert_eq!(store.load_segments(id).unwrap(), vec![seg(0, 100, 100)]);
}

#[test]
fn save_segments_rejects_invalid_and_keeps_previous() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    let good = vec![seg(0, 50, 20), seg(50, 100, 50)];
    store.save_segments(id, &good).unwrap();

    // start >= end
    assert!(matches!(store.save_segments(id, &[seg(5, 5, 5)]), Err(Error::Sqlite(_))));
    // pos beyond end
    assert!(matches!(store.save_segments(id, &[seg(0, 10, 11)]), Err(Error::Sqlite(_))));
    // pos before start
    assert!(matches!(store.save_segments(id, &[seg(10, 20, 9)]), Err(Error::Sqlite(_))));
    // Duplicate start violates the primary key.
    assert!(matches!(
        store.save_segments(id, &[seg(0, 10, 0), seg(0, 20, 0)]),
        Err(Error::Sqlite(_))
    ));

    assert_eq!(store.load_segments(id).unwrap(), good);
}

#[test]
fn load_segments_for_unknown_id_is_empty() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.load_segments(DownloadId(7)).unwrap().is_empty());
}

#[test]
fn delete_cascades_to_segments() {
    let mut store = Store::open_in_memory().unwrap();
    let keep = store.insert_download(&new_record("https://x/keep", "d")).unwrap();
    let gone = store.insert_download(&new_record("https://x/gone", "d")).unwrap();
    store.save_segments(keep, &[seg(0, 10, 3)]).unwrap();
    store.save_segments(gone, &[seg(0, 10, 3)]).unwrap();

    store.delete(gone).unwrap();
    assert_eq!(store.get(gone).unwrap(), None);
    assert!(store.load_segments(gone).unwrap().is_empty());
    assert_eq!(store.load_segments(keep).unwrap(), vec![seg(0, 10, 3)]);
    assert!(matches!(store.delete(gone), Err(Error::NotFound(id)) if id == gone));
}

#[test]
fn find_unfinished_filters_and_prefers_highest_id() {
    let mut store = Store::open_in_memory().unwrap();
    let url = "https://x/a.zip";

    let done = store.insert_download(&new_record(url, "d")).unwrap();
    store.update_state(done, DownloadState::Completed, None).unwrap();
    store.insert_download(&new_record(url, "other")).unwrap();
    store.insert_download(&new_record("https://x/b.zip", "d")).unwrap();
    assert_eq!(store.find_unfinished(url, "d").unwrap(), None);

    let first = store.insert_download(&new_record(url, "d")).unwrap();
    assert_eq!(store.find_unfinished(url, "d").unwrap().unwrap().id, first);

    let second = store.insert_download(&new_record(url, "d")).unwrap();
    assert_eq!(store.find_unfinished(url, "d").unwrap().unwrap().id, second);

    store.update_state(second, DownloadState::Completed, None).unwrap();
    assert_eq!(store.find_unfinished(url, "d").unwrap().unwrap().id, first);
}

#[test]
fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vdm.db");

    let id = {
        let mut store = Store::open(&path).unwrap();
        let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
        store.save_segments(id, &[seg(0, 10, 4)]).unwrap();
        id
    };

    let store = Store::open(&path).unwrap();
    let rec = store.get(id).unwrap().expect("row persisted");
    assert_eq!(rec.url, "https://x/a");
    assert_eq!(store.list().unwrap().len(), 1);
    assert_eq!(store.load_segments(id).unwrap(), vec![seg(0, 10, 4)]);
}

#[test]
fn opening_twice_runs_migrations_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vdm.db");

    drop(Store::open(&path).unwrap());
    drop(Store::open(&path).unwrap());

    let conn = raw(&path);
    let version: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0)).unwrap();
    assert_eq!(version, 4);
    let mode: String = conn.pragma_query_value(None, "journal_mode", |r| r.get(0)).unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn newer_schema_version_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vdm.db");
    drop(Store::open(&path).unwrap());

    raw(&path).pragma_update(None, "user_version", 99).unwrap();
    match Store::open(&path) {
        Err(Error::Corrupt(msg)) => assert!(msg.contains("newer version"), "{msg}"),
        Err(other) => panic!("expected Corrupt, got {other}"),
        Ok(_) => panic!("expected Corrupt, got Ok"),
    }
}

#[test]
fn unknown_state_text_is_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vdm.db");
    let id = Store::open(&path).unwrap().insert_download(&new_record("https://x/a", "d")).unwrap();

    raw(&path)
        .execute("UPDATE downloads SET state = 'bogus' WHERE id = ?1", [id.0 as i64])
        .unwrap();

    let store = Store::open(&path).unwrap();
    assert!(matches!(store.get(id), Err(Error::Corrupt(_))));
    assert!(matches!(store.list(), Err(Error::Corrupt(_))));
}

#[test]
fn negative_or_oversized_stored_values_are_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("vdm.db");
    let id = Store::open(&path).unwrap().insert_download(&new_record("https://x/a", "d")).unwrap();

    let conn = raw(&path);
    conn.execute("UPDATE downloads SET size = -1 WHERE id = ?1", [id.0 as i64]).unwrap();
    let store = Store::open(&path).unwrap();
    assert!(matches!(store.get(id), Err(Error::Corrupt(_))));
    drop(store);

    conn.execute(
        "UPDATE downloads SET size = NULL, connections = 300 WHERE id = ?1",
        [id.0 as i64],
    )
    .unwrap();
    let store = Store::open(&path).unwrap();
    assert!(matches!(store.get(id), Err(Error::Corrupt(_))));
}

#[test]
fn u64_out_of_range_is_corrupt() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();

    assert!(matches!(store.update_remote(id, Some(u64::MAX), None, None), Err(Error::Corrupt(_))));
    assert_eq!(store.get(id).unwrap().unwrap().size, None);

    assert!(matches!(store.save_segments(id, &[seg(0, u64::MAX, 0)]), Err(Error::Corrupt(_))));

    let big = NewRecord { size: Some(u64::MAX), ..new_record("https://x/b", "d") };
    assert!(matches!(store.insert_download(&big), Err(Error::Corrupt(_))));
}

#[test]
fn setters_persist_url_connections_and_speed_limit() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    store.set_url(id, "https://y/a").unwrap();
    store.set_connections(id, 16).unwrap();
    store.set_speed_limit(id, Some(1234)).unwrap();
    let rec = store.get(id).unwrap().unwrap();
    assert_eq!(
        (rec.url.as_str(), rec.connections, rec.speed_limit),
        ("https://y/a", 16, Some(1234))
    );
    store.set_speed_limit(id, None).unwrap();
    assert_eq!(store.get(id).unwrap().unwrap().speed_limit, None);

    let missing = DownloadId(999);
    assert!(matches!(store.set_url(missing, "u"), Err(Error::NotFound(_))));
    assert!(matches!(store.set_connections(missing, 1), Err(Error::NotFound(_))));
    assert!(matches!(store.set_speed_limit(missing, None), Err(Error::NotFound(_))));
}

#[test]
fn version_one_databases_are_upgraded() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.db");
    // Build a v1 database by hand: schema batch 1 only, with one row.
    {
        let conn = raw(&path);
        conn.execute_batch(
            r#"CREATE TABLE downloads (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT NOT NULL,
                save_dir TEXT NOT NULL, file_name TEXT NOT NULL, size INTEGER, etag TEXT,
                last_modified TEXT, state TEXT NOT NULL, error TEXT, connections INTEGER NOT NULL,
                created_at INTEGER NOT NULL, updated_at INTEGER NOT NULL);
               CREATE TABLE segments (download_id INTEGER NOT NULL REFERENCES downloads (id)
                ON DELETE CASCADE, start INTEGER NOT NULL, "end" INTEGER NOT NULL,
                pos INTEGER NOT NULL, PRIMARY KEY (download_id, start));
               INSERT INTO downloads (url, save_dir, file_name, state, connections, created_at, updated_at)
                VALUES ('https://x/old', 'd', 'old.bin', 'paused', 8, 1, 1);
               PRAGMA user_version = 1;"#,
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let rec = &store.list().unwrap()[0];
    assert_eq!(rec.url, "https://x/old");
    assert!(rec.headers.is_empty());
    assert_eq!(rec.speed_limit, None);
}

fn queue_spec(name: &str) -> QueueSpec {
    QueueSpec {
        name: name.into(),
        max_active: Some(2),
        speed_limit: Some(1 << 20),
        schedule: Some(ScheduleSpec {
            start: Some("01:00".into()),
            stop: Some("07:00".into()),
            days: vec![5, 6],
        }),
        when_done: WhenDone::ShutDown,
    }
}

#[test]
fn the_main_queue_exists_and_cannot_be_deleted() {
    let mut store = Store::open_in_memory().unwrap();
    let queues = store.list_queues().unwrap();
    assert_eq!(queues.len(), 1);
    assert_eq!(queues[0].id, QueueId::MAIN);
    assert!(!queues[0].stopped);
    assert!(matches!(store.delete_queue(QueueId::MAIN), Err(Error::MainQueue)));
    // Downloads land in it by default.
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    assert_eq!(store.get(id).unwrap().unwrap().queue, QueueId::MAIN);
}

#[test]
fn queues_round_trip_update_and_stop() {
    let mut store = Store::open_in_memory().unwrap();
    let id = store.insert_queue(&queue_spec("Night")).unwrap();
    let rec = store.get_queue(id).unwrap().unwrap();
    assert_eq!(rec.spec, queue_spec("Night"));

    let changed =
        QueueSpec { name: "Weekend".into(), schedule: None, max_active: None, ..queue_spec("x") };
    store.update_queue(id, &changed).unwrap();
    store.set_queue_stopped(id, true).unwrap();
    let rec = store.get_queue(id).unwrap().unwrap();
    assert_eq!(rec.spec, changed);
    assert!(rec.stopped);

    assert!(matches!(store.update_queue(QueueId(99), &changed), Err(Error::QueueNotFound(99))));
    assert!(matches!(store.set_queue_stopped(QueueId(99), true), Err(Error::QueueNotFound(99))));
}

#[test]
fn deleting_a_queue_moves_its_downloads_to_main() {
    let mut store = Store::open_in_memory().unwrap();
    let night = store.insert_queue(&queue_spec("Night")).unwrap();
    let a = store
        .insert_download(&NewRecord { queue: night, ..new_record("https://x/a", "d") })
        .unwrap();
    let b = store.insert_download(&new_record("https://x/b", "d")).unwrap();
    store.set_download_queue(b, night).unwrap();
    assert_eq!(store.get(b).unwrap().unwrap().queue, night);

    store.delete_queue(night).unwrap();
    assert!(store.get_queue(night).unwrap().is_none());
    for id in [a, b] {
        assert_eq!(store.get(id).unwrap().unwrap().queue, QueueId::MAIN);
    }
}

#[test]
fn downloads_only_go_to_existing_queues() {
    let mut store = Store::open_in_memory().unwrap();
    let bad = NewRecord { queue: QueueId(42), ..new_record("https://x/a", "d") };
    assert!(matches!(store.insert_download(&bad), Err(Error::QueueNotFound(42))));
    let id = store.insert_download(&new_record("https://x/a", "d")).unwrap();
    assert!(matches!(store.set_download_queue(id, QueueId(42)), Err(Error::QueueNotFound(42))));
}
