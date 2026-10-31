//! The download engine: persistent, named queues of downloads.
//!
//! [`Engine`] owns an [`vdm_store::Store`] and runs at most `max_active`
//! downloads at once, oldest first. Each download belongs to a queue
//! ([`QueueId::MAIN`] unless it names another), and a queue can have its own
//! limit on simultaneous downloads, its own speed limit, a daily schedule and
//! a manual stop. The engine applies a global speed limit plus optional
//! per-queue and per-download limits, honors a global [`Schedule`] too,
//! survives restarts (interrupted downloads are queued again and resume from
//! their `.vdmpart`), and reports everything as [`vdm_proto::Event`]s,
//! including when a queue runs out of work.
//!
//! Internally one actor task owns all state; the [`Engine`] handle and the
//! download tasks talk to it over a channel.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::NaiveDateTime;
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use vdm_proto::{
    Command, DownloadId, DownloadState, Event, NewDownload, Queue, QueueId, QueueSpec,
    SegmentProgress,
};
use vdm_store::{DownloadRecord, NewRecord, QueueRecord, Store};

use crate::checksum::Checksum;
use crate::download::{
    Completed, DownloadRequest, Downloader, MAX_CONNECTIONS, Observer, Options, Progress, Started,
    parse_url,
};
use crate::filename;
use crate::limiter::RateLimiter;
use crate::part::PART_SUFFIX;
use crate::schedule::Schedule;

/// Source of "now" in local time, replaceable in tests.
pub type Clock = Arc<dyn Fn() -> NaiveDateTime + Send + Sync>;

/// Engine settings.
#[derive(Clone)]
pub struct EngineConfig {
    pub options: Options,
    /// Where downloads go when the request names no folder.
    pub default_dir: PathBuf,
    /// Downloads running at the same time.
    pub max_active: usize,
    /// Global speed limit in bytes per second, shared by all downloads.
    pub speed_limit: Option<u64>,
    /// When the queue may download.
    pub schedule: Schedule,
    pub clock: Clock,
    /// How often the schedule is checked.
    pub schedule_check: Duration,
    /// Run after each download (and its checksum check), e.g. a virus scan.
    /// `{path}` is replaced by the file's path (appended when absent). A
    /// non-zero exit marks the download failed; the file is kept.
    pub after_download: Option<String>,
}

impl EngineConfig {
    pub fn new(default_dir: impl Into<PathBuf>) -> Self {
        Self {
            options: Options::default(),
            default_dir: default_dir.into(),
            max_active: 3,
            speed_limit: None,
            schedule: Schedule::always(),
            clock: Arc::new(|| chrono::Local::now().naive_local()),
            schedule_check: Duration::from_secs(1),
            after_download: None,
        }
    }
}

impl std::fmt::Debug for EngineConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineConfig")
            .field("options", &self.options)
            .field("default_dir", &self.default_dir)
            .field("max_active", &self.max_active)
            .field("speed_limit", &self.speed_limit)
            .field("schedule", &self.schedule)
            .field("schedule_check", &self.schedule_check)
            .field("after_download", &self.after_download)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] vdm_store::Error),
    #[error(transparent)]
    Download(#[from] crate::Error),
    #[error("download {0} not found")]
    NotFound(DownloadId),
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Invalid(String),
    #[error("the engine has stopped")]
    Stopped,
}

pub type EngineResult<T> = Result<T, EngineError>;

/// Handle to a running engine. Dropping it stops all downloads (they stay
/// queued and resume next time); prefer [`Engine::shutdown`] to wait for that.
pub struct Engine {
    tx: mpsc::UnboundedSender<Msg>,
    events: broadcast::Sender<Event>,
    task: Option<JoinHandle<()>>,
}

impl Engine {
    /// Start the engine on the current tokio runtime. Downloads that were
    /// running when the engine last stopped are queued again.
    pub fn start(cfg: EngineConfig, store: Store) -> EngineResult<Self> {
        let downloader = Downloader::new(cfg.options.clone())?;
        let (tx, rx) = mpsc::unbounded_channel();
        let (events, _) = broadcast::channel(1024);
        let actor = Actor::new(cfg, store, downloader, tx.clone(), events.clone())?;
        let task = tokio::spawn(actor.run(rx));
        Ok(Self { tx, events, task: Some(task) })
    }

    /// Events from now on. A receiver that falls more than 1024 events behind
    /// skips ahead (`RecvError::Lagged`).
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    /// Run a command. `Add` returns the new download's id; other commands `None`.
    pub async fn command(&self, cmd: Command) -> EngineResult<Option<DownloadId>> {
        self.call(|reply| Msg::Command(cmd, reply)).await?
    }

    pub async fn add(&self, new: NewDownload) -> EngineResult<DownloadId> {
        self.command(Command::Add(new)).await?.ok_or(EngineError::Stopped)
    }

    /// All downloads, oldest first.
    pub async fn list(&self) -> EngineResult<Vec<DownloadRecord>> {
        self.call(Msg::List).await?
    }

    /// All queues, the main queue first.
    pub async fn queues(&self) -> EngineResult<Vec<Queue>> {
        self.call(Msg::Queues).await
    }

