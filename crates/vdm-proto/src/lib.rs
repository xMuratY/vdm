//! Types shared by the engine, the GUI, the CLI and the browser native host.
//!
//! Everything here is plain data with serde support, so it can cross the
//! named-pipe IPC boundary unchanged.

use serde::{Deserialize, Serialize};

/// Identifies one download for the lifetime of the database.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DownloadId(pub u64);

impl std::fmt::Display for DownloadId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Lifecycle of a download: `Queued → Probing → Downloading ⇄ Paused → Verifying → Completed | Failed`.
/// A download queue. [`QueueId::MAIN`] always exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct QueueId(pub u64);

impl QueueId {
    /// The queue downloads go to unless they name another one.
    pub const MAIN: QueueId = QueueId(1);
}

impl Default for QueueId {
    fn default() -> Self {
        Self::MAIN
    }
}

/// What to do once a queue has nothing left to download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenDone {
    #[default]
    Nothing,
    /// Quit the app.
    Exit,
    Sleep,
    Hibernate,
    ShutDown,
}

/// A daily window during which a queue may download. Times are local
/// "HH:MM"; a missing start means midnight, a missing stop the next midnight,
/// and a stop at or before the start runs past midnight. `days` are the days
/// a window starts on, 0 = Monday … 6 = Sunday; empty means every day.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ScheduleSpec {
    #[serde(default)]
    pub start: Option<String>,
    #[serde(default)]
    pub stop: Option<String>,
    #[serde(default)]
    pub days: Vec<u8>,
}

/// A queue's settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueSpec {
    pub name: String,
    /// Downloads this queue runs at once; `None` = only the global limit.
    #[serde(default)]
    pub max_active: Option<u32>,
    /// Shared by the queue's downloads, in bytes per second.
    #[serde(default)]
    pub speed_limit: Option<u64>,
    /// `None` = no time restriction.
    #[serde(default)]
    pub schedule: Option<ScheduleSpec>,
    #[serde(default)]
    pub when_done: WhenDone,
}

/// A queue as the engine reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Queue {
    pub id: QueueId,
    #[serde(flatten)]
    pub spec: QueueSpec,
    /// Stopped by the user: starts nothing until started again.
    pub stopped: bool,
    /// Inside its schedule window (always true without a schedule).
    pub in_window: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Queued,
    Probing,
    Downloading,
    Paused,
    Verifying,
    Completed,
    Failed,
}

impl DownloadState {
    /// True once the download will make no further progress on its own.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed)
    }
}

/// A request to add a download, as sent by the GUI, CLI or browser extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewDownload {
    pub url: String,
    /// Target folder; `None` means the category default.
    pub save_dir: Option<String>,
    /// Override for the file name; `None` means use the server's or the URL's.
    pub file_name: Option<String>,
    /// Number of parallel connections; `None` means the global default.
    pub connections: Option<u8>,
    /// Extra request context captured by the browser extension.
    #[serde(default)]
    pub referrer: Option<String>,
    #[serde(default)]
    pub user_agent: Option<String>,
    #[serde(default)]
    pub cookies: Option<String>,
    /// Expected digest, e.g. "sha256:…"; the file is verified after downloading.
    #[serde(default)]
    pub checksum: Option<String>,
    /// `None` = the main queue.
    #[serde(default)]
    pub queue: Option<QueueId>,
}

/// Commands accepted by the engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Command {
    Add(NewDownload),
    Pause {
        id: DownloadId,
    },
    Resume {
        id: DownloadId,
    },
    Remove {
        id: DownloadId,
        delete_file: bool,
    },
    /// Bytes per second; `None` removes the limit. `id: None` sets the global limit.
    SetSpeedLimit {
        id: Option<DownloadId>,
        bytes_per_sec: Option<u64>,
    },
    SetConnections {
        id: DownloadId,
        connections: u8,
    },
    /// Swap an expired URL for a fresh one ("Refresh address").
    RefreshUrl {
        id: DownloadId,
        url: String,
    },
    MoveToQueue {
        id: DownloadId,
        queue: QueueId,
    },
}

