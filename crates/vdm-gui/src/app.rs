//! The iced application: download list, toolbar, Add and Settings dialogs.
//!
//! The [`Engine`] runs on iced's tokio executor. Commands go to it as tasks,
//! and its events come back through a subscription and update the
//! [`Downloads`] model, which the view renders.

use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod view;

use iced::{Subscription, Task, keyboard, mouse, window};
use tokio::sync::broadcast;
use vdm_core::{Engine, EngineConfig, Options, ProxySetting, SiteLogin};
use vdm_ipc::Request;
use vdm_proto::{Command, DownloadId, DownloadState, Event, NewDownload, Queue, QueueId, WhenDone};
use vdm_store::{DownloadRecord, Store};

use crate::categories::{Category, ClipboardWatcher};
use crate::columns::Column;
use crate::i18n::{self, Lang, t};
use crate::logins::{self, KeyringStore, LoginEntry, SecretStore};
use crate::model::{self, Downloads, Row};
use crate::notify::notify;
use crate::queues::{self, QueueForm};
use crate::settings::{Paths, Settings, SettingsForm};
use crate::tray::{self, Tray};
use crate::update::{self, Available};

/// Startup options.
#[derive(Debug, Clone)]
pub struct Config {
    pub paths: Paths,
    pub settings: Settings,
    /// URLs to add right after startup.
    pub add: Vec<String>,
    /// Save a PNG of the window after the delay, then quit (for checking the layout).
    pub screenshot: Option<(PathBuf, Duration)>,
    /// Open this dialog ("add" or "settings") once the engine is up.
    pub open: Option<String>,
    /// Requests from later launches of the app (single instance).
    pub inbox: Inbox,
    /// Start hidden in the tray (started at sign-in).
    pub start_minimized: bool,
}

/// Receiver for single-instance requests, handed to one subscription.
#[derive(Clone, Default)]
pub struct Inbox(Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<Request>>>>);

impl Inbox {
    pub fn new(rx: tokio::sync::mpsc::UnboundedReceiver<Request>) -> Self {
        Self(Arc::new(Mutex::new(Some(rx))))
    }
}

impl std::fmt::Debug for Inbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Inbox")
    }
}

impl Hash for Inbox {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

pub fn run(config: Config) -> anyhow::Result<()> {
    let icon = match window::icon::from_rgba(crate::icon::icon_rgba(64), 64, 64) {
        Ok(icon) => Some(icon),
        Err(err) => {
            tracing::warn!(%err, "could not set the window icon");
            None
        }
    };
    // Hidden in the tray when started at sign-in; without a tray it starts
    // minimized instead (see `boot`).
    let visible = !(config.start_minimized && tray::SUPPORTED && config.screenshot.is_none());
    iced::application(move || App::boot(config.clone()), App::update, App::view)
        .title(App::title)
        .subscription(App::subscription)
        .theme(App::theme)
        // Chosen at startup; a language switch picks up its font after a restart.
        .default_font(i18n::ui_font())
        .window(window::Settings {
            size: WINDOW_SIZE.into(),
            min_size: Some((860.0, 540.0).into()),
            exit_on_close_request: false,
            visible,
            icon,
            ..window::Settings::default()
        })
        .run()
        .map_err(|e| anyhow::anyhow!("{e}"))
}

/// The window's size when it first opens.
const WINDOW_SIZE: (f32, f32) = (1180.0, 740.0);

/// Shared engine handle. Hashes by identity so it can key a subscription.
#[derive(Clone)]
struct Handle(Arc<Engine>);

impl std::fmt::Debug for Handle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Engine")
    }
}

impl Hash for Handle {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

/// What the engine subscription yields.
#[derive(Debug, Clone)]
enum Update {
    Event(Event),
    /// Events were dropped; reload the list.
    Lagged,
}

#[derive(Debug, Clone)]
enum Message {
    EngineStarted(Result<(Handle, Vec<DownloadRecord>, Vec<Queue>), String>),
    Engine(Update),
    Reloaded(Result<Vec<DownloadRecord>, String>),
    /// A click on a row: alone, with Ctrl (toggle) or Shift (range).
    RowClick(DownloadId),
    RevealRow(DownloadId),
    Modifiers(keyboard::Modifiers),
    SelectAll,
    DeleteKey,
    Escape,
    /// The left mouse button went up anywhere (ends column drags).
    PointerUp,
    // Column header: cursor position (column space) and the width the columns share.
    HeaderMove {
        x: f32,
        available: f32,
    },
    HeaderPress(Column),
    ResizeStart(Column),
    HeaderMenu,
    CloseMenu,
    ColumnFit,
    ColumnSizeToContent(Option<Column>),
    ColumnShift(Column, i32),
    ColumnToggle(Column),
    ColumnReset,
    WindowMinimized(Option<bool>),
    WindowResized(f32),
    // Details panel
    ToggleDetails,
    CopyText(String),
    DetailsRecord(Result<Vec<DownloadRecord>, String>),
    DemoQueue(Result<QueueId, String>),
    // Queues
    NewQueue,
    EditQueue(QueueId),
    QueueName(String),
    QueueAtOnce(String),
    QueueSpeed(String),
    QueueScheduled(bool),
    QueueStart(String),
    QueueStop(String),
    QueueDay(usize),
    QueueWhenDone(WhenDone),
    SaveQueue,
    DeleteQueue,
    QueueSaved(Result<(), String>),
    SetQueueStopped(QueueId, bool),
    ToggleMoveMenu,
    MoveSelection(QueueId),
    AddQueue(QueueId),
    // A finished queue's "when done" countdown
    PowerTick,
    PowerCancel,
    PowerNow,
    ToggleAutostart(bool),
    SetFilter(view::Filter),
    Search(String),
    SetTheme(crate::theme::ThemeChoice),
    SetLanguage(Option<Lang>),
    SystemTheme(iced::theme::Mode),
    // Toolbar
    OpenAdd,
    Pause,
    Resume,
    Remove,
    AskDelete,
    ConfirmDelete,
    OpenFolder,
    OpenSettings,
    CloseDialog,
    DismissError,
    // Add dialog
    Clipboard(Option<String>),
    AddUrl(String),
    AddFolder(String),
    AddName(String),
    AddConnections(String),
    AddChecksum(String),
    SubmitAdd,
    Added(Result<DownloadId, String>),
    /// The server's answer about a browser download shown in the Add dialog (by URL).
    Probed(String, Result<(Option<u64>, bool, String), String>),
    /// Add the shown browser download and every one still waiting.
    AcceptAllPrompts,
    /// Close the dialog and drop every browser download still waiting.
    DiscardPrompts,
    ToggleAskBrowser(bool),
    ToggleClipboardAlways(bool),
    ToggleTorrentDownloadOnly(bool),
    // Settings dialog
    SetDownloadDir(String),
    SetMaxActive(String),
    SetConnections(String),
    SetSpeedLimit(String),
    SetProxy(String),
    SetAfterDownload(String),
    LoginSite(String),
    LoginUser(String),
    LoginPassword(String),
    AddLogin,
    RemoveLogin(usize),
    Browse(BrowseFor),
    Picked(BrowseFor, Option<PathBuf>),
    ImportLinks,
    Imported(Result<Vec<String>, String>),
    ExportList,
    Exported(Result<Option<PathBuf>, String>),
    SaveSettings,
    // Engine command results
    Done(Result<(), String>),
    // Window
    CloseRequested(window::Id),
    Stopped,
    Tray(tray::Action),
    Ipc(Request),
    ClipboardTick,
    ClipboardNow(Option<String>),
    ToggleWatchClipboard(bool),
    ToggleNotify(bool),
    ToggleCloseToTray(bool),
    ToggleCategories(bool),
    CheckUpdate {
        manual: bool,
    },
    UpdateChecked {
        manual: bool,
        result: Result<Option<Available>, String>,
    },
    InstallUpdate,
    UpdateReady(Result<PathBuf, String>),
    SkipUpdate,
    DismissUpdate,
    DismissNotice,
    ToggleCheckUpdates(bool),
    TakeScreenshot,
    Screenshot(window::Screenshot),
}

/// Which folder field a "Browse…" button fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BrowseFor {
    DownloadDir,
    AddFolder,
    /// A `.torrent` file for the Add dialog's address.
    TorrentFile,
}

#[derive(Debug, Clone, Default)]
struct AddForm {
    url: String,
    folder: String,
    name: String,
    connections: String,
    checksum: String,
    queue: QueueId,
    error: Option<String>,
    busy: bool,
    /// Set when the browser handed this download over: its request details
    /// (cookies, referrer, user agent) and what the server said about it.
    browser: Option<BrowserDownload>,
}

/// A download from the browser extension, waiting in the Add dialog.
#[derive(Debug, Clone)]
struct BrowserDownload {
    request: NewDownload,
    /// The folder the dialog suggested, to tell whether the user changed it.
    suggested_folder: String,
    probe: Probe,
}

/// What the server said when the dialog asked it about the file.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Probe {
    Checking,
    Known { size: Option<u64>, resumable: bool },
    Failed(String),
}

#[derive(Debug, Clone)]
enum Dialog {
    Add(AddForm),
    Settings {
        form: SettingsForm,
        login: LoginForm,
        error: Option<String>,
    },
    ConfirmDelete(Vec<DownloadId>),
    /// `id` None: a new queue.
    Queue {
        id: Option<QueueId>,
        form: QueueForm,
        error: Option<String>,
    },
}

/// The "add a site login" row in Settings.
#[derive(Debug, Clone, Default)]
struct LoginForm {
    site: String,
    username: String,
    password: String,
}