    pub async fn add_queue(&self, spec: QueueSpec) -> EngineResult<QueueId> {
        self.call(|reply| Msg::AddQueue(spec, reply)).await?
    }

    pub async fn update_queue(&self, id: QueueId, spec: QueueSpec) -> EngineResult<()> {
        self.call(|reply| Msg::UpdateQueue(id, spec, reply)).await?
    }

    /// Delete a queue; its downloads move to the main queue.
    pub async fn delete_queue(&self, id: QueueId) -> EngineResult<()> {
        self.call(|reply| Msg::DeleteQueue(id, reply)).await?
    }

    /// Stop a queue (its running downloads go back to waiting) or start it again.
    pub async fn set_queue_stopped(&self, id: QueueId, stopped: bool) -> EngineResult<()> {
        self.call(|reply| Msg::SetQueueStopped(id, stopped, reply)).await?
    }

    pub async fn set_schedule(&self, schedule: Schedule) -> EngineResult<()> {
        self.call(|reply| Msg::SetSchedule(schedule, reply)).await
    }

    pub async fn set_max_active(&self, max_active: usize) -> EngineResult<()> {
        self.call(|reply| Msg::SetMaxActive(max_active, reply)).await
    }

    /// Replace the download options (proxy, logins, connections, ...). Running
    /// downloads keep their old options; new and resumed ones use these.
    pub async fn set_options(&self, options: Options) -> EngineResult<()> {
        let downloader = Downloader::new(options.clone())?;
        self.call(|reply| Msg::SetOptions(Box::new((options, downloader)), reply)).await
    }

    pub async fn set_after_download(&self, command: Option<String>) -> EngineResult<()> {
        self.call(|reply| Msg::SetAfterDownload(command, reply)).await
    }

    /// Stop all downloads (they stay queued) and wait until they have saved
    /// their progress. The engine accepts no more commands afterwards.
    pub async fn stop(&self) -> EngineResult<()> {
        self.call(Msg::Shutdown).await
    }

    /// [`stop`](Self::stop), then wait for the engine task to finish.
    pub async fn shutdown(mut self) -> EngineResult<()> {
        self.stop().await?;
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
        Ok(())
    }

    async fn call<T>(&self, msg: impl FnOnce(oneshot::Sender<T>) -> Msg) -> EngineResult<T> {
        let (reply, rx) = oneshot::channel();
        self.tx.send(msg(reply)).map_err(|_| EngineError::Stopped)?;
        rx.await.map_err(|_| EngineError::Stopped)
    }
}

impl Drop for Engine {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Close);
    }
}

enum Msg {
    Command(Command, oneshot::Sender<EngineResult<Option<DownloadId>>>),
    List(oneshot::Sender<EngineResult<Vec<DownloadRecord>>>),
    SetSchedule(Schedule, oneshot::Sender<()>),
    Queues(oneshot::Sender<Vec<Queue>>),
    AddQueue(QueueSpec, oneshot::Sender<EngineResult<QueueId>>),
    UpdateQueue(QueueId, QueueSpec, oneshot::Sender<EngineResult<()>>),
    DeleteQueue(QueueId, oneshot::Sender<EngineResult<()>>),
    SetQueueStopped(QueueId, bool, oneshot::Sender<EngineResult<()>>),
    SetMaxActive(usize, oneshot::Sender<()>),
    SetOptions(Box<(Options, Downloader)>, oneshot::Sender<()>),
    SetAfterDownload(Option<String>, oneshot::Sender<()>),
    Shutdown(oneshot::Sender<()>),
    /// The handle was dropped.
    Close,
    Started(DownloadId, Started),
    Segments(DownloadId, Vec<SegmentProgress>, u8),
    Progress(DownloadId, Progress),
    Done(DownloadId, crate::Result<Completed>),
    /// The checksum check and after-download command finished.
    Checked(DownloadId, Completed, Result<(), String>),
    /// A torrent stopped seeding (ratio reached, stopped, or failed).
    SeedDone(DownloadId, Result<u64, String>),
}

/// Why an active download is being cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Pause,
    /// Back to the queue: the schedule window closed or the engine is stopping.
    Requeue,
    Remove {
        delete_file: bool,
    },
}

/// A finished torrent being seeded.
struct Seeding {
    cancel: CancellationToken,
    /// Remove the download (deleting its files: `true`) once seeding stops.
    remove: Option<bool>,
}

struct Active {
    cancel: CancellationToken,
    stop: Option<Stop>,
    meter: SpeedMeter,
    queue: QueueId,
    /// Latest piece map (multi-connection downloads only).
    segments: Vec<SegmentProgress>,
    /// Connections transferring now; from `Started` until the first piece map.
    connections: u8,
}

/// A queue while the engine runs.
struct QueueState {
    record: QueueRecord,
    schedule: Schedule,
    in_window: bool,
    limiter: Arc<RateLimiter>,
    /// Started a download since it last ran out of work.
    ran: bool,
}

impl QueueState {
    fn new(record: QueueRecord, now: NaiveDateTime) -> Self {
        let schedule = schedule_of(&record.spec);
        Self {
            in_window: schedule.is_active(now),
            limiter: Arc::new(RateLimiter::new(record.spec.speed_limit)),
            schedule,
            record,
            ran: false,
        }
    }