/// Progress of one segment, used for the segment bar in the progress window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentProgress {
    pub start: u64,
    /// Exclusive end.
    pub end: u64,
    /// Next byte to be written.
    pub pos: u64,
}

/// Events published by the engine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Added {
        id: DownloadId,
        /// Display name: the user's choice, or one derived from the URL until the
        /// server's name is known (see `Started`).
        file_name: String,
        url: String,
        save_dir: String,
        #[serde(default)]
        queue: QueueId,
    },
    /// A download moved to another queue.
    Moved {
        id: DownloadId,
        queue: QueueId,
    },
    /// A queue was created or changed (settings, stopped, schedule window).
    QueueChanged {
        queue: Queue,
    },
    QueueRemoved {
        id: QueueId,
    },
    /// A queue ran out of downloads after running some; its `when_done` applies.
    QueueFinished {
        id: QueueId,
        when_done: WhenDone,
    },
    StateChanged {
        id: DownloadId,
        state: DownloadState,
    },
    /// Data transfer is about to begin: the server was probed and a file name chosen.
    Started {
        id: DownloadId,
        file_name: String,
        total: Option<u64>,
        resumable: bool,
        connections: u8,
        resumed_from: u64,
    },
    Progress {
        id: DownloadId,
        downloaded: u64,
        total: Option<u64>,
        bytes_per_sec: u64,
        /// The file's pieces, in order, for multi-connection downloads (else empty).
        segments: Vec<SegmentProgress>,
        /// Connections transferring data right now.
        #[serde(default)]
        connections: u8,
    },
    Failed {
        id: DownloadId,
        error: String,
    },
    Completed {
        id: DownloadId,
        path: String,
    },
    /// The download was removed from the list (and its files deleted if asked).
    Removed {
        id: DownloadId,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_json_shape_is_stable() {
        let cmd = Command::Pause { id: DownloadId(7) };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(json, r#"{"cmd":"pause","id":7}"#);
        assert_eq!(serde_json::from_str::<Command>(&json).unwrap(), cmd);
    }

    #[test]
    fn new_download_optional_browser_fields_default() {
        let json = r#"{"cmd":"add","url":"https://example.com/a.zip","save_dir":null,"file_name":null,"connections":null}"#;
        let Command::Add(nd) = serde_json::from_str::<Command>(json).unwrap() else {
            panic!("expected add");
        };
        assert_eq!(nd.cookies, None);
        assert_eq!(nd.referrer, None);
    }

    #[test]
    fn queue_types_round_trip() {
        let queue = Queue {
            id: QueueId(3),
            spec: QueueSpec {
                name: "Night".into(),
                max_active: Some(2),
                speed_limit: None,
                schedule: Some(ScheduleSpec {
                    start: Some("01:00".into()),
                    stop: Some("07:00".into()),
                    days: vec![0, 1, 2, 3, 4],
                }),
                when_done: WhenDone::ShutDown,
            },
            stopped: false,
            in_window: true,
        };
        let json = serde_json::to_string(&queue).unwrap();
        assert!(json.contains(r#""when_done":"shut_down""#), "{json}");
        assert_eq!(serde_json::from_str::<Queue>(&json).unwrap(), queue);
        // Older messages without a queue still parse.
        let added = r#"{"event":"added","id":1,"file_name":"a","url":"u","save_dir":"d"}"#;
        let Event::Added { queue, .. } = serde_json::from_str(added).unwrap() else { panic!() };
        assert_eq!(queue, QueueId::MAIN);
    }

    #[test]
    fn terminal_states() {
        assert!(DownloadState::Completed.is_terminal());
        assert!(DownloadState::Failed.is_terminal());
        assert!(!DownloadState::Paused.is_terminal());
    }
}