struct App {
    config: Config,
    settings: Settings,
    engine: Option<Handle>,
    downloads: Downloads,
    dialog: Option<Dialog>,
    error: Option<String>,
    tray: Option<Tray>,
    clipboard: ClipboardWatcher,
    /// "Still running in the tray" was shown once already.
    tray_hint_shown: bool,
    quitting: bool,
    /// Downloads that arrived before the engine was up.
    pending: Vec<NewDownload>,
    /// Where site-login passwords live; None if the OS store is unavailable.
    secrets: Option<Arc<dyn SecretStore>>,
    update: UpdateState,
    /// A neutral message, e.g. "Vibe Download Manager is up to date".
    notice: Option<String>,
    /// Which downloads the list shows, and the search box.
    filter: view::Filter,
    search: String,
    /// The system's light/dark mode, for the "System" theme.
    system_mode: iced::theme::Mode,
    /// Ctrl/Shift state, for multi-selection clicks.
    modifiers: keyboard::Modifiers,
    /// The window is hidden in the tray, or minimized: no clipboard prompts then.
    window_hidden: bool,
    minimized: bool,
    /// Last cursor position over the column header, and the columns' shared width.
    header_x: f32,
    list_available: f32,
    drag: Option<ColumnDrag>,
    header_menu: Option<HeaderMenu>,
    /// All queues, the main queue first.
    queues: Vec<Queue>,
    /// The toolbar's "Move to queue" menu is open.
    move_menu: bool,
    /// A finished queue's action, waiting out its countdown.
    power: Option<PowerCountdown>,
    /// Piece maps, speed history and activity, for the details panel.
    live: crate::details::Tracker,
    /// The stored record of the download the details panel shows.
    details_record: Option<DownloadRecord>,
    /// The download whose record was last asked for, and whether it changed since.
    details_for: Option<DownloadId>,
    details_stale: bool,
    /// Screenshot aid: select the first download that reports progress (and
    /// with `Some(true)`, scroll the panel to its end).
    demo_details: Option<bool>,
    /// The window's width, to fit the toolbar and the details panel.
    window_width: f32,
    /// Browser downloads waiting for their turn in the Add dialog.
    prompts: std::collections::VecDeque<NewDownload>,
}

/// "The computer shuts down in 42 s", with a way to cancel.
#[derive(Debug, Clone)]
struct PowerCountdown {
    action: WhenDone,
    queue: String,
    deadline: std::time::Instant,
}

impl PowerCountdown {
    const SECONDS: u64 = 60;

    fn remaining(&self) -> u64 {
        self.deadline.saturating_duration_since(std::time::Instant::now()).as_secs()
    }
}

/// A column header being dragged.
#[derive(Debug, Clone, Copy)]
enum ColumnDrag {
    Resize { column: Column, last_x: f32 },
    Move { column: Column, start_x: f32, moved: bool, target: usize },
}

/// The header's right-click menu, opened over `column` at `x`.
#[derive(Debug, Clone, Copy)]
struct HeaderMenu {
    column: Column,
    x: f32,
}

/// Horizontal space between list columns.
const COLUMN_GAP: f32 = 14.0;

/// Where an available update stands.
enum UpdateState {
    Idle,
    Available(Available),
    Downloading(Available),
}

impl App {
    fn boot(config: Config) -> (Self, Task<Message>) {
        let secrets: Option<Arc<dyn SecretStore>> = match KeyringStore::new() {
            Ok(store) => Some(Arc::new(store)),
            Err(err) => {
                tracing::warn!(%err, "no credential store; site logins are unavailable");
                None
            }
        };
        let options = engine_options(&config.settings, secrets.as_deref());
        let start = Task::perform(
            start_engine(config.paths.clone(), config.settings.clone(), options),
            Message::EngineStarted,
        );
        let mut tasks = vec![start, iced::system::theme().map(Message::SystemTheme)];
        if update_url().is_some() && config.settings.check_updates {
            tasks.push(Task::perform(tokio::time::sleep(startup_delay()), |()| {
                Message::CheckUpdate { manual: false }
            }));
        }
        let start_hidden = config.start_minimized && config.screenshot.is_none();
        if let Some((_, delay)) = &config.screenshot {
            tasks.push(Task::perform(tokio::time::sleep(*delay), |()| Message::TakeScreenshot));
        }
        let tray = match Tray::new() {
            Ok(tray) => Some(tray),
            Err(err) => {
                tracing::info!(%err, "running without a tray icon");
                None
            }
        };
        if start_hidden && tray.is_none() {
            // No tray to come back from: start minimized, or visible.
            tasks.push(if tray::SUPPORTED {
                show_window()
            } else {
                window::latest().and_then(|id| window::minimize(id, true))
            });
        }
        let window_hidden = start_hidden && tray.is_some();
        let app = Self {
            settings: config.settings.clone(),
            config,
            engine: None,
            downloads: Downloads::default(),
            dialog: None,
            error: None,
            tray,
            clipboard: ClipboardWatcher::new(),
            tray_hint_shown: false,
            quitting: false,
            pending: Vec::new(),
            secrets,
            update: UpdateState::Idle,
            notice: None,
            filter: view::Filter::default(),
            search: String::new(),
            system_mode: iced::theme::Mode::None,
            modifiers: keyboard::Modifiers::default(),
            window_hidden,
            minimized: false,
            header_x: 0.0,
            list_available: 0.0,
            drag: None,
            header_menu: None,
            queues: Vec::new(),
            move_menu: false,
            power: None,
            live: crate::details::Tracker::default(),
            details_record: None,
            details_for: None,
            details_stale: false,
            demo_details: None,
            window_width: WINDOW_SIZE.0,
            prompts: std::collections::VecDeque::new(),
        };
        (app, Task::batch(tasks))
    }

    fn title(&self) -> String {
        let active = self.downloads.count(DownloadState::Downloading);
        if active == 0 {
            "Vibe Download Manager".to_owned()
        } else {
            let speed = model::fmt_speed(self.downloads.total_speed());
            t!("app.title_active", count = active, speed = speed)
        }
    }

    fn subscription(&self) -> Subscription<Message> {
        let mut subs = vec![
            window::close_requests().map(Message::CloseRequested),
            Subscription::run_with(self.config.inbox.clone(), inbox_requests).map(Message::Ipc),
            iced::system::theme_changes().map(Message::SystemTheme),
            iced::event::listen_with(input_event),
            window::resize_events().map(|(_, size)| Message::WindowResized(size.width)),
        ];
        if let Some(handle) = &self.engine {
            subs.push(Subscription::run_with(handle.clone(), engine_updates).map(Message::Engine));
        }
        if self.tray.is_some() {
            subs.push(Subscription::run(tray::actions).map(Message::Tray));
        }
        if update_url().is_some() && self.settings.check_updates {
            subs.push(
                iced::time::every(update::CHECK_INTERVAL)
                    .map(|_| Message::CheckUpdate { manual: false }),
            );
        }
        if self.power.is_some() {
            subs.push(iced::time::every(Duration::from_secs(1)).map(|_| Message::PowerTick));
        }
        if self.settings.watch_clipboard {
            subs.push(
                iced::time::every(Duration::from_millis(800)).map(|_| Message::ClipboardTick),
            );
        }
        Subscription::batch(subs)
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        let mut tasks = vec![self.handle(message)];
        tasks.extend(self.fetch_details());
        tasks.extend(self.next_prompt());
        Task::batch(tasks)
    }

    /// Open the next waiting browser download in the Add dialog, once no
    /// other dialog is up, and bring the window forward for it.
    fn next_prompt(&mut self) -> Option<Task<Message>> {
        if self.dialog.is_some() {
            return None;
        }
        let new = self.prompts.pop_front()?;
        self.move_menu = false;
        self.header_menu = None;
        let probe = self.probe_task(&new);
        let folder = new.save_dir.clone().unwrap_or_default();
        self.dialog = Some(Dialog::Add(AddForm {
            url: new.url.clone(),
            folder: folder.clone(),
            name: new.file_name.clone().unwrap_or_default(),
            connections: new.connections.unwrap_or(self.settings.connections).to_string(),
            checksum: new.checksum.clone().unwrap_or_default(),
            queue: new.queue.unwrap_or(QueueId::MAIN),
            error: None,
            busy: false,
            browser: Some(BrowserDownload {
                request: new,
                suggested_folder: folder,
                probe: Probe::Checking,
            }),
        }));
        self.window_hidden = false;
        let attention = window::latest().and_then(|id| {
            window::request_user_attention(id, Some(window::UserAttention::Informational))
        });
        Some(Task::batch([probe, show_window(), attention]))
    }