    fn report(&self) -> Queue {
        Queue {
            id: self.record.id,
            spec: self.record.spec.clone(),
            stopped: self.record.stopped,
            in_window: self.in_window,
        }
    }

    /// May start downloads right now (not counting how many already run).
    fn open(&self) -> bool {
        !self.record.stopped && self.in_window
    }
}

/// A queue's schedule; one that doesn't parse (it was validated when saved)
/// counts as no restriction.
fn schedule_of(spec: &QueueSpec) -> Schedule {
    spec.schedule
        .as_ref()
        .and_then(|s| Schedule::from_spec(s).ok())
        .unwrap_or_else(Schedule::always)
}

struct Actor {
    store: Store,
    downloader: Downloader,
    cfg: EngineConfig,
    global: Arc<RateLimiter>,
    limits: HashMap<DownloadId, Arc<RateLimiter>>,
    active: HashMap<DownloadId, Active>,
    seeding: HashMap<DownloadId, Seeding>,
    queues: HashMap<QueueId, QueueState>,
    tx: mpsc::UnboundedSender<Msg>,
    events: broadcast::Sender<Event>,
    window_open: bool,
    closing: bool,
    shutdown: Option<oneshot::Sender<()>>,
}

impl Actor {
    fn new(
        cfg: EngineConfig,
        mut store: Store,
        downloader: Downloader,
        tx: mpsc::UnboundedSender<Msg>,
        events: broadcast::Sender<Event>,
    ) -> EngineResult<Self> {
        let mut limits = HashMap::new();
        for rec in store.list()? {
            if matches!(
                rec.state,
                DownloadState::Probing | DownloadState::Downloading | DownloadState::Verifying
            ) {
                // Interrupted by a crash or shutdown: run it again.
                store.update_state(rec.id, DownloadState::Queued, None)?;
            }
            if rec.state == DownloadState::Seeding {
                // Seeding ends with the app; the download itself is complete.
                store.update_state(rec.id, DownloadState::Completed, None)?;
            }
            if rec.speed_limit.is_some() {
                limits.insert(rec.id, Arc::new(RateLimiter::new(rec.speed_limit)));
            }
        }
        let now = (cfg.clock)();
        let queues = store
            .list_queues()?
            .into_iter()
            .map(|record| (record.id, QueueState::new(record, now)))
            .collect();
        Ok(Self {
            store,
            downloader,
            global: Arc::new(RateLimiter::new(cfg.speed_limit)),
            window_open: cfg.schedule.is_active(now),
            cfg,
            limits,
            active: HashMap::new(),
            seeding: HashMap::new(),
            queues,
            tx,
            events,
            closing: false,
            shutdown: None,
        })
    }

    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut tick = tokio::time::interval(self.cfg.schedule_check);
        tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
        self.pump();
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Some(msg) => self.handle(msg),
                    None => break,
                },
                _ = tick.tick() => self.check_schedule(),
            }
            if self.closing && self.active.is_empty() {
                break;
            }
        }
        if let Some(reply) = self.shutdown.take() {
            let _ = reply.send(());
        }
    }

    fn handle(&mut self, msg: Msg) {
        match msg {
            Msg::Command(cmd, reply) => {
                let result = self.command(cmd);
                let _ = reply.send(result);
            }
            Msg::List(reply) => {
                let _ = reply.send(self.store.list().map_err(Into::into));
            }
            Msg::SetSchedule(schedule, reply) => {
                self.cfg.schedule = schedule;
                self.check_schedule();
                let _ = reply.send(());
            }
            Msg::Queues(reply) => {
                let mut queues: Vec<Queue> = self.queues.values().map(QueueState::report).collect();
                queues.sort_by_key(|q| q.id);
                let _ = reply.send(queues);
            }
            Msg::AddQueue(spec, reply) => {
                let _ = reply.send(self.add_queue(spec));
            }
            Msg::UpdateQueue(id, spec, reply) => {
                let _ = reply.send(self.update_queue(id, spec));
            }
            Msg::DeleteQueue(id, reply) => {
                let _ = reply.send(self.delete_queue(id));
            }
            Msg::SetQueueStopped(id, stopped, reply) => {
                let _ = reply.send(self.set_queue_stopped(id, stopped));
            }
            Msg::SetMaxActive(n, reply) => {
                self.cfg.max_active = n.max(1);
                self.pump();
                let _ = reply.send(());
            }
            Msg::SetOptions(boxed, reply) => {
                let (options, downloader) = *boxed;
                self.cfg.options = options;
                self.downloader = downloader;
                let _ = reply.send(());
            }
            Msg::SetAfterDownload(command, reply) => {
                self.cfg.after_download = command;
                let _ = reply.send(());
            }
            Msg::Shutdown(reply) => {
                self.shutdown = Some(reply);
                self.close();
            }
            Msg::Close => self.close(),
            Msg::Started(id, info) => self.on_started(id, &info),
            Msg::Segments(id, segments, connections) => {
                if let Some(active) = self.active.get_mut(&id) {
                    active.segments = segments;
                    active.connections = connections;
                }
            }
            Msg::Progress(id, p) => self.on_progress(id, p),
            Msg::Done(id, result) => self.on_done(id, result),
            Msg::Checked(id, done, result) => self.on_checked(id, &done, result),
            Msg::SeedDone(id, result) => self.on_seed_done(id, result),
        }
    }

    fn command(&mut self, cmd: Command) -> EngineResult<Option<DownloadId>> {
        match cmd {
            Command::Add(new) => return self.add(new).map(Some),
            Command::Pause { id } => self.pause(id)?,
            Command::Resume { id } => self.resume(id)?,
            Command::Remove { id, delete_file } => self.remove(id, delete_file)?,
            Command::SetSpeedLimit { id: None, bytes_per_sec } => {
                self.global.set_rate(bytes_per_sec);
            }
            Command::SetSpeedLimit { id: Some(id), bytes_per_sec } => {
                self.store.set_speed_limit(id, bytes_per_sec)?;
                self.limiter(id, None).set_rate(bytes_per_sec);
            }
            Command::SetConnections { id, connections } => {
                // Takes effect the next time the download starts.
                self.store.set_connections(id, connections.clamp(1, MAX_CONNECTIONS))?;
            }
            Command::RefreshUrl { .. } => {
                return Err(EngineError::Unsupported(
                    "refreshing an expired address is not supported yet",
                ));
            }
            Command::MoveToQueue { id, queue } => self.move_to_queue(id, queue)?,
        }
        Ok(None)
    }

    // ------------------------------------------------------------ queues

    fn queue_exists(&self, id: QueueId) -> EngineResult<()> {
        if self.queues.contains_key(&id) {
            Ok(())
        } else {
            Err(EngineError::Invalid(format!("queue {} does not exist", id.0)))
        }
    }

    fn add_queue(&mut self, spec: QueueSpec) -> EngineResult<QueueId> {
        validate_queue(&spec)?;
        let id = self.store.insert_queue(&spec)?;
        let record = QueueRecord { id, spec, stopped: false };
        let state = QueueState::new(record, (self.cfg.clock)());
        self.emit(Event::QueueChanged { queue: state.report() });
        self.queues.insert(id, state);
        Ok(id)
    }

    fn update_queue(&mut self, id: QueueId, spec: QueueSpec) -> EngineResult<()> {
        validate_queue(&spec)?;
        self.queue_exists(id)?;
        self.store.update_queue(id, &spec)?;
        let now = (self.cfg.clock)();
        let Some(q) = self.queues.get_mut(&id) else { return Ok(()) };
        q.limiter.set_rate(spec.speed_limit);
        q.schedule = schedule_of(&spec);
        q.in_window = q.schedule.is_active(now);
        q.record.spec = spec;
        let report = q.report();
        let open = q.open();
        self.emit(Event::QueueChanged { queue: report });
        if !open {
            self.stop_queue_downloads(id);
        }
        self.pump();
        Ok(())
    }

    fn delete_queue(&mut self, id: QueueId) -> EngineResult<()> {
        self.queue_exists(id)?;
        let moved: Vec<DownloadId> =
            self.store.list()?.into_iter().filter(|r| r.queue == id).map(|r| r.id).collect();
        self.store.delete_queue(id)?;
        self.queues.remove(&id);
        for active in self.active.values_mut().filter(|a| a.queue == id) {
            active.queue = QueueId::MAIN;
        }
        for download in moved {
            self.emit(Event::Moved { id: download, queue: QueueId::MAIN });
        }
        self.emit(Event::QueueRemoved { id });
        self.pump();
        Ok(())
    }

    fn set_queue_stopped(&mut self, id: QueueId, stopped: bool) -> EngineResult<()> {
        self.queue_exists(id)?;
        self.store.set_queue_stopped(id, stopped)?;
        let Some(q) = self.queues.get_mut(&id) else { return Ok(()) };
        q.record.stopped = stopped;
        if stopped {
            // A stopped queue hasn't finished its work.
            q.ran = false;
        }
        let report = q.report();
        self.emit(Event::QueueChanged { queue: report });
        if stopped {
            self.stop_queue_downloads(id);
        } else {
            self.pump();
        }
        Ok(())
    }

    fn move_to_queue(&mut self, id: DownloadId, queue: QueueId) -> EngineResult<()> {
        self.queue_exists(queue)?;
        self.store.set_download_queue(id, queue)?;
        let open = self.queues.get(&queue).is_some_and(QueueState::open);
        if let Some(active) = self.active.get_mut(&id) {
            active.queue = queue;
            if !open {
                active.stop.get_or_insert(Stop::Requeue);
                active.cancel.cancel();
            }
        }
        self.emit(Event::Moved { id, queue });
        self.pump();
        Ok(())
    }

    /// Put a queue's running downloads back to waiting.
    fn stop_queue_downloads(&mut self, id: QueueId) {
        for active in self.active.values_mut().filter(|a| a.queue == id) {
            active.stop.get_or_insert(Stop::Requeue);
            active.cancel.cancel();
        }
    }

    /// Report a queue that ran out of work after starting some.
    fn check_finished(&mut self, id: QueueId) {
        let Some(q) = self.queues.get(&id) else { return };
        if !q.ran || self.active.values().any(|a| a.queue == id) {
            return;
        }
        let waiting = match self.store.list() {
            Ok(list) => list.iter().any(|r| {
                r.queue == id
                    && matches!(
                        r.state,
                        DownloadState::Queued
                            | DownloadState::Probing
                            | DownloadState::Downloading
                            | DownloadState::Verifying
                    )
            }),
            Err(err) => {
                tracing::error!(%err, "could not read the queue");
                return;
            }
        };
        if waiting {
            return;
        }
        let when_done = q.record.spec.when_done;
        if let Some(q) = self.queues.get_mut(&id) {
            q.ran = false;
        }
        tracing::info!(queue = id.0, ?when_done, "queue finished");
        self.emit(Event::QueueFinished { id, when_done });
    }

    fn add(&mut self, new: NewDownload) -> EngineResult<DownloadId> {
        let torrent = crate::torrent::is_torrent_url(&new.url);
        let url = if torrent { None } else { Some(parse_url(&new.url)?) };
        let save_dir = new.save_dir.map_or_else(|| self.cfg.default_dir.clone(), PathBuf::from);
        let mut headers = Vec::new();
        for (name, value) in
            [("Referer", new.referrer), ("User-Agent", new.user_agent), ("Cookie", new.cookies)]
        {
            if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
                headers.push((name.to_owned(), value));
            }
        }
        // Empty until the downloader picks a name, unless the user chose one.
        let file_name = new.file_name.as_deref().map(filename::sanitize).unwrap_or_default();
        let checksum = match new.checksum.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            Some(text) => Some(
                text.parse::<Checksum>()
                    .map_err(|e| EngineError::Invalid(format!("checksum: {e}")))?
                    .to_string(),
            ),
            None => None,
        };
        let queue = new.queue.unwrap_or(QueueId::MAIN);
        self.queue_exists(queue)?;
        let display = match (&url, file_name.is_empty()) {
            (_, false) => file_name.clone(),
            (Some(url), true) => filename::choose(None, url),
            (None, true) => crate::torrent::display_name(&new.url),
        };
        let url = url.map_or_else(|| new.url.trim().to_owned(), |u| u.to_string());
        let save_dir = save_dir.to_string_lossy().into_owned();
        let id = self.store.insert_download(&NewRecord {
            url: url.clone(),
            save_dir: save_dir.clone(),
            file_name,
            size: None,
            etag: None,
            last_modified: None,
            connections: new
                .connections
                .unwrap_or(self.cfg.options.connections)
                .clamp(1, MAX_CONNECTIONS),
            headers,
            speed_limit: None,
            checksum,
            queue,
        })?;
        self.emit(Event::Added { id, file_name: display, url, save_dir, queue });
        self.pump();
        Ok(id)
    }

    fn pause(&mut self, id: DownloadId) -> EngineResult<()> {
        if let Some(seed) = self.seeding.get(&id) {
            // Stop seeding; `on_seed_done` marks it completed.
            seed.cancel.cancel();
            return Ok(());
        }
        if let Some(active) = self.active.get_mut(&id) {
            if !matches!(active.stop, Some(Stop::Remove { .. })) {
                active.stop = Some(Stop::Pause);
            }
            active.cancel.cancel();
            return Ok(());
        }
        let rec = self.record(id)?;
        if matches!(rec.state, DownloadState::Queued | DownloadState::Failed) {
            self.set_state(id, DownloadState::Paused, None)?;
        }
        Ok(())
    }

    fn resume(&mut self, id: DownloadId) -> EngineResult<()> {
        if let Some(active) = self.active.get_mut(&id) {
            // Resuming something that is being paused: let it requeue instead.
            if active.stop == Some(Stop::Pause) {
                active.stop = Some(Stop::Requeue);
            }
            return Ok(());
        }
        let rec = self.record(id)?;
        if matches!(rec.state, DownloadState::Paused | DownloadState::Failed) {
            self.set_state(id, DownloadState::Queued, None)?;
            self.pump();
        }
        Ok(())
    }

    fn remove(&mut self, id: DownloadId, delete_file: bool) -> EngineResult<()> {
        if let Some(seed) = self.seeding.get_mut(&id) {
            // Removed once the seeder lets go of the files.
            seed.remove = Some(delete_file);
            seed.cancel.cancel();
            return Ok(());
        }
        if let Some(active) = self.active.get_mut(&id) {
            // Finished in `on_done`, once the download has let go of its files.
            active.stop = Some(Stop::Remove { delete_file });
            active.cancel.cancel();
            return Ok(());
        }
        let rec = self.record(id)?;
        self.finish_remove(&rec, delete_file)
    }

    fn finish_remove(&mut self, rec: &DownloadRecord, delete_file: bool) -> EngineResult<()> {
        if delete_file && !rec.file_name.is_empty() {
            let dir = Path::new(&rec.save_dir);
            let mut paths = vec![
                dir.join(format!("{}{PART_SUFFIX}", rec.file_name)),
                dir.join(format!("{}{PART_SUFFIX}.json", rec.file_name)),
            ];
            if matches!(rec.state, DownloadState::Completed | DownloadState::Seeding) {
                paths.push(dir.join(&rec.file_name));
            }
            for path in paths {
                // Torrents and streams use folders.
                let result = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                if let Err(err) = result
                    && err.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(path = %path.display(), %err, "could not delete file");
                }
            }
        }
        self.store.delete(rec.id)?;
        self.limits.remove(&rec.id);
        self.emit(Event::Removed { id: rec.id });
        Ok(())
    }

    /// Start queued downloads, oldest first, while there is room overall and
    /// in their queue, and their queue is running and inside its schedule.
    fn pump(&mut self) {
        if self.closing || !self.window_open {
            return;
        }
        while self.active.len() < self.cfg.max_active {
            let next = match self.store.list() {
                Ok(list) => list.into_iter().find(|r| {
                    r.state == DownloadState::Queued
                        && !self.active.contains_key(&r.id)
                        && self.queue_has_room(r.queue)
                }),
                Err(err) => {
                    tracing::error!(%err, "could not read the queue");
                    return;
                }
            };
            let Some(rec) = next else { return };
            if let Err(err) = self.start(rec) {
                tracing::error!(%err, "could not start a download");
                return;
            }
        }
    }

    fn queue_has_room(&self, id: QueueId) -> bool {
        let Some(q) = self.queues.get(&id) else { return false };
        let running = self.active.values().filter(|a| a.queue == id).count();
        q.open() && q.record.spec.max_active.is_none_or(|max| running < max.max(1) as usize)
    }

    fn start(&mut self, rec: DownloadRecord) -> EngineResult<()> {
        let id = rec.id;
        let queue = rec.queue;
        self.set_state(id, DownloadState::Downloading, None)?;
        let mut limits = vec![self.global.clone(), self.limiter(id, rec.speed_limit)];
        if let Some(q) = self.queues.get_mut(&queue) {
            q.ran = true;
            limits.push(q.limiter.clone());
        }
        let req = DownloadRequest {
            url: rec.url,
            out_dir: PathBuf::from(rec.save_dir),
            file_name: (!rec.file_name.is_empty()).then_some(rec.file_name),
            connections: Some(rec.connections),
            headers: rec.headers,
            limits,
            quality: vdm_media::Quality::default(),
        };
        let cancel = CancellationToken::new();
        self.active.insert(
            id,
            Active {
                cancel: cancel.clone(),
                stop: None,
                meter: SpeedMeter::default(),
                queue,
                segments: Vec::new(),
                connections: 0,
            },
        );

        let downloader = self.downloader.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let observer = TaskObserver { id, tx: tx.clone() };
            let result = downloader.download(req, observer, cancel).await;
            let _ = tx.send(Msg::Done(id, result));
        });
        Ok(())
    }

    fn on_started(&mut self, id: DownloadId, info: &Started) {
        let Some(active) = self.active.get_mut(&id) else { return };
        active.connections = info.connections;
        active.segments.clear();
        let saved = self
            .store
            .set_file_name(id, &info.file_name)
            .and_then(|()| self.store.update_remote(id, info.size, None, None));
        if let Err(err) = saved {
            tracing::warn!(%id, %err, "could not save download details");
        }
        self.emit(Event::Started {
            id,
            file_name: info.file_name.clone(),
            total: info.size,
            resumable: info.resumable,
            connections: info.connections,
            resumed_from: info.resumed_from,
        });
    }

    fn on_progress(&mut self, id: DownloadId, p: Progress) {
        let Some(active) = self.active.get_mut(&id) else { return };
        let bytes_per_sec = active.meter.update(p.downloaded);
        let (segments, connections) = (active.segments.clone(), active.connections);
        self.emit(Event::Progress {
            id,
            downloaded: p.downloaded,
            total: p.total,
            bytes_per_sec,
            segments,
            connections,
        });
    }

    fn on_done(&mut self, id: DownloadId, result: crate::Result<Completed>) {
        let Some(active) = self.active.remove(&id) else { return };
        // Only a download that ran to its end can finish its queue.
        let ended = active.stop.is_none() && !matches!(result, Err(crate::Error::Cancelled { .. }));
        let queue = active.queue;
        let outcome = match (result, active.stop) {
            (_, Some(Stop::Remove { delete_file })) => {
                self.record(id).and_then(|rec| self.finish_remove(&rec, delete_file))
            }
            (Ok(done), _) => self.check(id, done),
            (Err(crate::Error::Cancelled { .. }), Some(Stop::Pause)) => {
                self.set_state(id, DownloadState::Paused, None)
            }
            (Err(crate::Error::Cancelled { .. }), _) => {
                self.set_state(id, DownloadState::Queued, None)
            }
            (Err(err), _) => {
                let error = err.to_string();
                tracing::warn!(%id, %error, "download failed");
                let saved = self.set_state(id, DownloadState::Failed, Some(&error));
                self.emit(Event::Failed { id, error });
                saved
            }
        };
        if let Err(err) = outcome {
            tracing::error!(%id, %err, "could not record download result");
        }
        self.pump();
        if ended {
            self.check_finished(queue);
        }
    }

    /// Verify the checksum and run the after-download command (in the
    /// background), or complete right away when there is nothing to do.
    fn check(&mut self, id: DownloadId, done: Completed) -> EngineResult<()> {
        let rec = self.record(id)?;
        let checksum = rec.checksum.as_deref().and_then(|c| c.parse::<Checksum>().ok());
        let command = self.cfg.after_download.clone().filter(|c| !c.trim().is_empty());
        if checksum.is_none() && command.is_none() {
            return self.complete(id, &done);
        }
        self.set_state(id, DownloadState::Verifying, None)?;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = after_download(&done.path, checksum, command).await;
            let _ = tx.send(Msg::Checked(id, done, result));
        });
        Ok(())
    }

    fn on_checked(&mut self, id: DownloadId, done: &Completed, result: Result<(), String>) {
        // The download may have been removed while it was being checked.
        if !matches!(self.store.get(id), Ok(Some(_))) {
            return;
        }
        let outcome = match result {
            Ok(()) => self.complete(id, done),
            Err(error) => {
                tracing::warn!(%id, %error, "download failed its check");
                let saved = self.set_state(id, DownloadState::Failed, Some(&error));
                self.emit(Event::Failed { id, error });
                saved
            }
        };
        if let Err(err) = outcome {
            tracing::error!(%id, %err, "could not record the check result");
        }
        if let Ok(Some(rec)) = self.store.get(id) {
            self.check_finished(rec.queue);
        }
    }

    fn complete(&mut self, id: DownloadId, done: &Completed) -> EngineResult<()> {
        if let Some(name) = done.path.file_name() {
            self.store.set_file_name(id, &name.to_string_lossy())?;
        }
        self.store.update_remote(id, Some(done.size), None, None)?;
        self.set_state(id, DownloadState::Completed, None)?;
        self.emit(Event::Completed { id, path: done.path.to_string_lossy().into_owned() });
        if let Some(seed) = done.seed.clone()
            && !self.closing
        {
            self.start_seeding(id, seed)?;
        }
        Ok(())
    }

    /// Upload a finished torrent to other peers, in the background.
    fn start_seeding(
        &mut self,
        id: DownloadId,
        seed: Arc<crate::torrent::Seed>,
    ) -> EngineResult<()> {
        self.set_state(id, DownloadState::Seeding, None)?;
        let cancel = CancellationToken::new();
        self.seeding.insert(id, Seeding { cancel: cancel.clone(), remove: None });
        let (downloader, tx) = (self.downloader.clone(), self.tx.clone());
        tokio::spawn(async move {
            let result = downloader.seed(&seed, &[], &cancel).await.map_err(|e| e.to_string());
            let _ = tx.send(Msg::SeedDone(id, result));
        });
        Ok(())
    }

    fn on_seed_done(&mut self, id: DownloadId, result: Result<u64, String>) {
        let Some(seed) = self.seeding.remove(&id) else { return };
        match &result {
            Ok(uploaded) => tracing::info!(%id, uploaded, "stopped seeding"),
            Err(err) => tracing::warn!(%id, %err, "seeding stopped"),
        }
        let outcome = match (seed.remove, self.record(id)) {
            (Some(delete_file), Ok(rec)) => self.finish_remove(&rec, delete_file),
            (None, Ok(rec)) if rec.state == DownloadState::Seeding => {
                self.set_state(id, DownloadState::Completed, None)
            }
            _ => Ok(()),
        };
        if let Err(err) = outcome {
            tracing::error!(%id, %err, "could not record the end of seeding");
        }
    }

    fn check_schedule(&mut self) {
        let now = (self.cfg.clock)();
        let mut changed = Vec::new();
        for q in self.queues.values_mut() {
            let in_window = q.schedule.is_active(now);
            if in_window != q.in_window {
                q.in_window = in_window;
                changed.push((q.record.id, in_window, q.report()));
            }
        }
        for (id, in_window, report) in changed {
            tracing::info!(queue = id.0, in_window, "queue schedule window changed");
            self.emit(Event::QueueChanged { queue: report });
            if in_window {
                self.pump();
            } else {
                self.stop_queue_downloads(id);
            }
        }

        let open = self.cfg.schedule.is_active(now);
        if open == self.window_open {
            return;
        }
        self.window_open = open;
        if open {
            tracing::info!("schedule window opened");
            self.pump();
        } else {
            tracing::info!("schedule window closed; requeueing active downloads");
            self.stop_all(Stop::Requeue);
        }
    }

    fn close(&mut self) {
        self.closing = true;
        self.stop_all(Stop::Requeue);
        for seed in self.seeding.values() {
            seed.cancel.cancel();
        }
    }

    fn stop_all(&mut self, stop: Stop) {
        for active in self.active.values_mut() {
            active.stop.get_or_insert(stop);
            active.cancel.cancel();
        }
    }

    fn limiter(&mut self, id: DownloadId, initial: Option<u64>) -> Arc<RateLimiter> {
        self.limits.entry(id).or_insert_with(|| Arc::new(RateLimiter::new(initial))).clone()
    }

    fn record(&self, id: DownloadId) -> EngineResult<DownloadRecord> {
        self.store.get(id)?.ok_or(EngineError::NotFound(id))
    }

    fn set_state(
        &mut self,
        id: DownloadId,
        state: DownloadState,
        error: Option<&str>,
    ) -> EngineResult<()> {
        self.store.update_state(id, state, error)?;
        self.emit(Event::StateChanged { id, state });
        Ok(())
    }

    fn emit(&self, event: Event) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }
}