    /// Ask the server for the size and resume support of a browser download,
    /// with its cookies and referrer, for the Add dialog.
    fn probe_task(&self, new: &NewDownload) -> Task<Message> {
        let options = engine_options(&self.settings, self.secrets.as_deref());
        let url = new.url.clone();
        let headers: Vec<(String, String)> = [
            ("Referer", new.referrer.clone()),
            ("User-Agent", new.user_agent.clone()),
            ("Cookie", new.cookies.clone()),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| (name.to_owned(), v)))
        .collect();
        let key = url.clone();
        Task::perform(
            async move {
                let downloader = vdm_core::Downloader::new(options).map_err(|e| e.to_string())?;
                let cancel = vdm_core::CancellationToken::new();
                let probe = downloader.probe(&url, &headers, &cancel);
                match tokio::time::timeout(Duration::from_secs(15), probe).await {
                    Ok(Ok(info)) => Ok((info.size, info.resumable, info.file_name)),
                    Ok(Err(err)) => Err(err.to_string()),
                    Err(_) => {
                        cancel.cancel();
                        Err(t!("prompt.timed_out"))
                    }
                }
            },
            move |result| Message::Probed(key.clone(), result),
        )
    }

    /// The download the details panel shows: the only selected one.
    fn details_id(&self) -> Option<DownloadId> {
        match self.downloads.selection().as_slice() {
            [r] if self.settings.details_panel => Some(r.id),
            _ => None,
        }
    }

    /// Ask for the shown download's stored record when it isn't known yet
    /// or has changed (the server's size and validators arrive with `Started`).
    fn fetch_details(&mut self) -> Option<Task<Message>> {
        let id = self.details_id()?;
        if self.details_for == Some(id) && !self.details_stale {
            return None;
        }
        let h = self.engine.clone()?;
        self.details_for = Some(id);
        self.details_stale = false;
        Some(Task::perform(
            async move { h.0.list().await.map_err(|e| e.to_string()) },
            Message::DetailsRecord,
        ))
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::EngineStarted(Ok((handle, records, queues))) => {
                self.queues = queues;
                self.downloads = Downloads::from_records(&records);
                self.engine = Some(handle);
                let urls = std::mem::take(&mut self.config.add);
                let mut news: Vec<_> =
                    urls.into_iter().map(|url| self.new_download(url, None, None, None)).collect();
                news.append(&mut self.pending);
                let adds: Vec<_> = news.into_iter().map(|new| self.add_task(new)).collect();
                let open = match self.config.open.as_deref() {
                    Some("add") => self.update(Message::OpenAdd),
                    Some("settings") => self.update(Message::OpenSettings),
                    // For screenshots of the lower half of Settings.
                    Some("settings-end") => self.update(Message::OpenSettings).chain(
                        iced::widget::operation::snap_to_end(iced::widget::Id::new(
                            SETTINGS_SCROLL,
                        )),
                    ),
                    // For screenshots of multi-selection and the column menu.
                    Some("select-all") => self.update(Message::SelectAll),
                    // For screenshots of the details panel: the first download
                    // that is running (else the first one), with the panel open.
                    // For screenshots of the browser prompt: two downloads as
                    // the extension would hand them over.
                    Some("prompt") => {
                        let demo = [
                            "http://127.0.0.1:8093/Quarterly-Report-Q3.pdf",
                            "http://127.0.0.1:8096/VSCodeUserSetup-x64.exe",
                        ];
                        for url in demo {
                            let mut new = self.new_download(url.into(), None, None, None);
                            new.save_dir = None;
                            new.file_name = url.rsplit('/').next().map(str::to_owned);
                            new.referrer = Some("https://intranet.example.com/reports".into());
                            new.cookies = Some("session=demo".into());
                            let new = self.complete_download(new);
                            self.prompts.push_back(new);
                        }
                        Task::none()
                    }
                    Some(mode @ ("details" | "details-end")) => {
                        self.demo_details = Some(mode == "details-end");
                        self.settings.details_panel = true;
                        Task::none()
                    }
                    Some("columns") => {
                        self.header_menu = Some(HeaderMenu { column: Column::Speed, x: 520.0 });
                        Task::none()
                    }
                    // For screenshots of queues: a scheduled queue holding the
                    // last two downloads, its editor, and its countdown.
                    Some("demo-queues") => self.demo_queues(),
                    Some("queue-editor") => {
                        let id = self.queues.iter().map(|q| q.id).find(|id| *id != QueueId::MAIN);
                        self.filter = id.map_or(view::Filter::All, view::Filter::Queue);
                        self.update(id.map_or(Message::NewQueue, Message::EditQueue))
                    }
                    Some("power") => {
                        let queue = self.queues.iter().find(|q| q.id != QueueId::MAIN);
                        if let Some(queue) = queue {
                            self.filter = view::Filter::Queue(queue.id);
                        }
                        self.power = Some(PowerCountdown {
                            action: WhenDone::ShutDown,
                            queue: queue.map(queues::name).unwrap_or_default(),
                            deadline: std::time::Instant::now() + Duration::from_secs(42),
                        });
                        Task::none()
                    }
                    _ => Task::none(),
                };
                return Task::batch(adds).chain(open);
            }
            Message::EngineStarted(Err(err)) => {
                self.error = Some(t!("errors.engine", error = err));
            }
            Message::Engine(Update::Event(event)) => {
                self.live.apply(&event, std::time::Instant::now(), chrono::Local::now());
                if let Event::Started { id, .. }
                | Event::Completed { id, .. }
                | Event::Failed { id, .. }
                | Event::Moved { id, .. } = &event
                    && self.details_for == Some(*id)
                {
                    self.details_stale = true;
                }
                self.downloads.apply(&event);
                if let Some(end) = self.demo_details
                    && let Event::Progress { id, bytes_per_sec: 1.., .. } = &event
                {
                    self.demo_details = None;
                    self.downloads.select(Some(*id));
                    if end {
                        // Once the record (and so the whole panel) is in.
                        let snap = iced::widget::operation::snap_to_end(iced::widget::Id::new(
                            view::DETAILS_SCROLL,
                        ));
                        return Task::perform(tokio::time::sleep(Duration::from_secs(2)), |()| {
                            Message::Done(Ok(()))
                        })
                        .chain(snap);
                    }
                }
                self.apply_queue_event(&event);
                self.after_event(&event);
            }
            Message::Engine(Update::Lagged) => return self.reload(),
            Message::Reloaded(Ok(records)) => {
                let selected: Vec<_> = self.downloads.selection().iter().map(|r| r.id).collect();
                self.downloads = Downloads::from_records(&records);
                self.downloads.select_all(&selected);
            }
            Message::Reloaded(Err(err)) | Message::Done(Err(err)) => self.error = Some(err),
            Message::Done(Ok(())) => {}
            Message::RowClick(id) => {
                self.header_menu = None;
                if self.modifiers.command() {
                    self.downloads.toggle(id);
                } else if self.modifiers.shift() {
                    let order: Vec<_> = self.visible_rows().iter().map(|r| r.id).collect();
                    self.downloads.select_range(&order, id);
                } else {
                    self.downloads.select(Some(id));
                }
            }
            Message::RevealRow(id) => {
                if let Some(row) = self.downloads.get(id).cloned() {
                    self.reveal_rows(std::slice::from_ref(&row));
                }
            }
            Message::Modifiers(modifiers) => self.modifiers = modifiers,
            Message::SelectAll => {
                if self.dialog.is_none() {
                    let ids: Vec<_> = self.visible_rows().iter().map(|r| r.id).collect();
                    self.downloads.select_all(&ids);
                }
            }
            Message::DeleteKey => {
                if self.dialog.is_none() {
                    return self.update(Message::AskDelete);
                }
            }
            Message::Escape => {
                if self.move_menu {
                    self.move_menu = false;
                } else if self.header_menu.take().is_none() && self.dialog.is_none() {
                    self.downloads.select(None);
                }
            }
            Message::SetFilter(filter) => {
                self.filter = filter;
                // Keep only the selected rows that are still on screen.
                self.downloads.retain_selection(|row| filter.matches(row));
            }
            Message::HeaderMove { x, available } => {
                self.header_x = x;
                self.list_available = available;
                match &mut self.drag {
                    Some(ColumnDrag::Resize { column, last_x }) => {
                        let (column, dx) = (*column, x - *last_x);
                        *last_x = x;
                        self.settings.columns.resize(column, dx, available);
                    }
                    Some(ColumnDrag::Move { start_x, moved, target, .. }) => {
                        *moved |= (x - *start_x).abs() > 6.0;
                        *target = self.settings.columns.drop_index(x, available, COLUMN_GAP);
                    }
                    None => {}
                }
            }
            Message::ResizeStart(column) => {
                self.header_menu = None;
                self.drag = Some(ColumnDrag::Resize { column, last_x: self.header_x });
            }
            Message::HeaderPress(column) => {
                self.header_menu = None;
                let target = self.settings.columns.visible().position(|s| s.column == column);
                self.drag = Some(ColumnDrag::Move {
                    column,
                    start_x: self.header_x,
                    moved: false,
                    target: target.unwrap_or(0),
                });
            }
            Message::PointerUp => match self.drag.take() {
                Some(ColumnDrag::Resize { .. }) => self.save_columns(),
                Some(ColumnDrag::Move { column, moved: true, target, .. }) => {
                    self.settings.columns.move_to(column, target);
                    self.save_columns();
                }
                _ => {}
            },
            Message::HeaderMenu => {
                let columns = &self.settings.columns;
                if let Some(column) =
                    columns.column_at(self.header_x, self.list_available, COLUMN_GAP)
                {
                    self.header_menu = Some(HeaderMenu { column, x: self.header_x });
                }
            }
            Message::CloseMenu => self.header_menu = None,
            Message::ColumnFit => {
                self.settings.columns.fit = !self.settings.columns.fit;
                self.after_column_change();
            }
            Message::ColumnSizeToContent(only) => {
                let targets: Vec<Column> = match only {
                    Some(column) => vec![column],
                    None => self.settings.columns.visible().map(|s| s.column).collect(),
                };
                for column in targets {
                    let width = self.content_width(column);
                    self.settings.columns.size_to(column, width);
                }
                self.after_column_change();
            }
            Message::ColumnShift(column, step) => {
                self.settings.columns.shift(column, step);
                self.after_column_change();
            }
            Message::ColumnToggle(column) => {
                self.settings.columns.toggle(column);
                self.after_column_change();
            }
            Message::ColumnReset => {
                self.settings.columns = crate::columns::Columns::default();
                self.after_column_change();
            }
            Message::WindowMinimized(minimized) => {
                self.minimized = minimized.unwrap_or(false);
                return iced::clipboard::read().map(Message::ClipboardNow);
            }
            Message::ToggleAutostart(v) => self.with_settings(|f| f.launch_at_startup = v),
            Message::ToggleAskBrowser(v) => self.with_settings(|f| f.ask_browser_downloads = v),
            Message::ToggleClipboardAlways(v) => self.with_settings(|f| f.clipboard_always = v),
            Message::ToggleTorrentDownloadOnly(v) => {
                self.with_settings(|f| f.torrent_download_only = v);
            }
            Message::Probed(url, result) => self.on_probed(&url, result),
            Message::AcceptAllPrompts => {
                let rest: Vec<NewDownload> = self.prompts.drain(..).collect();
                let current = self.submit_add();
                return Task::batch(
                    std::iter::once(current).chain(rest.into_iter().map(|n| self.add_task(n))),
                );
            }
            Message::DiscardPrompts => {
                self.prompts.clear();
                self.dialog = None;
            }
            Message::WindowResized(width) => self.window_width = width,
            Message::ToggleDetails => {
                self.settings.details_panel = !self.settings.details_panel;
                self.details_for = None;
                self.save_columns();
            }
            Message::CopyText(text) => return iced::clipboard::write(text),
            Message::DetailsRecord(Ok(records)) => {
                self.details_record = records.into_iter().find(|r| Some(r.id) == self.details_for);
            }
            Message::DetailsRecord(Err(err)) => self.error = Some(err),
            Message::DemoQueue(Ok(id)) => self.filter = view::Filter::Queue(id),
            Message::DemoQueue(Err(err)) => self.error = Some(err),
            Message::Search(query) => self.search = query,
            Message::SetTheme(choice) => self.with_settings(|f| f.theme = choice),
            Message::SetLanguage(choice) => {
                // Previewed right away; Cancel puts the saved one back.
                i18n::set_language(choice.unwrap_or_else(Lang::system));
                self.with_settings(|f| f.language = choice);
            }
            Message::SystemTheme(mode) => self.system_mode = mode,

            Message::OpenAdd => {
                self.move_menu = false;
                self.dialog = Some(Dialog::Add(AddForm {
                    folder: self.settings.download_dir.display().to_string(),
                    connections: self.settings.connections.to_string(),
                    queue: self.current_queue(),
                    ..AddForm::default()
                }));
                // Prefill the URL from the clipboard.
                return iced::clipboard::read().map(Message::Clipboard);
            }
            Message::Clipboard(clip) => {
                if let (Some(Dialog::Add(form)), Some(clip)) = (&mut self.dialog, clip) {
                    let clip = clip.trim();
                    if form.url.is_empty() && looks_like_url(clip) {
                        form.url = clip.to_owned();
                    }
                }
            }
            Message::AddUrl(v) => self.with_add(|f| f.url = v),
            Message::AddFolder(v) => self.with_add(|f| f.folder = v),
            Message::AddName(v) => self.with_add(|f| f.name = v),
            Message::AddConnections(v) => self.with_add(|f| f.connections = v),
            Message::AddChecksum(v) => self.with_add(|f| f.checksum = v),
            Message::SubmitAdd => return self.submit_add(),
            // Adds also come from the command line, other launches and the
            // clipboard; only a submitted Add dialog reacts to them.
            Message::Added(Ok(id)) => {
                if self.add_dialog_busy() {
                    self.dialog = None;
                }
                self.downloads.select(Some(id));
            }
            Message::Added(Err(err)) => {
                if self.add_dialog_busy() {
                    self.with_add(|f| {
                        f.error = Some(err);
                        f.busy = false;
                    });
                } else {
                    self.error = Some(t!("errors.add_failed", error = err));
                }
            }

            Message::Pause => {
                return self.on_selection(Row::can_pause, |id| Command::Pause { id });
            }
            Message::Resume => {
                return self.on_selection(Row::can_resume, |id| Command::Resume { id });
            }
            Message::Remove => {
                return self
                    .on_selection(|_| true, |id| Command::Remove { id, delete_file: false });
            }
            Message::AskDelete => {
                let ids: Vec<_> = self.downloads.selection().iter().map(|r| r.id).collect();
                if !ids.is_empty() {
                    self.dialog = Some(Dialog::ConfirmDelete(ids));
                }
            }
            Message::ConfirmDelete => {
                if let Some(Dialog::ConfirmDelete(ids)) = self.dialog.take() {
                    return Task::batch(
                        ids.into_iter()
                            .map(|id| self.command(Command::Remove { id, delete_file: true })),
                    );
                }
            }
            Message::OpenFolder => {
                let rows: Vec<Row> = self.downloads.selection().into_iter().cloned().collect();
                self.reveal_rows(&rows);
            }
            Message::OpenSettings => {
                self.header_menu = None;
                self.dialog = Some(Dialog::Settings {
                    form: SettingsForm {
                        launch_at_startup: crate::autostart::is_enabled(),
                        ..SettingsForm::from_settings(&self.settings)
                    },
                    login: LoginForm::default(),
                    error: None,
                });
            }
            Message::SetProxy(v) => self.with_settings(|f| f.proxy = v),
            Message::SetAfterDownload(v) => self.with_settings(|f| f.after_download = v),
            Message::LoginSite(v) => self.with_login(|l| l.site = v),
            Message::LoginUser(v) => self.with_login(|l| l.username = v),
            Message::LoginPassword(v) => self.with_login(|l| l.password = v),
            Message::AddLogin => return self.add_login(),
            Message::RemoveLogin(i) => return self.remove_login(i),
            Message::Browse(BrowseFor::TorrentFile) => {
                let (title, filter) = (t!("add.open_torrent"), t!("files.torrent_files"));
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .add_filter(filter, &["torrent"])
                            .pick_file()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    |picked| Message::Picked(BrowseFor::TorrentFile, picked),
                );
            }
            Message::Browse(target) => {
                let title = match target {
                    BrowseFor::DownloadDir => t!("settings.download_folder"),
                    BrowseFor::AddFolder | BrowseFor::TorrentFile => t!("add.save_to"),
                };
                return Task::perform(
                    async move {
                        rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .pick_folder()
                            .await
                            .map(|h| h.path().to_path_buf())
                    },
                    move |picked| Message::Picked(target, picked),
                );
            }
            Message::Picked(_, None) => {}
            Message::Picked(target, Some(path)) => {
                let path = path.display().to_string();
                match target {
                    BrowseFor::DownloadDir => self.with_settings(|f| f.download_dir = path),
                    BrowseFor::AddFolder => self.with_add(|f| f.folder = path),
                    BrowseFor::TorrentFile => {
                        if let Some(url) = torrent_file_url(std::path::Path::new(&path)) {
                            self.with_add(|f| {
                                f.url = url;
                                f.error = None;
                            });
                        }
                    }
                }
            }
            Message::ImportLinks => {
                let (title, filter) = (t!("files.import_title"), t!("files.text_files"));
                return Task::perform(
                    async move {
                        let Some(file) = rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .add_filter(filter, &["txt", "lst", "csv"])
                            .pick_file()
                            .await
                        else {
                            return Ok(Vec::new());
                        };
                        let text = String::from_utf8_lossy(&file.read().await).into_owned();
                        Ok(links_in(&text))
                    },
                    Message::Imported,
                );
            }
            Message::Imported(Ok(urls)) => {
                if !urls.is_empty() {
                    self.dialog = None;
                }
                let adds: Vec<_> = urls
                    .into_iter()
                    .map(|url| self.add_task(self.new_download(url, None, None, None)))
                    .collect();
                return Task::batch(adds);
            }
            Message::Imported(Err(err)) | Message::Exported(Err(err)) => self.error = Some(err),
            Message::Exported(Ok(_)) => {}
            Message::ExportList => {
                let lines: String =
                    self.downloads.rows().iter().map(|r| format!("{}\n", r.url)).collect();
                let (title, name) = (t!("files.export_title"), t!("files.export_name"));
                return Task::perform(
                    async move {
                        let Some(file) = rfd::AsyncFileDialog::new()
                            .set_title(title)
                            .set_file_name(name)
                            .save_file()
                            .await
                        else {
                            return Ok(None);
                        };
                        let path = file.path().to_path_buf();
                        tokio::fs::write(&path, lines)
                            .await
                            .map_err(|e| export_failed(&path, &e))?;
                        Ok(Some(path))
                    },
                    Message::Exported,
                );
            }
            Message::SetDownloadDir(v) => self.with_settings(|f| f.download_dir = v),
            Message::SetMaxActive(v) => self.with_settings(|f| f.max_active = v),
            Message::SetConnections(v) => self.with_settings(|f| f.connections = v),
            Message::SetSpeedLimit(v) => self.with_settings(|f| f.speed_limit = v),
            Message::SaveSettings => return self.save_settings(),
            Message::NewQueue => {
                self.move_menu = false;
                self.dialog =
                    Some(Dialog::Queue { id: None, form: QueueForm::default(), error: None });
            }
            Message::EditQueue(id) => {
                if let Some(queue) = self.queues.iter().find(|q| q.id == id) {
                    self.dialog = Some(Dialog::Queue {
                        id: Some(id),
                        form: QueueForm::from_queue(queue),
                        error: None,
                    });
                }
            }
            Message::QueueName(v) => self.with_queue(|f| f.name = v),
            Message::QueueAtOnce(v) => self.with_queue(|f| f.max_active = v),
            Message::QueueSpeed(v) => self.with_queue(|f| f.speed_limit = v),
            Message::QueueScheduled(v) => self.with_queue(|f| f.scheduled = v),
            Message::QueueStart(v) => self.with_queue(|f| f.start = v),
            Message::QueueStop(v) => self.with_queue(|f| f.stop = v),
            Message::QueueDay(d) => self.with_queue(|f| {
                if let Some(day) = f.days.get_mut(d) {
                    *day = !*day;
                }
            }),
            Message::QueueWhenDone(v) => self.with_queue(|f| f.when_done = v),
            Message::SaveQueue => return self.save_queue(),
            Message::DeleteQueue => {
                if let (Some(Dialog::Queue { id: Some(id), .. }), Some(h)) =
                    (&self.dialog, self.engine.clone())
                {
                    let id = *id;
                    if self.filter == view::Filter::Queue(id) {
                        self.filter = view::Filter::All;
                    }
                    return Task::perform(
                        async move { h.0.delete_queue(id).await.map_err(|e| e.to_string()) },
                        Message::QueueSaved,
                    );
                }
            }
            Message::QueueSaved(Ok(())) => self.dialog = None,
            Message::QueueSaved(Err(err)) => {
                if let Some(Dialog::Queue { error, .. }) = &mut self.dialog {
                    *error = Some(err);
                } else {
                    self.error = Some(t!("queues.changed_failed", error = err));
                }
            }
            Message::SetQueueStopped(id, stopped) => {
                let Some(h) = self.engine.clone() else { return Task::none() };
                return Task::perform(
                    async move { h.0.set_queue_stopped(id, stopped).await.map_err(|e| e.to_string()) },
                    Message::QueueSaved,
                );
            }
            Message::ToggleMoveMenu => {
                self.header_menu = None;
                self.move_menu = !self.move_menu;
            }
            Message::MoveSelection(queue) => {
                self.move_menu = false;
                let ids: Vec<_> = self.downloads.selection().iter().map(|r| r.id).collect();
                return Task::batch(
                    ids.into_iter().map(|id| self.command(Command::MoveToQueue { id, queue })),
                );
            }
            Message::AddQueue(queue) => self.with_add(|f| f.queue = queue),
            Message::PowerTick => {
                if self.power.as_ref().is_some_and(|p| p.remaining() == 0) {
                    return self.update(Message::PowerNow);
                }
            }
            Message::PowerCancel => self.power = None,
            Message::PowerNow => {
                let Some(power) = self.power.take() else { return Task::none() };
                return self.run_power(power.action);
            }
            Message::CloseDialog => {
                if matches!(self.dialog, Some(Dialog::Settings { .. })) {
                    apply_language(&self.settings);
                }
                self.dialog = None;
            }
            Message::DismissError => self.error = None,
            Message::DismissNotice => self.notice = None,
            Message::ToggleCheckUpdates(v) => self.with_settings(|f| f.check_updates = v),
            Message::CheckUpdate { manual } => {
                let Some(url) = update_url() else {
                    if manual {
                        self.notice = Some(t!("update.no_address"));
                    }
                    return Task::none();
                };
                if matches!(self.update, UpdateState::Downloading(_)) {
                    return Task::none();
                }
                // A manual check also offers versions the user skipped before.
                let skipped =
                    if manual { Vec::new() } else { self.settings.skipped_versions.clone() };
                let options = engine_options(&self.settings, self.secrets.as_deref());
                return Task::perform(check_for_update(url, options, skipped), move |result| {
                    Message::UpdateChecked { manual, result }
                });
            }
            Message::UpdateChecked { manual, result } => match result {
                Ok(Some(available)) => {
                    if manual {
                        self.dialog = None;
                    }
                    self.update = UpdateState::Available(available);
                    // Test hook: take "Update now" without a click.
                    if std::env::var_os("VDM_UPDATE_AUTO").is_some() {
                        return self.update(Message::InstallUpdate);
                    }
                }
                Ok(None) if manual => {
                    self.notice =
                        Some(t!("update.up_to_date", version = update::current_version()));
                }
                Ok(None) => {}
                Err(err) if manual => {
                    self.error = Some(t!("update.check_failed", error = err));
                }
                Err(err) => tracing::warn!(%err, "update check failed"),
            },
            Message::SkipUpdate => {
                if let UpdateState::Available(available) = &self.update {
                    let mut saved = self.settings.clone();
                    saved.skipped_versions.push(available.version.to_string());
                    if let Err(err) = saved.save(&self.config.paths.settings) {
                        self.error = Some(t!("settings.save_failed", error = err));
                    } else {
                        self.settings = saved;
                    }
                }
                self.update = UpdateState::Idle;
            }
            Message::DismissUpdate => self.update = UpdateState::Idle,
            Message::InstallUpdate => {
                if let UpdateState::Available(available) = &self.update {
                    let available = available.clone();
                    let options = engine_options(&self.settings, self.secrets.as_deref());
                    self.update = UpdateState::Downloading(available.clone());
                    return Task::perform(
                        download_update(available, options),
                        Message::UpdateReady,
                    );
                }
            }
            Message::UpdateReady(Ok(msi)) => {
                self.update = UpdateState::Idle;
                match launch_installer(&msi) {
                    Ok(()) => return self.quit(),
                    Err(err) => self.error = Some(err),
                }
            }
            Message::UpdateReady(Err(err)) => {
                self.update = UpdateState::Idle;
                self.error = Some(t!("update.failed", error = err));
            }

            Message::CloseRequested(id) => {
                if !self.quitting && self.tray.is_some() && self.settings.close_to_tray {
                    if self.settings.notify && !self.tray_hint_shown {
                        self.tray_hint_shown = true;
                        notify(t!("notify.still_running"), t!("notify.still_running_body"));
                    }
                    self.window_hidden = true;
                    return window::set_mode(id, window::Mode::Hidden);
                }
                return self.quit();
            }
            Message::Stopped => return iced::exit(),
            Message::Tray(tray::Action::Show) => {
                self.window_hidden = false;
                return show_window();
            }
            Message::Tray(tray::Action::PauseAll) => {
                return self.for_each_row(Row::can_pause, |id| Command::Pause { id });
            }
            Message::Tray(tray::Action::ResumeAll) => {
                return self.for_each_row(Row::can_resume, |id| Command::Resume { id });
            }
            Message::Tray(tray::Action::Quit) => return self.quit(),
            Message::Ipc(request) => {
                // Plain URLs from a second launch, full downloads (with cookies)
                // from the browser extension.
                // Torrent links come from the browser via Windows (magnet links,
                // .torrent files): they get the same confirmation as browser downloads.
                let (torrents, plain): (Vec<_>, Vec<_>) =
                    request.urls.into_iter().partition(|u| vdm_core::is_torrent_url(u));
                let mut news: Vec<_> =
                    plain.into_iter().map(|url| self.new_download(url, None, None, None)).collect();
                let torrents: Vec<_> = torrents
                    .into_iter()
                    .map(|url| self.new_download(url, None, None, None))
                    .collect();
                if self.settings.ask_browser_downloads {
                    self.prompts.extend(torrents);
                } else {
                    news.extend(torrents);
                }
                let browser: Vec<_> =
                    request.downloads.into_iter().map(|d| self.complete_download(d)).collect();
                if self.settings.ask_browser_downloads {
                    // Each opens in the Add dialog in turn (see `next_prompt`).
                    self.prompts.extend(browser);
                } else {
                    news.extend(browser);
                }
                let adds = if self.engine.is_some() {
                    Task::batch(news.into_iter().map(|new| self.add_task(new)))
                } else {
                    // Still starting: add them once the engine is up.
                    self.pending.extend(news);
                    Task::none()
                };
                if request.show {
                    self.window_hidden = false;
                    return adds.chain(show_window());
                }
                return adds;
            }
            // Check the window first: links copied while it is minimized or in
            // the tray are noted but never offered.
            Message::ClipboardTick => {
                return window::latest()
                    .and_then(window::is_minimized)
                    .map(Message::WindowMinimized);
            }
            Message::ClipboardNow(text) => {
                // Always observe, so the watcher's memory stays current. Links
                // copied while the window is minimized or in the tray are only
                // offered when the user asked for that.
                let in_view = !self.window_hidden && !self.minimized;
                if let Some(url) = self.clipboard.observe(text.as_deref())
                    && (in_view || self.settings.clipboard_always)
                    && self.engine.is_some()
                    && self.dialog.is_none()
                {
                    self.dialog = Some(Dialog::Add(AddForm {
                        url,
                        folder: self.settings.download_dir.display().to_string(),
                        connections: self.settings.connections.to_string(),
                        queue: self.current_queue(),
                        ..AddForm::default()
                    }));
                    self.window_hidden = false;
                    return show_window();
                }
            }
            Message::ToggleWatchClipboard(v) => self.with_settings(|f| f.watch_clipboard = v),
            Message::ToggleNotify(v) => self.with_settings(|f| f.notify = v),
            Message::ToggleCloseToTray(v) => self.with_settings(|f| f.close_to_tray = v),
            Message::ToggleCategories(v) => self.with_settings(|f| f.use_categories = v),
            Message::TakeScreenshot => {
                return window::latest().and_then(window::screenshot).map(Message::Screenshot);
            }
            Message::Screenshot(shot) => {
                if let Some((path, _)) = &self.config.screenshot
                    && let Err(err) = save_png(path, &shot)
                {
                    eprintln!("could not save screenshot: {err}");
                }
                return iced::exit();
            }
        }
        Task::none()
    }

    // ------------------------------------------------------------ helpers

    fn new_download(
        &self,
        url: String,
        folder: Option<String>,
        name: Option<String>,
        connections: Option<u8>,
    ) -> NewDownload {
        let save_dir = folder.unwrap_or_else(|| self.default_folder(&url));
        NewDownload {
            url,
            save_dir: Some(save_dir),
            file_name: name,
            connections: Some(connections.unwrap_or(self.settings.connections)),
            referrer: None,
            user_agent: None,
            cookies: None,
            checksum: None,
            queue: None,
        }
    }

    /// Fill in what a browser download leaves open: folder and connections.
    /// The browser's suggested name, when there is one, picks the category.
    fn complete_download(&self, mut new: NewDownload) -> NewDownload {
        if new.save_dir.is_none() {
            let folder = match new.file_name.as_deref().filter(|n| !n.trim().is_empty()) {
                Some(name) => self.default_folder_for_name(name),
                None => self.default_folder(&new.url),
            };
            new.save_dir = Some(folder);
        }
        new.connections.get_or_insert(self.settings.connections);
        new
    }

    fn add_login(&mut self) -> Task<Message> {
        let (Some(secrets), Some(Dialog::Settings { login, .. })) = (&self.secrets, &self.dialog)
        else {
            return Task::none();
        };
        let Some(site) = LoginEntry::normalized_site(&login.site) else {
            self.set_settings_error(t!("settings.not_a_site", site = login.site.trim()));
            return Task::none();
        };
        let entry = LoginEntry { site, username: login.username.trim().to_owned() };
        let password = login.password.clone();
        let mut entries = self.settings.logins.clone();
        if let Err(err) = logins::upsert(&mut entries, entry, &password, secrets.as_ref()) {
            self.set_settings_error(t!("settings.login_save_failed", error = err));
            return Task::none();
        }
        self.with_login(|l| *l = LoginForm::default());
        self.store_logins(entries)
    }

    fn remove_login(&mut self, index: usize) -> Task<Message> {
        let (Some(secrets), Some(entry)) =
            (&self.secrets, self.settings.logins.get(index).cloned())
        else {
            return Task::none();
        };
        let mut entries = self.settings.logins.clone();
        if let Err(err) = logins::remove(&mut entries, &entry, secrets.as_ref()) {
            self.set_settings_error(t!("settings.login_remove_failed", error = err));
            return Task::none();
        }
        self.store_logins(entries)
    }

    /// Save the login list right away (passwords are already in the store)
    /// and give the engine the new credentials.
    fn store_logins(&mut self, entries: Vec<LoginEntry>) -> Task<Message> {
        let mut saved = self.settings.clone();
        saved.logins = entries;
        if let Err(err) = saved.save(&self.config.paths.settings) {
            self.set_settings_error(t!("settings.save_failed", error = err));
            return Task::none();
        }
        self.settings = saved;
        self.push_options()
    }

    /// Send the current proxy, logins and connection settings to the engine.
    fn push_options(&self) -> Task<Message> {
        let Some(h) = self.engine.clone() else { return Task::none() };
        let options = engine_options(&self.settings, self.secrets.as_deref());
        let after = Some(self.settings.after_download.clone()).filter(|c| !c.trim().is_empty());
        Task::perform(
            async move {
                h.0.set_options(options).await.map_err(|e| e.to_string())?;
                h.0.set_after_download(after).await.map_err(|e| e.to_string())
            },
            Message::Done,
        )
    }

    fn with_login(&mut self, f: impl FnOnce(&mut LoginForm)) {
        if let Some(Dialog::Settings { login, error, .. }) = &mut self.dialog {
            f(login);
            *error = None;
        }
    }

    fn add_dialog_busy(&self) -> bool {
        matches!(&self.dialog, Some(Dialog::Add(form)) if form.busy)
    }

    /// The default folder for `url`: the download folder, plus a category
    /// subfolder when sorting by type is on.
    fn default_folder(&self, url: &str) -> String {
        self.default_folder_for_name(&model::display_name(url))
    }

    /// The default folder for a file called `name` (category by its extension).
    fn default_folder_for_name(&self, name: &str) -> String {
        let dir = &self.settings.download_dir;
        let category = Category::from_name(name);
        match category.folder() {
            Some(sub) if self.settings.use_categories => dir.join(sub).display().to_string(),
            _ => dir.display().to_string(),
        }
    }

    /// Notifications and the tray tooltip.
    fn after_event(&mut self, event: &Event) {
        if self.settings.notify {
            match event {
                Event::Completed { id, .. } => {
                    if let Some(row) = self.downloads.get(*id) {
                        notify(t!("notify.complete"), row.file_name.clone());
                    }
                }
                Event::Failed { id, error } => {
                    let name = self
                        .downloads
                        .get(*id)
                        .map_or_else(|| t!("notify.a_download"), |r| r.file_name.clone());
                    notify(t!("notify.failed"), format!("{name}: {error}"));
                }
                _ => {}
            }
        }
        let title = self.title();
        if let Some(tray) = &mut self.tray {
            tray.set_tooltip(&title);
        }
    }

    /// Stop the engine (saving progress), then exit.
    fn quit(&mut self) -> Task<Message> {
        if self.quitting {
            return iced::exit();
        }
        self.quitting = true;
        match self.engine.clone() {
            Some(h) => Task::perform(async move { h.0.stop().await }, |_| Message::Stopped),
            None => iced::exit(),
        }
    }

    fn for_each_row(
        &self,
        pick: fn(&Row) -> bool,
        cmd: impl Fn(DownloadId) -> Command,
    ) -> Task<Message> {
        let ids: Vec<_> = self.downloads.rows().iter().filter(|r| pick(r)).map(|r| r.id).collect();
        Task::batch(ids.into_iter().map(|id| self.command(cmd(id))))
    }

    fn submit_add(&mut self) -> Task<Message> {
        let Some(Dialog::Add(f)) = &self.dialog else { return Task::none() };
        let url = f.url.trim().to_owned();
        if url.is_empty() {
            self.with_add(|f| f.error = Some(t!("add.need_address")));
            return Task::none();
        }
        let connections = match f.connections.trim().parse::<u8>() {
            Ok(n) if (1..=vdm_core::MAX_CONNECTIONS).contains(&n) => n,
            _ => {
                let max = vdm_core::MAX_CONNECTIONS;
                self.with_add(|f| f.error = Some(t!("add.bad_connections", max = max)));
                return Task::none();
            }
        };
        // The prefilled default folder still gets sorted by type.
        let folder = Some(f.folder.trim().to_owned())
            .filter(|s| !s.is_empty() && std::path::Path::new(s) != self.settings.download_dir);
        let name = Some(f.name.trim().to_owned()).filter(|s| !s.is_empty());
        let checksum = Some(f.checksum.trim().to_owned()).filter(|s| !s.is_empty());
        if let Some(sum) = &checksum
            && let Err(err) = sum.parse::<vdm_core::Checksum>()
        {
            self.with_add(|f| f.error = Some(t!("add.bad_checksum", error = err)));
            return Task::none();
        }
        let mut new = self.new_download(url, folder, name, Some(connections));
        new.checksum = checksum;
        new.queue = Some(f.queue);
        if let Some(browser) = &f.browser {
            new.referrer.clone_from(&browser.request.referrer);
            new.user_agent.clone_from(&browser.request.user_agent);
            new.cookies.clone_from(&browser.request.cookies);
        }
        if self.engine.is_none() {
            // Still starting: it is added once the engine is up.
            self.pending.push(new);
            self.dialog = None;
            return Task::none();
        }
        self.with_add(|f| {
            f.busy = true;
            f.error = None;
        });
        self.add_task(new)
    }

    /// Show the server's answer in the Add dialog, if it still shows that download.
    /// A name from the server fills an empty name field, and picks the
    /// folder for its type unless the user chose one.
    fn on_probed(&mut self, url: &str, result: Result<(Option<u64>, bool, String), String>) {
        let Some(Dialog::Add(f)) = &self.dialog else { return };
        if f.url != url || f.browser.is_none() {
            return;
        }
        let renamed = match &result {
            Ok((_, _, name)) if f.name.trim().is_empty() && !name.is_empty() => Some(name.clone()),
            _ => None,
        };
        let folder = renamed.as_deref().map(|name| self.default_folder_for_name(name));
        self.with_add(|f| {
            let Some(browser) = &mut f.browser else { return };
            browser.probe = match result {
                Ok((size, resumable, _)) => Probe::Known { size, resumable },
                Err(err) => Probe::Failed(err),
            };
            if let Some(name) = renamed {
                f.name = name;
                if let Some(folder) = folder
                    && f.folder == browser.suggested_folder
                {
                    browser.suggested_folder.clone_from(&folder);
                    f.folder = folder;
                }
            }
        });
    }

    fn add_task(&self, new: NewDownload) -> Task<Message> {
        let Some(h) = self.engine.clone() else { return Task::none() };
        Task::perform(async move { h.0.add(new).await.map_err(|e| e.to_string()) }, Message::Added)
    }

    fn save_settings(&mut self) -> Task<Message> {
        let Some(Dialog::Settings { form, .. }) = &self.dialog else { return Task::none() };
        let parsed = form.parse();
        let new = match parsed {
            Ok(s) => s,
            Err(err) => {
                self.set_settings_error(err);
                return Task::none();
            }
        };
        // The form holds neither the logins (saved as they're edited) nor the
        // skipped update versions.
        let new = Settings {
            logins: self.settings.logins.clone(),
            skipped_versions: self.settings.skipped_versions.clone(),
            columns: self.settings.columns.clone(),
            details_panel: self.settings.details_panel,
            ..new
        };
        let autostart = form.launch_at_startup;
        if autostart != crate::autostart::is_enabled()
            && let Err(err) = crate::autostart::set(autostart)
        {
            self.set_settings_error(t!("settings.autostart_failed", error = err));
            return Task::none();
        }
        if let Err(err) = new.save(&self.config.paths.settings) {
            self.set_settings_error(t!("settings.save_failed", error = err));
            return Task::none();
        }
        self.settings = new.clone();
        self.dialog = None;
        apply_language(&self.settings);
        if let Some(tray) = &self.tray {
            tray.relabel();
        }
        let Some(h) = self.engine.clone() else { return Task::none() };
        let limits = Task::perform(
            async move {
                h.0.set_max_active(new.max_active).await.map_err(|e| e.to_string())?;
                let cmd = Command::SetSpeedLimit { id: None, bytes_per_sec: new.speed_limit };
                h.0.command(cmd).await.map(|_| ()).map_err(|e| e.to_string())
            },
            Message::Done,
        );
        Task::batch([limits, self.push_options()])
    }

    fn set_settings_error(&mut self, err: String) {
        if let Some(Dialog::Settings { error, .. }) = &mut self.dialog {
            *error = Some(err);
        } else {
            self.error = Some(err);
        }
    }

    /// Run `cmd` for every selected row that `applies` to.
    fn on_selection(
        &self,
        applies: impl Fn(&Row) -> bool,
        cmd: impl Fn(DownloadId) -> Command,
    ) -> Task<Message> {
        let ids: Vec<_> =
            self.downloads.selection().into_iter().filter(|r| applies(r)).map(|r| r.id).collect();
        Task::batch(ids.into_iter().map(|id| self.command(cmd(id))))
    }

    /// Show downloads in the file manager: one window per folder, at most five.
    fn reveal_rows(&mut self, rows: &[Row]) {
        let mut folders: Vec<&str> = Vec::new();
        for row in rows {
            if folders.contains(&row.save_dir.as_str()) || folders.len() >= 5 {
                continue;
            }
            folders.push(&row.save_dir);
            let path = PathBuf::from(&row.save_dir).join(&row.file_name);
            if let Err(err) = reveal(&path, row.state == DownloadState::Completed) {
                self.error = Some(t!("errors.open_folder", error = err));
                return;
            }
        }
    }

    /// Screenshot aid: an "Overnight" queue (01:00–07:00 on weekdays, then
    /// shut down) holding the last two downloads.
    fn demo_queues(&mut self) -> Task<Message> {
        let Some(h) = self.engine.clone() else { return Task::none() };
        let ids: Vec<DownloadId> =
            self.downloads.rows().iter().rev().take(2).map(|r| r.id).collect();
        let spec = vdm_proto::QueueSpec {
            name: "Overnight".into(),
            max_active: Some(2),
            speed_limit: None,
            schedule: Some(vdm_proto::ScheduleSpec {
                start: Some("01:00".into()),
                stop: Some("07:00".into()),
                days: vec![0, 1, 2, 3, 4],
            }),
            when_done: WhenDone::ShutDown,
        };
        Task::perform(
            async move {
                let id = h.0.add_queue(spec).await.map_err(|e| e.to_string())?;
                for download in ids {
                    let cmd = Command::MoveToQueue { id: download, queue: id };
                    h.0.command(cmd).await.map_err(|e| e.to_string())?;
                }
                Ok(id)
            },
            Message::DemoQueue,
        )
    }

    /// The queue new downloads go to: the one being viewed, else the main queue.
    fn current_queue(&self) -> QueueId {
        match self.filter {
            view::Filter::Queue(id) => id,
            _ => QueueId::MAIN,
        }
    }

    fn with_queue(&mut self, f: impl FnOnce(&mut QueueForm)) {
        if let Some(Dialog::Queue { form, error, .. }) = &mut self.dialog {
            f(form);
            *error = None;
        }
    }

    fn save_queue(&mut self) -> Task<Message> {
        let Some(Dialog::Queue { id, form, error }) = &mut self.dialog else { return Task::none() };
        let spec = match form.parse(*id == Some(QueueId::MAIN)) {
            Ok(spec) => spec,
            Err(err) => {
                *error = Some(err);
                return Task::none();
            }
        };
        let (id, Some(h)) = (*id, self.engine.clone()) else { return Task::none() };
        Task::perform(
            async move {
                match id {
                    Some(id) => h.0.update_queue(id, spec).await,
                    None => h.0.add_queue(spec).await.map(|_| ()),
                }
                .map_err(|e| e.to_string())
            },
            Message::QueueSaved,
        )
    }

    /// Keep the queue list in step with the engine.
    fn apply_queue_event(&mut self, event: &Event) {
        match event {
            Event::QueueChanged { queue } => {
                match self.queues.iter_mut().find(|q| q.id == queue.id) {
                    Some(q) => *q = queue.clone(),
                    None => {
                        self.queues.push(queue.clone());
                        self.queues.sort_by_key(|q| q.id);
                    }
                }
            }
            Event::QueueRemoved { id } => {
                self.queues.retain(|q| q.id != *id);
                if self.filter == view::Filter::Queue(*id) {
                    self.filter = view::Filter::All;
                }
            }
            Event::QueueFinished { id, when_done } => {
                let name =
                    self.queues.iter().find(|q| q.id == *id).map(queues::name).unwrap_or_default();
                if self.settings.notify {
                    notify(t!("notify.queue_done"), t!("notify.queue_done_body", queue = name));
                }
                if *when_done != WhenDone::Nothing {
                    self.power = Some(PowerCountdown {
                        action: *when_done,
                        queue: name,
                        deadline: std::time::Instant::now()
                            + Duration::from_secs(PowerCountdown::SECONDS),
                    });
                }
            }
            _ => {}
        }
    }

    /// Carry out a finished queue's action. Shutting down first stops the
    /// engine so every download saves its progress.
    fn run_power(&mut self, action: WhenDone) -> Task<Message> {
        match action {
            WhenDone::Nothing => Task::none(),
            WhenDone::Exit => self.quit(),
            WhenDone::ShutDown => {
                self.quitting = true;
                let engine = self.engine.clone();
                Task::perform(
                    async move {
                        if let Some(h) = engine {
                            let _ = h.0.stop().await;
                        }
                        tokio::task::spawn_blocking(move || crate::power::run(action)).await
                    },
                    |_| Message::Stopped,
                )
            }
            WhenDone::Sleep | WhenDone::Hibernate => Task::perform(
                async move {
                    tokio::task::spawn_blocking(move || crate::power::run(action))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r)
                        .map_err(|e| t!("power.failed", error = e))
                },
                Message::Done,
            ),
        }
    }

    /// Save the column layout right away, and close the header menu.
    fn after_column_change(&mut self) {
        self.header_menu = None;
        self.save_columns();
    }

    fn save_columns(&mut self) {
        if let Err(err) = self.settings.save(&self.config.paths.settings) {
            self.error = Some(t!("settings.save_failed", error = err));
        }
    }

    fn command(&self, cmd: Command) -> Task<Message> {
        let Some(h) = self.engine.clone() else { return Task::none() };
        Task::perform(
            async move { h.0.command(cmd).await.map(|_| ()).map_err(|e| e.to_string()) },
            Message::Done,
        )
    }

    fn reload(&self) -> Task<Message> {
        let Some(h) = self.engine.clone() else { return Task::none() };
        Task::perform(async move { h.0.list().await.map_err(|e| e.to_string()) }, Message::Reloaded)
    }

    fn with_add(&mut self, f: impl FnOnce(&mut AddForm)) {
        if let Some(Dialog::Add(form)) = &mut self.dialog {
            f(form);
        }
    }

    fn with_settings(&mut self, f: impl FnOnce(&mut SettingsForm)) {
        if let Some(Dialog::Settings { form, error, .. }) = &mut self.dialog {
            f(form);
            *error = None;
        }
    }
}

/// Download options from the settings, with site-login passwords from `secrets`.
fn engine_options(settings: &Settings, secrets: Option<&dyn SecretStore>) -> Options {
    let logins = match secrets {
        Some(store) => {
            let (resolved, problems) = logins::resolve(&settings.logins, store);
            for problem in problems {
                tracing::warn!(%problem, "site login skipped");
            }
            resolved
                .into_iter()
                .map(|l| SiteLogin { site: l.site, username: l.username, password: l.password })
                .collect()
        }
        None => Vec::new(),
    };
    Options {
        connections: settings.connections,
        proxy: settings.proxy.parse().unwrap_or(ProxySetting::System),
        logins,
        torrent_upload: !settings.torrent_download_only,
        ..Options::default()
    }
}

/// A `.torrent` file as a `file://` URL, the way downloads refer to it.
pub fn torrent_file_url(path: &std::path::Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    // Windows' canonical form starts with `\\?\`, which file URLs don't take.
    let path = PathBuf::from(path.to_string_lossy().trim_start_matches(r"\\?\"));
    reqwest::Url::from_file_path(path).ok().map(String::from)
}

async fn start_engine(
    paths: Paths,
    settings: Settings,
    options: Options,
) -> Result<(Handle, Vec<DownloadRecord>, Vec<Queue>), String> {
    let store =
        Store::open(&paths.database).map_err(|e| format!("{}: {e}", paths.database.display()))?;
    let mut cfg = EngineConfig::new(&settings.download_dir);
    cfg.max_active = settings.max_active;
    cfg.speed_limit = settings.speed_limit;
    cfg.options = options;
    cfg.after_download = Some(settings.after_download.clone()).filter(|c| !c.trim().is_empty());
    let engine = Engine::start(cfg, store).map_err(|e| e.to_string())?;
    let records = engine.list().await.map_err(|e| e.to_string())?;
    let queues = engine.queues().await.map_err(|e| e.to_string())?;
    Ok((Handle(Arc::new(engine)), records, queues))
}

/// Bring the window back from the tray or from behind other windows.
fn show_window() -> Task<Message> {
    window::latest().and_then(|id| {
        window::set_mode(id, window::Mode::Windowed)
            .chain(window::minimize(id, false))
            .chain(window::gain_focus(id))
    })
}

/// Single-instance requests as a stream, for `Subscription::run_with`.
fn inbox_requests(inbox: &Inbox) -> impl iced::futures::Stream<Item = Request> + use<> {
    let rx = inbox.0.lock().unwrap_or_else(|e| e.into_inner()).take();
    iced::futures::stream::unfold(rx, |rx| async move {
        let mut rx = rx?;
        let request = rx.recv().await?;
        Some((request, Some(rx)))
    })
}

/// Engine events as a stream, for `Subscription::run_with`.
fn engine_updates(handle: &Handle) -> impl iced::futures::Stream<Item = Update> + use<> {
    let rx = handle.0.subscribe();
    iced::futures::stream::unfold(rx, |mut rx| async move {
        match rx.recv().await {
            Ok(event) => Some((Update::Event(event), rx)),
            Err(broadcast::error::RecvError::Lagged(_)) => Some((Update::Lagged, rx)),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    })
}

// ---------------------------------------------------------------- widgets

/// Id of the Settings dialog's scrollable body.
const SETTINGS_SCROLL: &str = "settings-scroll";

/// Every http(s)/ftp link in a text file: one per line, or anywhere in it.
fn links_in(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in text.split(|c: char| c.is_whitespace() || c == ',' || c == ';' || c == '"') {
        let word = word.trim_matches(|c: char| matches!(c, '<' | '>' | '(' | ')' | '\''));
        let lower = word.to_ascii_lowercase();
        let is_link = ["http://", "https://", "ftp://"].iter().any(|p| lower.starts_with(p));
        if is_link && word.len() > 8 && !out.iter().any(|u| u == word) {
            out.push(word.to_owned());
        }
    }
    out
}

/// The update manifest address: `VDM_UPDATE_URL` at run time (for testing),
/// else the one baked in at build time; None when neither is set.
fn update_url() -> Option<String> {
    std::env::var("VDM_UPDATE_URL")
        .ok()
        .or_else(|| option_env!("VDM_UPDATE_URL").map(str::to_owned))
        .filter(|u| !u.trim().is_empty())
}

/// `VDM_UPDATE_DELAY` seconds (for testing), else the normal startup delay.
fn startup_delay() -> Duration {
    std::env::var("VDM_UPDATE_DELAY")
        .ok()
        .and_then(|s| s.parse().ok())
        .map(Duration::from_secs_f64)
        .unwrap_or(update::STARTUP_DELAY)
}

/// Fetch the manifest and decide whether there is an update to offer.
async fn check_for_update(
    url: String,
    options: Options,
    skipped: Vec<String>,
) -> Result<Option<Available>, String> {
    let mut client = reqwest::Client::builder()
        .user_agent(concat!("VibeDownloadManager/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30));
    client = match &options.proxy {
        ProxySetting::System => client,
        ProxySetting::None => client.no_proxy(),
        ProxySetting::Url(p) => client.proxy(reqwest::Proxy::all(p).map_err(|e| e.to_string())?),
    };
    let text = client
        .build()
        .map_err(|e| e.to_string())?
        .get(&url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|e| e.to_string())?
        .text()
        .await
        .map_err(|e| e.to_string())?;
    let manifest = update::parse_manifest(&text)?;
    update::check(&manifest, &update::current_version(), &update::platform_key(), false, &skipped)
}

/// Download the installer with our own engine and check it: SHA-256 from
/// the manifest, and the publisher's signature when the build names one.
async fn download_update(available: Available, options: Options) -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join("vdm-update");
    tokio::fs::create_dir_all(&dir).await.map_err(|e| e.to_string())?;
    let downloader = vdm_core::Downloader::new(options).map_err(|e| e.to_string())?;
    let req = vdm_core::DownloadRequest::new(available.asset.url.clone(), &dir);
    let done = downloader
        .download(req, |_: vdm_core::Progress| {}, vdm_core::CancellationToken::new())
        .await
        .map_err(|e| e.to_string())?;
    let expected = vdm_core::Checksum {
        algorithm: vdm_core::checksum::Algorithm::Sha256,
        hex: available.asset.sha256.clone(),
    };
    let ok = vdm_core::checksum::verify(&done.path, &expected).await.map_err(|e| e.to_string())?;
    if !ok {
        let _ = tokio::fs::remove_file(&done.path).await;
        return Err("the downloaded installer doesn't match its checksum".into());
    }
    if let Some(publisher) = option_env!("VDM_UPDATE_PUBLISHER") {
        verify_signature(&done.path, publisher).await?;
    }
    Ok(done.path)
}

/// Require a valid Authenticode signature from `publisher` (Windows).
async fn verify_signature(path: &std::path::Path, publisher: &str) -> Result<(), String> {
    if !cfg!(windows) {
        return Ok(());
    }
    let script = format!(
        "$s = Get-AuthenticodeSignature -LiteralPath '{}'; \"$($s.Status)|$($s.SignerCertificate.Subject)\"",
        path.display().to_string().replace('\'', "''")
    );
    let out = tokio::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .output()
        .await
        .map_err(|e| format!("could not check the installer's signature: {e}"))?;
    let report = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let (status, subject) = report.split_once('|').unwrap_or((report.as_str(), ""));
    if status != "Valid" || !subject.contains(publisher) {
        return Err(format!("the installer's signature isn't valid ({status}; {subject})"));
    }
    Ok(())
}

/// Start the silent install (which relaunches the app afterwards). With
/// `VDM_UPDATE_DRY_RUN` set, only write the command to a file next to the MSI.
fn launch_installer(msi: &std::path::Path) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let log = msi.with_extension("log");
    let (program, args) =
        update::install_command(msi, &log, &exe).ok_or_else(|| t!("update.windows_only"))?;
    if std::env::var_os("VDM_UPDATE_DRY_RUN").is_some() {
        let file = msi.with_extension("command.txt");
        std::fs::write(&file, format!("{program} {}", args.join(" ")))
            .map_err(|e| e.to_string())?;
        return Err(format!("Dry run: the install command was written to {}", file.display()));
    }
    spawn_hidden(&program, &args)
}

#[cfg(windows)]
fn spawn_hidden(program: &str, args: &[String]) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = std::process::Command::new(program);
    // The last argument is a complete cmd command line; pass it untouched.
    if let Some((line, rest)) = args.split_last() {
        cmd.args(rest).raw_arg(line);
    }
    cmd.creation_flags(CREATE_NO_WINDOW)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| t!("update.installer_failed", error = e))
}

#[cfg(not(windows))]
fn spawn_hidden(_program: &str, _args: &[String]) -> Result<(), String> {
    Err(t!("update.windows_only"))
}

/// Keyboard and mouse events the whole window cares about.
fn input_event(
    event: iced::Event,
    status: iced::event::Status,
    _window: window::Id,
) -> Option<Message> {
    use iced::event::Status;
    use keyboard::key::Named;
    match event {
        iced::Event::Keyboard(keyboard::Event::ModifiersChanged(m)) => Some(Message::Modifiers(m)),
        iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
            Some(Message::PointerUp)
        }
        // Keys a text field used are left alone.
        iced::Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
            if status == Status::Ignored =>
        {
            match key.as_ref() {
                keyboard::Key::Character("a") if modifiers.command() => Some(Message::SelectAll),
                keyboard::Key::Character("i") if modifiers.command() => {
                    Some(Message::ToggleDetails)
                }
                keyboard::Key::Named(Named::Delete) => Some(Message::DeleteKey),
                keyboard::Key::Named(Named::Escape) => Some(Message::Escape),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Use the language chosen in `settings`, or the system's.
pub fn apply_language(settings: &Settings) {
    i18n::set_language(settings.language.unwrap_or_else(Lang::system));
}

fn export_failed(path: &std::path::Path, err: &std::io::Error) -> String {
    t!("errors.export_failed", path = path.display(), error = err)
}

/// Keep the end of `s` (the informative part of a path or URL), at most `max` characters.
fn shorten(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_owned();
    }
    let tail: String = s.chars().skip(count - (max - 1)).collect();
    format!("\u{2026}{tail}")
}

fn looks_like_url(s: &str) -> bool {
    (s.starts_with("http://") || s.starts_with("https://")) && !s.contains(char::is_whitespace)
}

/// Show `path` in the system file manager (selected, if the file exists).
fn reveal(path: &std::path::Path, file_exists: bool) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(path);
    let mut cmd;
    if cfg!(windows) {
        cmd = std::process::Command::new("explorer");
        if file_exists {
            cmd.arg(format!("/select,{}", path.display()));
        } else {
            cmd.arg(dir);
        }
    } else if cfg!(target_os = "macos") {
        cmd = std::process::Command::new("open");
        if file_exists {
            cmd.arg("-R").arg(path);
        } else {
            cmd.arg(dir);
        }
    } else {
        cmd = std::process::Command::new("xdg-open");
        cmd.arg(dir);
    }
    cmd.spawn().map(|_| ())
}

fn save_png(path: &std::path::Path, shot: &window::Screenshot) -> anyhow::Result<()> {
    let file = std::io::BufWriter::new(std::fs::File::create(path)?);
    let mut enc = png::Encoder::new(file, shot.size.width, shot.size.height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&shot.rgba)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortening_keeps_the_end() {
        assert_eq!(shorten("abc", 5), "abc");
        assert_eq!(shorten("abcdefgh", 5), "\u{2026}efgh");
        assert_eq!(shorten("ééééééé", 4).chars().count(), 4);
    }

    #[test]
    fn links_from_text_files() {
        let text = "https://a.example/x.zip\r\n\n# comment\nftp://b.example/y.iso, \"https://c.example/z\"\nnot a link\nhttps://a.example/x.zip <https://d.example/w>";
        assert_eq!(
            links_in(text),
            [
                "https://a.example/x.zip",
                "ftp://b.example/y.iso",
                "https://c.example/z",
                "https://d.example/w"
            ]
        );
        assert!(links_in("nothing here").is_empty());
    }

    #[test]
    fn url_detection() {
        assert!(looks_like_url("https://example.com/a.zip"));
        assert!(looks_like_url("http://x"));
        assert!(!looks_like_url("ftp://x"));
        assert!(!looks_like_url("https://x y"));
        assert!(!looks_like_url("hello"));
    }
}