/// Reject queue settings that can't work.
fn validate_queue(spec: &QueueSpec) -> EngineResult<()> {
    if spec.name.trim().is_empty() {
        return Err(EngineError::Invalid("a queue needs a name".into()));
    }
    if spec.max_active == Some(0) {
        return Err(EngineError::Invalid("a queue must run at least one download".into()));
    }
    if let Some(schedule) = &spec.schedule {
        Schedule::from_spec(schedule).map_err(EngineError::Invalid)?;
    }
    Ok(())
}

/// Check `path` against `checksum`, then run the after-download `command`.
async fn after_download(
    path: &Path,
    checksum: Option<Checksum>,
    command: Option<String>,
) -> Result<(), String> {
    if let Some(expected) = checksum {
        let actual = crate::checksum::hash_file(path, expected.algorithm)
            .await
            .map_err(|e| format!("could not check the file: {e}"))?;
        if !actual.eq_ignore_ascii_case(&expected.hex) {
            return Err(format!(
                "checksum mismatch: expected {expected}, got {}:{actual}",
                expected.algorithm.name()
            ));
        }
    }
    if let Some(template) = command {
        let args = command_line(&template, path)?;
        let status = tokio::process::Command::new(&args[0])
            .args(&args[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map_err(|e| format!("could not run the after-download command `{}`: {e}", args[0]))?;
        if !status.success() {
            let code = status.code().map_or_else(|| "none".to_owned(), |c| c.to_string());
            return Err(format!(
                "the after-download command reported a problem (exit code {code})"
            ));
        }
    }
    Ok(())
}

/// Split a command template into arguments (whitespace-separated, double
/// quotes group), replacing `{path}` with `path`, or appending it if absent.
fn command_line(template: &str, path: &Path) -> Result<Vec<String>, String> {
    let path = path.to_string_lossy();
    let mut args = Vec::new();
    let mut current = String::new();
    let (mut in_quotes, mut has_token) = (false, false);
    for c in template.chars() {
        match c {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            c if c.is_whitespace() && !in_quotes => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            c => {
                current.push(c);
                has_token = true;
            }
        }
    }
    if in_quotes {
        return Err("the after-download command has an unclosed quote".into());
    }
    if has_token {
        args.push(current);
    }
    if args.is_empty() {
        return Err("the after-download command is empty".into());
    }
    if args.iter().any(|a| a.contains("{path}")) {
        for a in &mut args {
            *a = a.replace("{path}", &path);
        }
    } else {
        args.push(path.into_owned());
    }
    Ok(args)
}

/// Forwards a download task's updates to the actor.
struct TaskObserver {
    id: DownloadId,
    tx: mpsc::UnboundedSender<Msg>,
}

impl Observer for TaskObserver {
    fn started(&mut self, info: &Started) {
        let _ = self.tx.send(Msg::Started(self.id, info.clone()));
    }

    fn segments(&mut self, segments: Vec<SegmentProgress>, connections: u8) {
        let _ = self.tx.send(Msg::Segments(self.id, segments, connections));
    }

    fn progress(&mut self, progress: Progress) {
        let _ = self.tx.send(Msg::Progress(self.id, progress));
    }
}

/// Smoothed download speed from successive byte counts.
#[derive(Default)]
struct SpeedMeter {
    last: Option<(Instant, u64)>,
    bytes_per_sec: f64,
}

impl SpeedMeter {
    /// Minimum time between samples, so bursts don't swing the estimate.
    const SAMPLE: Duration = Duration::from_millis(200);

    fn update(&mut self, downloaded: u64) -> u64 {
        let now = Instant::now();
        match self.last {
            Some((_, bytes)) if downloaded < bytes => {
                // Restarted from zero.
                self.last = Some((now, downloaded));
                self.bytes_per_sec = 0.0;
            }
            Some((at, bytes)) => {
                let dt = now.duration_since(at);
                if dt >= Self::SAMPLE {
                    let sample = (downloaded - bytes) as f64 / dt.as_secs_f64();
                    self.bytes_per_sec = if self.bytes_per_sec == 0.0 {
                        sample
                    } else {
                        0.6 * self.bytes_per_sec + 0.4 * sample
                    };
                    self.last = Some((now, downloaded));
                }
            }
            None => self.last = Some((now, downloaded)),
        }
        self.bytes_per_sec as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_templates() {
        let p = Path::new("C:/Downloads/my file.zip");
        assert_eq!(
            command_line(r#""C:\Program Files\AV\scan.exe" -Scan -File "{path}""#, p).unwrap(),
            [r"C:\Program Files\AV\scan.exe", "-Scan", "-File", "C:/Downloads/my file.zip"]
        );
        assert_eq!(
            command_line("clamscan --no-summary", p).unwrap(),
            ["clamscan", "--no-summary", "C:/Downloads/my file.zip"]
        );
        assert_eq!(
            command_line("check --file={path} --x", p).unwrap(),
            ["check", "--file=C:/Downloads/my file.zip", "--x"]
        );
        assert_eq!(
            command_line(r#"a "" b"#, p).unwrap(),
            ["a", "", "b", "C:/Downloads/my file.zip"]
        );
        assert!(command_line("   ", p).is_err());
        assert!(command_line(r#"scan "unclosed"#, p).is_err());
    }
}
