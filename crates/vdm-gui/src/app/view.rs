//! What the window shows: sidebar, toolbar, download list, banners and dialogs.

use iced::widget::{
    button, center, checkbox, column, container, mouse_area, opaque, pick_list, progress_bar,
    responsive, row, scrollable, space, stack, svg, text, text_input, tooltip,
};
use iced::{Alignment, Color, Element, Font, Length, Padding, Theme, color, font, mouse};
use vdm_proto::{DownloadState, Queue, QueueId, WhenDone};

use super::{
    App, BrowseFor, COLUMN_GAP, ColumnDrag, Dialog, HeaderMenu, LoginForm, Message,
    SETTINGS_SCROLL, UpdateState, shorten,
};
use crate::categories::Category;
use crate::columns::{Column, text_width};
use crate::i18n::{Lang, t};
use crate::icons::{self, Icon, icon};
use crate::model::{self, Row};
use crate::queues;
use crate::theme::{self, ThemeChoice, Tokens, tokens};
use crate::update;

mod details;

pub(super) use details::DETAILS_SCROLL;

fn semibold() -> Font {
    Font { weight: font::Weight::Semibold, ..crate::i18n::ui_font() }
}

fn bold() -> Font {
    Font { weight: font::Weight::Bold, ..crate::i18n::ui_font() }
}

/// The sidebar's width.
const SIDEBAR_WIDTH: f32 = 236.0;
/// Below this width of the area right of the sidebar, the toolbar's "Add
/// download" button shows just its icon.
const COMPACT_BELOW: f32 = 720.0;
/// Width of the trailing cell that holds a row's "Show in folder" button.
const ACTIONS_WIDTH: f32 = 34.0;
/// A row's horizontal padding plus its border, on each side.
const ROW_INSET: f32 = 15.0;
/// Space the row list keeps clear for its scrollbar.
const SCROLL_GAP: f32 = 12.0;

/// Which downloads the list shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Filter {
    #[default]
    All,
    Active,
    Waiting,
    Completed,
    Failed,
    Category(Category),
    Queue(QueueId),
}

impl Filter {
    const LIBRARY: [Filter; 5] =
        [Filter::All, Filter::Active, Filter::Waiting, Filter::Completed, Filter::Failed];

    pub(super) fn matches(self, r: &Row) -> bool {
        use DownloadState as S;
        match self {
            Filter::All => true,
            Filter::Active => matches!(r.state, S::Downloading | S::Probing | S::Verifying),
            Filter::Waiting => matches!(r.state, S::Queued | S::Paused),
            Filter::Completed => r.state == S::Completed,
            Filter::Failed => r.state == S::Failed,
            Filter::Category(c) => Category::from_name(&r.file_name) == c,
            Filter::Queue(q) => r.queue == q,
        }
    }

    fn label(self) -> String {
        match self {
            Filter::All => t!("nav.all"),
            Filter::Active => t!("nav.active"),
            Filter::Waiting => t!("nav.waiting"),
            Filter::Completed => t!("nav.completed"),
            Filter::Failed => t!("nav.failed"),
            Filter::Category(c) => c.label(),
            // Named by the app, which knows the queue (see `App::filter_label`).
            Filter::Queue(_) => t!("queues.section"),
        }
    }

    fn icon(self) -> Icon {
        match self {
            Filter::All => Icon::Download,
            Filter::Active => Icon::Bolt,
            Filter::Waiting => Icon::Clock,
            Filter::Completed => Icon::Check,
            Filter::Failed => Icon::Alert,
            Filter::Category(c) => category_icon(c),
            Filter::Queue(_) => Icon::Queue,
        }
    }
}

impl App {
    /// The Vibe theme in the chosen variant; Settings previews its choice.
    pub(super) fn theme(&self) -> Theme {
        let choice = match &self.dialog {
            Some(Dialog::Settings { form, .. }) => form.theme,
            _ => self.settings.theme,
        };
        theme::vibe(choice.is_dark(self.system_mode))
    }

    pub(super) fn view(&self) -> Element<'_, Message> {
        let page: Element<'_, Message> = if !self.settings.details_panel {
            row![self.sidebar(), self.main_area()].into()
        } else if self.details_beside() {
            row![self.sidebar(), self.main_area(), self.details_panel()].into()
        } else {
            // In a narrow window the panel floats over the list instead.
            let panel = container(self.details_panel()).style(theme::floating_edge);
            let floating = container(opaque(panel))
                .width(Length::Fill)
                .height(Length::Fill)
                .align_x(Alignment::End);
            row![self.sidebar(), stack![self.main_area(), floating]].into()
        };
        match &self.dialog {
            None => page,
            Some(dialog) => {
                let width = if matches!(dialog, Dialog::Settings { .. }) { 620.0 } else { 540.0 };
                stack![page, modal(self.dialog_view(dialog), width)].into()
            }
        }
    }

    /// The details panel sits beside the list (else it floats over it).
    fn details_beside(&self) -> bool {
        self.window_width >= details::BESIDE_FROM
    }

    /// Width of the list area: the window less the sidebar and a panel beside it.
    fn main_width(&self) -> f32 {
        let panel =
            if self.settings.details_panel && self.details_beside() { details::WIDTH } else { 0.0 };
        self.window_width - SIDEBAR_WIDTH - panel
    }

    /// A filter's name; a queue's comes from the queue list.
    fn filter_label(&self, f: Filter) -> String {
        match f {
            Filter::Queue(id) => {
                self.queues.iter().find(|q| q.id == id).map_or_else(|| f.label(), queues::name)
            }
            other => other.label(),
        }
    }

    /// Rows that pass the filter and the search box.
    pub(super) fn visible_rows(&self) -> Vec<&Row> {
        let query = self.search.trim().to_lowercase();
        self.downloads
            .rows()
            .iter()
            .filter(|r| self.filter.matches(r))
            .filter(|r| {
                query.is_empty()
                    || r.file_name.to_lowercase().contains(&query)
                    || r.url.to_lowercase().contains(&query)
            })
            .collect()
    }

    // ------------------------------------------------------------ sidebar

    fn sidebar(&self) -> Element<'_, Message> {
        let rows = self.downloads.rows();
        let count = |f: Filter| rows.iter().filter(|r| f.matches(r)).count();

        let brand = row![
            icons::logo(38.0),
            column![
                text("Vibe").size(19).font(bold()),
                text("Download Manager").size(12).style(theme::muted),
            ],
        ]
        .spacing(10)
        .align_y(Alignment::Center);

        let mut nav = column![section_label(t!("nav.library"))].spacing(2);
        for f in Filter::LIBRARY {
            nav = nav.push(self.nav_item(f, count(f)));
        }
        nav = nav.push(space().height(14)).push(section_label(t!("queues.section")));
        for queue in &self.queues {
            nav = nav.push(self.queue_item(queue));
        }
        nav = nav.push(
            button(
                row![
                    icon(Icon::Plus, 15.0).style(theme::icon_in(|t| t.muted)),
                    text(t!("queues.new")).size(13).style(theme::muted),
                ]
                .spacing(10)
                .align_y(Alignment::Center),
            )
            .padding([6, 11])
            .width(Length::Fill)
            .on_press(Message::NewQueue)
            .style(theme::nav(false)),
        );
        nav = nav.push(space().height(14)).push(section_label(t!("nav.categories")));
        for c in Category::ALL {
            let f = Filter::Category(c);
            nav = nav.push(self.nav_item(f, count(f)));
        }

        let content = column![
            container(brand).padding([2, 6]),
            space().height(14),
            scrollable(nav).height(Length::Fill).direction(thin()).style(theme::scrollbar),
            self.speed_card(),
            nav_button(Icon::Settings, t!("nav.settings"), None, false, Message::OpenSettings),
        ]
        .spacing(8);
        container(content)
            .width(SIDEBAR_WIDTH)
            .height(Length::Fill)
            .padding([18, 14])
            .style(theme::sidebar)
            .into()
    }

    fn nav_item(&self, f: Filter, count: usize) -> Element<'_, Message> {
        nav_button(
            f.icon(),
            self.filter_label(f),
            Some(count),
            self.filter == f,
            Message::SetFilter(f),
        )
    }

    /// A queue in the sidebar: its icon shows whether it's stopped or scheduled.
    fn queue_item(&self, queue: &Queue) -> Element<'_, Message> {
        let f = Filter::Queue(queue.id);
        let icon = if queue.stopped {
            Icon::Pause
        } else if queue.spec.schedule.is_some() {
            Icon::Clock
        } else {
            Icon::Queue
        };
        let count = self.downloads.unfinished_in(queue.id);
        nav_button(icon, queues::name(queue), Some(count), self.filter == f, Message::SetFilter(f))
    }

    fn speed_card(&self) -> Element<'_, Message> {
        let d = &self.downloads;
        let speed = model::fmt_speed(d.total_speed());
        let running = d.count(DownloadState::Downloading);
        let headline = match (speed.is_empty(), running) {
            (false, _) => speed,
            (true, 0) => t!("speed.idle"),
            (true, _) => t!("speed.connecting"),
        };
        let running = t!("speed.running", count = running);
        let limit =
            self.settings.speed_limit.map(|l| t!("speed.limit", speed = model::fmt_bytes(l)));
        let time_left =
            d.total_eta_secs().map(|secs| t!("speed.time_left", time = model::fmt_eta(secs)));
        container(
            column![
                row![
                    icon(Icon::Bolt, 14.0).style(theme::icon_in(|t| t.muted)),
                    text(t!("speed.title")).size(12).style(theme::muted),
                ]
                .spacing(6)
                .align_y(Alignment::Center),
                text(headline).size(21).font(bold()),
                text(running).size(12).style(theme::muted),
            ]
            .push(time_left.map(|l| text(l).size(12).style(theme::muted)))
            .push(limit.map(|l| text(l).size(12).style(theme::muted)))
            .spacing(4),
        )
        .padding(12)
        .width(Length::Fill)
        .style(theme::card)
        .into()
    }

    // ------------------------------------------------------------ main area

    fn main_area(&self) -> Element<'_, Message> {
        let mut page = column![self.toolbar()].spacing(14).padding([18, 22]);
        if let Some(power) = &self.power {
            page = page.push(power_banner(power));
        }
        if let Some(update) = self.update_banner() {
            page = page.push(update);
        }
        if let Some(message) = &self.notice {
            page = page.push(notice(Icon::Sparkle, message, |t| t.primary, Message::DismissNotice));
        }
        if let Some(err) = &self.error {
            page = page.push(notice(Icon::Alert, err, |t| t.danger, Message::DismissError));
        }
        page = page.push(self.list_title()).push(self.list()).push(self.status_bar());
        let page = container(page).width(Length::Fill).height(Length::Fill);
        if self.move_menu { stack![page, self.move_menu_view()].into() } else { page.into() }
    }

    fn toolbar(&self) -> Element<'_, Message> {
        let selection = self.downloads.selection();
        let can = |f: fn(&Row) -> bool| selection.iter().any(|r| f(r));
        let any = !selection.is_empty();
        let search = container(
            row![
                icon(Icon::Search, 16.0).style(theme::icon_in(|t| t.muted)),
                text_input(&t!("toolbar.search"), &self.search)
                    .on_input(Message::Search)
                    .padding([9, 0])
                    .size(14)
                    .style(theme::bare_input),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .padding([0, 12])
        .width(Length::FillPortion(6))
        .max_width(320.0)
        .style(theme::search_box);

        let tools = row![
            tool(
                Icon::Play,
                t!("toolbar.resume"),
                can(Row::can_resume).then_some(Message::Resume),
                false
            ),
            tool(
                Icon::Pause,
                t!("toolbar.pause"),
                can(Row::can_pause).then_some(Message::Pause),
                false
            ),
            tool(
                Icon::Folder,
                t!("toolbar.show_in_folder"),
                any.then_some(Message::OpenFolder),
                false
            ),
            tool(
                Icon::Queue,
                t!("queues.move"),
                (any && self.queues.len() > 1).then_some(Message::ToggleMoveMenu),
                false
            ),
            tool(Icon::Close, t!("toolbar.remove"), any.then_some(Message::Remove), false),
            tool(Icon::Trash, t!("toolbar.delete"), any.then_some(Message::AskDelete), true),
            tool(Icon::Info, t!("details.toggle"), Some(Message::ToggleDetails), false),
        ]
        .spacing(2);

        row![
            search,
            space().width(Length::FillPortion(1)),
            container(tools).padding(3).style(theme::card),
            add_button(self.main_width() < COMPACT_BELOW),
        ]
        .spacing(12)
        .align_y(Alignment::Center)
        .into()
    }

    fn list_title(&self) -> Element<'_, Message> {
        let shown = self.visible_rows().len();
        let mut title = row![
            text(self.filter_label(self.filter)).size(22).font(bold()),
            container(text(shown.to_string()).size(12).font(semibold()))
                .padding([2, 9])
                .style(theme::pill_in(|t| t.muted)),
        ]
        .spacing(10)
        .align_y(Alignment::Center);
        let Filter::Queue(id) = self.filter else { return title.into() };
        let Some(queue) = self.queues.iter().find(|q| q.id == id) else { return title.into() };
        let toggle = if queue.stopped {
            small_icon_button(Icon::Play, t!("queues.start"), Message::SetQueueStopped(id, false))
        } else {
            small_icon_button(Icon::Pause, t!("queues.stop"), Message::SetQueueStopped(id, true))
        };
        title = title.push(space().width(Length::Fill)).push(toggle).push(small_icon_button(
            Icon::Settings,
            t!("queues.edit"),
            Message::EditQueue(id),
        ));
        column![title, text(queues::summary(queue)).size(13).style(theme::muted)].spacing(4).into()
    }

    /// The toolbar's "Move to queue" menu.
    fn move_menu_view(&self) -> Element<'_, Message> {
        let mut items = column![].spacing(1);
        for queue in &self.queues {
            items = items.push(menu_item(
                queues::name(queue),
                None,
                Some(Message::MoveSelection(queue.id)),
            ));
        }
        let items = items.push(menu_divider()).push(menu_item(
            t!("queues.new"),
            None,
            Some(Message::NewQueue),
        ));
        let card = container(items).padding(5).width(240).style(theme::dialog);
        let backdrop = mouse_area(container(space()).width(Length::Fill).height(Length::Fill))
            .on_press(Message::ToggleMoveMenu)
            .on_right_press(Message::ToggleMoveMenu);
        stack![
            backdrop,
            container(opaque(card)).width(Length::Fill).align_x(Alignment::End).padding(Padding {
                top: 66.0,
                right: 200.0,
                ..Padding::ZERO
            }),
        ]
        .into()
    }

    fn list(&self) -> Element<'_, Message> {
        if self.visible_rows().is_empty() {
            return self.empty_state();
        }
        responsive(move |size| self.table(size.width)).into()
    }

    /// Header and rows, laid out for a list `width` pixels wide.
    fn table(&self, width: f32) -> Element<'_, Message> {
        let columns = &self.settings.columns;
        let shown = columns.visible().count() as f32;
        let available = width - SCROLL_GAP - 2.0 * ROW_INSET - COLUMN_GAP * shown - ACTIONS_WIDTH;
        let layout = columns.layout(available);
        let items = self
            .visible_rows()
            .into_iter()
            .map(|r| list_row(r, self.downloads.is_selected(r.id), &layout));
        let table = column![
            self.header(&layout, available),
            scrollable(
                column(items).spacing(6).padding(Padding { right: SCROLL_GAP, ..Padding::ZERO })
            )
            .height(Length::Fill)
            .direction(thin())
            .style(theme::scrollbar),
        ]
        .spacing(8)
        .height(Length::Fill);
        match self.header_menu {
            None => table.into(),
            Some(menu) => stack![table, self.header_menu_view(menu, width)].into(),
        }
    }

    /// Column titles: drag a title to move the column, its right edge to
    /// resize it, right-click for the column menu.
    fn header(&self, layout: &[(Column, f32)], available: f32) -> Element<'_, Message> {
        let moving = match self.drag {
            Some(ColumnDrag::Move { column, moved: true, target, .. }) => Some((column, target)),
            _ => None,
        };
        let mut cells = row![].spacing(COLUMN_GAP).align_y(Alignment::Center);
        for (index, &(column, width)) in layout.iter().enumerate() {
            let dragged = moving.is_some_and(|(c, _)| c == column);
            let target = moving.is_some_and(|(_, t)| t == index);
            let title = text(column_title(column).to_uppercase())
                .size(11)
                .font(semibold())
                .wrapping(text::Wrapping::None)
                .style(move |theme: &Theme| {
                    let t = tokens(theme);
                    text::Style { color: Some(if dragged { t.primary } else { t.muted }) }
                });
            let label = mouse_area(container(title).width(Length::Fill).padding([5, 0]))
                .on_press(Message::HeaderPress(column))
                .interaction(mouse::Interaction::Grab);
            let handle = mouse_area(
                container(container(space()).width(1).height(14).style(theme::divider))
                    .center_x(10)
                    .height(24),
            )
            .on_press(Message::ResizeStart(column))
            .interaction(mouse::Interaction::ResizingHorizontally);
            cells = cells.push(
                container(row![label, handle].align_y(Alignment::Center))
                    .width(width)
                    .clip(true)
                    .style(move |theme: &Theme| theme::drop_target(theme, target)),
            );
        }
        cells = cells.push(space().width(ACTIONS_WIDTH));
        let header = container(cells).padding(Padding {
            top: 0.0,
            right: ROW_INSET + SCROLL_GAP,
            bottom: 0.0,
            left: ROW_INSET,
        });
        mouse_area(header)
            .on_move(move |p| Message::HeaderMove { x: p.x - ROW_INSET, available })
            .on_right_press(Message::HeaderMenu)
            .into()
    }

    /// The column menu, opened by right-clicking the header.
    fn header_menu_view(&self, menu: HeaderMenu, width: f32) -> Element<'_, Message> {
        const MENU_WIDTH: f32 = 260.0;
        let columns = &self.settings.columns;
        let order: Vec<Column> = columns.visible().map(|s| s.column).collect();
        let pos = order.iter().position(|c| *c == menu.column).unwrap_or(0);
        let column = menu.column;
        let mut items = column![
            menu_item(t!("columns.fit"), Some(columns.fit), Some(Message::ColumnFit)),
            menu_item(
                t!("columns.size_one", column = column_title(column)),
                None,
                Some(Message::ColumnSizeToContent(Some(column))),
            ),
            menu_item(t!("columns.size_all"), None, Some(Message::ColumnSizeToContent(None))),
            menu_item(
                t!("columns.move_left"),
                None,
                (pos > 0).then_some(Message::ColumnShift(column, -1)),
            ),
            menu_item(
                t!("columns.move_right"),
                None,
                (pos + 1 < order.len()).then_some(Message::ColumnShift(column, 1)),
            ),
            menu_divider(),
            container(
                text(t!("columns.show").to_uppercase())
                    .size(10)
                    .font(semibold())
                    .style(theme::muted)
            )
            .padding([4, 10]),
        ]
        .spacing(1);
        for state in columns.items.iter().filter(|s| s.column.can_hide()) {
            items = items.push(menu_item(
                column_title(state.column),
                Some(state.visible),
                Some(Message::ColumnToggle(state.column)),
            ));
        }
        items = items.push(menu_divider()).push(menu_item(
            t!("columns.reset"),
            None,
            Some(Message::ColumnReset),
        ));
        let card = container(items).padding(5).width(MENU_WIDTH).style(theme::dialog);
        let left = (menu.x + ROW_INSET).min(width - MENU_WIDTH - 8.0).max(0.0);
        // Any click outside the menu closes it.
        let backdrop = mouse_area(container(space()).width(Length::Fill).height(Length::Fill))
            .on_press(Message::CloseMenu)
            .on_right_press(Message::CloseMenu);
        stack![
            backdrop,
            container(opaque(card)).padding(Padding { top: 30.0, left, ..Padding::ZERO }),
        ]
        .into()
    }

    /// About how wide `column` must be to show its title and every visible
    /// row's content, for "Size to content".
    pub(super) fn content_width(&self, column: Column) -> f32 {
        let title = text_width(&column_title(column).to_uppercase(), 11.0) + 14.0;
        let content = self
            .visible_rows()
            .iter()
            .map(|r| match column {
                Column::Name => {
                    let name = text_width(&ellipsize(&r.file_name, 40), 14.5);
                    50.0 + name.max(text_width(host_of(&r.url), 12.0))
                }
                Column::Size => {
                    let total = r.total.map(model::fmt_bytes).unwrap_or_default();
                    text_width(&total, 13.0)
                        .max(text_width(&t!("list.done", size = "999.9 MiB"), 11.0))
                }
                Column::Progress => 160.0,
                Column::Speed => text_width("999.9 MiB/s", 13.0),
                Column::TimeLeft => text_width("23h 59m", 13.0),
                Column::Status => text_width(&model::state_label(r.state), 12.5) + 22.0,
            })
            .fold(0.0, f32::max);
        title.max(content) + 6.0
    }
    fn empty_state(&self) -> Element<'_, Message> {
        if self.engine.is_none() {
            return center(text(t!("list.starting")).size(15).style(theme::muted)).into();
        }
        let query = self.search.trim();
        let (title, hint, offer_add) = if !query.is_empty() {
            (t!("list.no_matches"), t!("list.no_matches_hint", query = query), false)
        } else if self.downloads.rows().is_empty() {
            (t!("list.empty_title"), t!("list.empty_hint"), true)
        } else {
            (
                t!("list.nothing_here"),
                t!("list.nothing_here_hint", filter = self.filter_label(self.filter)),
                false,
            )
        };
        let mut content = column![
            icons::logo(72.0),
            space().height(6),
            text(title).size(21).font(bold()),
            text(hint).size(14).style(theme::muted),
        ]
        .spacing(8)
        .align_x(Alignment::Center);
        if offer_add {
            content = content.push(space().height(8)).push(add_button(false));
        }
        center(content).height(Length::Fill).into()
    }

    fn status_bar(&self) -> Element<'_, Message> {
        let d = &self.downloads;
        let summary = t!(
            "statusbar.summary",
            total = d.rows().len(),
            running = d.count(DownloadState::Downloading),
            queued = d.count(DownloadState::Queued),
            done = d.count(DownloadState::Completed),
        );
        let left = text(summary).size(12).style(theme::muted);
        let selection = self.downloads.selection();
        let right = match self.downloads.selected() {
            None if selection.len() > 1 => {
                let size: u64 = selection.iter().filter_map(|r| r.total).sum();
                text(t!(
                    "statusbar.selected",
                    count = selection.len(),
                    size = model::fmt_bytes(size)
                ))
                .size(12)
                .style(theme::muted)
            }
            Some(r) if r.error.is_some() => {
                text(format!("{}: {}", r.file_name, r.error.as_deref().unwrap_or_default()))
                    .size(12)
                    .style(theme::tone_text(|t| t.danger))
            }
            Some(r) => text(shorten(&r.url, 70)).size(12).style(theme::muted),
            None => {
                let dir = self.settings.download_dir.display().to_string();
                text(t!("statusbar.saving_to", folder = shorten(&dir, 50)))
                    .size(12)
                    .style(theme::muted)
            }
        };
        let right = container(right.wrapping(text::Wrapping::None))
            .width(Length::Fill)
            .align_x(Alignment::End)
            .clip(true);
        row![left.wrapping(text::Wrapping::None), right].spacing(12).into()
    }

    /// "Vibe x.y.z is available" with its buttons, or the download in progress.
    fn update_banner(&self) -> Option<Element<'_, Message>> {
        let content: Element<'_, Message> = match &self.update {
            UpdateState::Idle => return None,
            UpdateState::Available(available) => {
                let notes = available
                    .notes
                    .as_deref()
                    .and_then(|n| n.lines().find(|l| !l.trim().is_empty()))
                    .unwrap_or_default();
                row![
                    icon(Icon::Sparkle, 20.0).style(theme::icon_in(|t| t.accent)),
                    column![
                        text(t!("update.available", version = available.version))
                            .size(14)
                            .font(semibold()),
                        text(shorten(notes, 90)).size(12).style(theme::muted),
                    ]
                    .spacing(2)
                    .width(Length::Fill),
                    button(text(t!("update.later")).size(13))
                        .padding([6, 12])
                        .on_press(Message::DismissUpdate)
                        .style(theme::ghost),
                    button(text(t!("update.skip")).size(13))
                        .padding([6, 12])
                        .on_press(Message::SkipUpdate)
                        .style(theme::ghost),
                    button(text(t!("update.now")).size(13).font(semibold()))
                        .padding([7, 14])
                        .on_press(Message::InstallUpdate)
                        .style(theme::primary),
                ]
                .spacing(10)
                .align_y(Alignment::Center)
                .into()
            }
            UpdateState::Downloading(available) => row![
                icon(Icon::Download, 18.0).style(theme::icon_in(|t| t.primary)),
                text(t!("update.downloading", version = available.version)).size(13),
            ]
            .spacing(10)
            .align_y(Alignment::Center)
            .into(),
        };
        Some(
            container(content)
                .padding([10, 14])
                .width(Length::Fill)
                .style(theme::banner(|t| t.primary))
                .into(),
        )
    }

    // ------------------------------------------------------------ dialogs

    fn dialog_view<'a>(&'a self, dialog: &'a Dialog) -> Element<'a, Message> {
        match dialog {
            Dialog::Add(f) => {
                let submit = (!f.busy).then_some(Message::SubmitAdd);
                let mut body = column![
                    dialog_header(Icon::Download, None, t!("add.title"), t!("add.subtitle"),),
                    field(
                        t!("add.address"),
                        input("https://example.com/file.zip", &f.url)
                            .on_input(Message::AddUrl)
                            .on_submit_maybe(submit.clone()),
                    ),
                    field(
                        t!("add.save_to"),
                        with_browse(
                            input(&t!("add.folder"), &f.folder).on_input(Message::AddFolder),
                            BrowseFor::AddFolder,
                        ),
                    ),
                    row![
                        container(field(
                            t!("add.file_name"),
                            input(&t!("add.file_name_placeholder"), &f.name)
                                .on_input(Message::AddName),
                        ))
                        .width(Length::FillPortion(3)),
                        container(field(
                            t!("add.connections"),
                            input("8", &f.connections).on_input(Message::AddConnections),
                        ))
                        .width(Length::FillPortion(1)),
                    ]
                    .spacing(12)
                    .align_y(Alignment::End),
                    field(
                        t!("add.checksum"),
                        input(&t!("add.checksum_placeholder"), &f.checksum)
                            .on_input(Message::AddChecksum)
                            .on_submit_maybe(submit.clone()),
                    ),
                ]
                .spacing(16);
                if self.queues.len() > 1 {
                    body = body.push(field(t!("add.queue"), self.queue_picker(f.queue)));
                }
                if let Some(err) = &f.error {
                    body = body.push(error_text(err.clone()));
                }
                body.push(buttons(
                    secondary_button(t!("common.cancel"), Some(Message::CloseDialog)),
                    primary_button(
                        if f.busy { t!("add.adding") } else { t!("add.download") },
                        submit,
                    ),
                ))
                .into()
            }
            Dialog::Settings { form, login, error } => {
                let body = column![
                    section(t!("settings.section_downloads")),
                    field(
                        t!("settings.download_folder"),
                        with_browse(
                            input(&t!("add.folder"), &form.download_dir)
                                .on_input(Message::SetDownloadDir),
                            BrowseFor::DownloadDir,
                        ),
                    ),
                    row![
                        container(field(
                            t!("settings.at_once"),
                            input("3", &form.max_active).on_input(Message::SetMaxActive),
                        ))
                        .width(Length::FillPortion(1)),
                        container(field(
                            t!("settings.connections_each"),
                            input("8", &form.connections).on_input(Message::SetConnections),
                        ))
                        .width(Length::FillPortion(1)),
                        container(field(
                            t!("settings.speed_limit"),
                            input(&t!("settings.speed_limit_placeholder"), &form.speed_limit)
                                .on_input(Message::SetSpeedLimit)
                                .on_submit(Message::SaveSettings),
                        ))
                        .width(Length::FillPortion(2)),
                    ]
                    .spacing(12)
                    .align_y(Alignment::End),
                    section(t!("settings.section_appearance")),
                    setting_row(
                        t!("settings.theme"),
                        t!("settings.theme_hint"),
                        theme_picker(form.theme)
                    ),
                    setting_row(
                        t!("settings.language"),
                        t!("settings.language_hint"),
                        language_picker(form.language),
                    ),
                    section(t!("settings.section_behavior")),
                    toggle(
                        form.use_categories,
                        t!("settings.sort_categories"),
                        Message::ToggleCategories,
                    ),
                    toggle(
                        form.watch_clipboard,
                        t!("settings.watch_clipboard"),
                        Message::ToggleWatchClipboard,
                    ),
                    toggle(form.notify, t!("settings.notify"), Message::ToggleNotify),
                    toggle(
                        form.close_to_tray,
                        t!("settings.close_to_tray"),
                        Message::ToggleCloseToTray,
                    ),
                    toggle(
                        form.launch_at_startup,
                        t!("settings.launch_at_startup"),
                        Message::ToggleAutostart,
                    ),
                    section(t!("settings.section_network")),
                    field(
                        t!("settings.proxy"),
                        input(&t!("settings.proxy_placeholder"), &form.proxy)
                            .on_input(Message::SetProxy),
                    ),
                    field(
                        t!("settings.after_download"),
                        input(&t!("settings.after_download_placeholder"), &form.after_download)
                            .on_input(Message::SetAfterDownload),
                    ),
                    section(t!("settings.section_logins")),
                    self.logins_view(login),
                    section(t!("settings.section_updates")),
                    row![
                        toggle(
                            form.check_updates,
                            t!("settings.check_updates"),
                            Message::ToggleCheckUpdates
                        ),
                        space().width(Length::Fill),
                        small_button(
                            t!("settings.check_now"),
                            Message::CheckUpdate { manual: true }
                        ),
                    ]
                    .align_y(Alignment::Center),
                    section(t!("settings.section_list")),
                    row![
                        small_button(t!("settings.import"), Message::ImportLinks),
                        small_button(t!("settings.export"), Message::ExportList),
                    ]
                    .spacing(8),
                ]
                .spacing(14)
                .padding(Padding { right: 16.0, ..Padding::ZERO });

                let mut page = column![
                    dialog_header(
                        Icon::Settings,
                        None,
                        t!("settings.title"),
                        format!("Vibe Download Manager {}", update::current_version()),
                    ),
                    container(
                        scrollable(body)
                            .id(iced::widget::Id::new(SETTINGS_SCROLL))
                            .direction(thin())
                            .style(theme::scrollbar)
                    )
                    .max_height(500),
                ]
                .spacing(18);
                if let Some(err) = error {
                    page = page.push(error_text(err.clone()));
                }
                page.push(buttons(
                    secondary_button(t!("common.cancel"), Some(Message::CloseDialog)),
                    primary_button(t!("common.save"), Some(Message::SaveSettings)),
                ))
                .into()
            }
            Dialog::Queue { id, form, error } => self.queue_dialog(*id, form, error.as_deref()),
            Dialog::ConfirmDelete(ids) => {
                let question = match ids.as_slice() {
                    [id] => {
                        let name = self
                            .downloads
                            .get(*id)
                            .map_or_else(|| t!("delete.this_download"), |r| r.file_name.clone());
                        t!("delete.question", name = name)
                    }
                    _ => t!("delete.question_many", count = ids.len()),
                };
                column![
                    dialog_header(
                        Icon::Trash,
                        Some(|t: &Tokens| t.danger),
                        t!("delete.title"),
                        t!("delete.subtitle"),
                    ),
                    text(question).size(14),
                    buttons(
                        secondary_button(t!("common.cancel"), Some(Message::CloseDialog)),
                        button(text(t!("delete.confirm")).size(14).font(semibold()))
                            .padding([9, 18])
                            .on_press(Message::ConfirmDelete)
                            .style(theme::danger),
                    ),
                ]
                .spacing(18)
                .into()
            }
        }
    }

    fn queue_picker(&self, current: QueueId) -> Element<'_, Message> {
        let options: Vec<QueueChoice> =
            self.queues.iter().map(|q| QueueChoice(q.id, queues::name(q))).collect();
        let selected = options.iter().find(|c| c.0 == current).cloned();
        pick_list(options, selected, |c: QueueChoice| Message::AddQueue(c.0))
            .text_size(14)
            .padding([8, 12])
            .width(Length::Fill)
            .style(theme::pick)
            .menu_style(theme::pick_menu)
            .into()
    }

    /// The editor for a new or existing queue.
    fn queue_dialog<'a>(
        &'a self,
        id: Option<QueueId>,
        form: &'a queues::QueueForm,
        error: Option<&'a str>,
    ) -> Element<'a, Message> {
        let is_main = id == Some(QueueId::MAIN);
        let title = if id.is_some() { t!("queue.title_edit") } else { t!("queue.title_new") };
        let mut body =
            column![dialog_header(Icon::Queue, None, title, t!("queue.subtitle"))].spacing(16);
        if !is_main {
            body = body.push(field(
                t!("queue.name"),
                input(&t!("queue.name_placeholder"), &form.name).on_input(Message::QueueName),
            ));
        }
        body = body.push(
            row![
                container(field(
                    t!("queue.at_once"),
                    input(&t!("queue.at_once_placeholder"), &form.max_active)
                        .on_input(Message::QueueAtOnce),
                ))
                .width(Length::FillPortion(1)),
                container(field(
                    t!("settings.speed_limit"),
                    input(&t!("settings.speed_limit_placeholder"), &form.speed_limit)
                        .on_input(Message::QueueSpeed),
                ))
                .width(Length::FillPortion(2)),
            ]
            .spacing(12)
            .align_y(Alignment::End),
        );
        body = body.push(toggle(form.scheduled, t!("queue.scheduled"), Message::QueueScheduled));
        if form.scheduled {
            let names = queues::day_names();
            let days = row(names.into_iter().enumerate().map(|(i, name)| {
                button(text(name).size(13).font(semibold()))
                    .padding([6, 10])
                    .on_press(Message::QueueDay(i))
                    .style(theme::segment(form.days[i]))
                    .into()
            }))
            .spacing(2);
            body = body
                .push(
                    row![
                        container(field(
                            t!("queue.start"),
                            input(&t!("queue.time_placeholder"), &form.start)
                                .on_input(Message::QueueStart),
                        ))
                        .width(Length::FillPortion(1)),
                        container(field(
                            t!("queue.stop"),
                            input(&t!("queue.time_placeholder"), &form.stop)
                                .on_input(Message::QueueStop),
                        ))
                        .width(Length::FillPortion(1)),
                    ]
                    .spacing(12),
                )
                .push(field(
                    t!("queue.days"),
                    container(days).padding(3).style(theme::segment_track),
                ));
        }
        let when_options: Vec<WhenDoneChoice> =
            queues::WHEN_DONE.iter().map(|&w| WhenDoneChoice(w)).collect();
        body = body.push(field(
            t!("queue.when_done"),
            pick_list(when_options, Some(WhenDoneChoice(form.when_done)), |c: WhenDoneChoice| {
                Message::QueueWhenDone(c.0)
            })
            .text_size(14)
            .padding([8, 12])
            .width(Length::Fill)
            .style(theme::pick)
            .menu_style(theme::pick_menu),
        ));
        if let Some(err) = error {
            body = body.push(error_text(err.to_owned()));
        }
        let mut actions = row![].spacing(10).align_y(Alignment::Center);
        if id.is_some_and(|id| id != QueueId::MAIN) {
            actions = actions.push(
                tooltip(
                    button(text(t!("queue.delete")).size(14))
                        .padding([9, 14])
                        .on_press(Message::DeleteQueue)
                        .style(theme::danger),
                    container(text(t!("queue.delete_hint")).size(12))
                        .padding([4, 8])
                        .style(theme::tip),
                    tooltip::Position::Top,
                )
                .gap(4),
            );
        }
        actions = actions
            .push(space().width(Length::Fill))
            .push(secondary_button(t!("common.cancel"), Some(Message::CloseDialog)))
            .push(primary_button(t!("common.save"), Some(Message::SaveQueue)));
        body.push(actions).into()
    }

    /// Saved site logins and a row to add one.
    fn logins_view<'a>(&'a self, login: &'a LoginForm) -> Element<'a, Message> {
        let mut list =
            column![text(t!("settings.logins_hint")).size(12).style(theme::muted)].spacing(8);
        if self.secrets.is_none() {
            list = list.push(error_text(t!("settings.logins_unavailable")));
        }
        for (i, entry) in self.settings.logins.iter().enumerate() {
            list = list.push(
                container(
                    row![
                        icon(Icon::Check, 15.0).style(theme::icon_in(|t| t.success)),
                        text(&entry.site).size(13).font(semibold()),
                        text(&entry.username).size(13).style(theme::muted).width(Length::Fill),
                        button(text(t!("common.remove")).size(12))
                            .padding([4, 10])
                            .on_press(Message::RemoveLogin(i))
                            .style(theme::ghost),
                    ]
                    .spacing(10)
                    .align_y(Alignment::Center),
                )
                .padding([4, 4])
                .style(theme::raised),
            );
        }
        let can_add = self.secrets.is_some()
            && !login.site.trim().is_empty()
            && !login.username.trim().is_empty()
            && !login.password.is_empty();
        list.push(
            row![
                input(&t!("settings.site_placeholder"), &login.site)
                    .on_input(Message::LoginSite)
                    .width(Length::FillPortion(3)),
                input(&t!("settings.user"), &login.username)
                    .on_input(Message::LoginUser)
                    .width(Length::FillPortion(2)),
                input(&t!("settings.password"), &login.password)
                    .on_input(Message::LoginPassword)
                    .secure(true)
                    .width(Length::FillPortion(2)),
                secondary_button(t!("common.add"), can_add.then_some(Message::AddLogin)),
            ]
            .spacing(8)
            .align_y(Alignment::Center),
        )
        .into()
    }
}

// ---------------------------------------------------------------- pieces

fn list_row<'a>(r: &'a Row, selected: bool, layout: &[(Column, f32)]) -> Element<'a, Message> {
    let mut line = row![].spacing(COLUMN_GAP).align_y(Alignment::Center);
    for &(column, width) in layout {
        line = line.push(container(row_cell(r, column)).width(width).clip(true));
    }
    let reveal = tooltip(
        button(icon(Icon::Folder, 16.0).style(theme::icon_in(|t| t.muted)))
            .padding(7)
            .on_press(Message::RevealRow(r.id))
            .style(theme::ghost),
        container(text(t!("toolbar.show_in_folder")).size(12)).padding([4, 8]).style(theme::tip),
        tooltip::Position::Left,
    )
    .gap(4);
    line = line.push(container(reveal).width(ACTIONS_WIDTH).align_x(Alignment::End));
    button(line)
        .padding([10, 14])
        .width(Length::Fill)
        .on_press(Message::RowClick(r.id))
        .style(theme::row(selected))
        .into()
}

/// One cell of a download row.
fn row_cell(r: &Row, column: Column) -> Element<'_, Message> {
    match column {
        Column::Name => {
            let category = Category::from_name(&r.file_name);
            let badge =
                container(icon(category_icon(category), 18.0).style(move |theme: &Theme, _| {
                    svg::Style { color: Some(category_tone(category, &tokens(theme))) }
                }))
                .center(38)
                .style(move |theme: &Theme| {
                    theme::badge(category_tone(category, &tokens(theme)))(theme)
                });
            let subtitle = match (&r.error, r.state) {
                (Some(err), DownloadState::Failed) => {
                    text(shorten(err, 70)).size(12).style(theme::tone_text(|t| t.danger))
                }
                _ => text(host_of(&r.url)).size(12).style(theme::muted),
            };
            row![
                badge,
                column![
                    text(ellipsize(&r.file_name, 40))
                        .size(14)
                        .font(semibold())
                        .wrapping(text::Wrapping::None),
                    subtitle.wrapping(text::Wrapping::None),
                ]
                .spacing(2),
            ]
            .spacing(12)
            .align_y(Alignment::Center)
            .into()
        }
        Column::Size => {
            // Total on top; while partway, how much is done under it.
            let (size, done) = match r.total {
                Some(t) if r.downloaded == 0 || r.downloaded >= t => (model::fmt_bytes(t), None),
                Some(t) => (
                    model::fmt_bytes(t),
                    Some(t!("list.done", size = model::fmt_bytes(r.downloaded))),
                ),
                None if r.downloaded > 0 => (model::fmt_bytes(r.downloaded), None),
                None => ("\u{2014}".to_owned(), None),
            };
            column![text(size).size(13).wrapping(text::Wrapping::None)]
                .push(
                    done.map(|d| {
                        text(d).size(11).style(theme::muted).wrapping(text::Wrapping::None)
                    }),
                )
                .spacing(1)
                .into()
        }
        Column::Progress => match r.progress() {
            Some(p) => row![
                progress_bar(0.0..=1.0, p).girth(6).style(progress_style(r.state)),
                text(format!("{:.0}%", p * 100.0))
                    .size(12)
                    .style(theme::muted)
                    .width(36)
                    .align_x(Alignment::End),
            ]
            .spacing(8)
            .align_y(Alignment::Center)
            .into(),
            None => text("\u{2014}").size(13).style(theme::muted).into(),
        },
        Column::Speed => {
            text(model::fmt_speed(r.bytes_per_sec)).size(13).wrapping(text::Wrapping::None).into()
        }
        Column::TimeLeft => text(r.eta_secs().map(model::fmt_eta).unwrap_or_default())
            .size(13)
            .style(theme::muted)
            .wrapping(text::Wrapping::None)
            .into(),
        Column::Status => {
            let tone_of = state_tone(r.state);
            container(
                text(model::state_label(r.state))
                    .size(12)
                    .font(semibold())
                    .wrapping(text::Wrapping::None),
            )
            .padding([3, 10])
            .style(move |theme: &Theme| theme::pill(tone_of(&tokens(theme)))(theme))
            .into()
        }
    }
}

/// A column's title.
fn column_title(column: Column) -> String {
    match column {
        Column::Name => t!("list.name"),
        Column::Size => t!("list.size"),
        Column::Progress => t!("list.progress"),
        Column::Speed => t!("list.speed"),
        Column::TimeLeft => t!("list.time_left"),
        Column::Status => t!("list.status"),
    }
}

/// An entry of a pop-up menu, with a check mark when `checked` is `Some(true)`.
fn menu_item<'a>(
    label: String,
    checked: Option<bool>,
    message: Option<Message>,
) -> Element<'a, Message> {
    let enabled = message.is_some();
    let mark: Element<'a, Message> = if checked == Some(true) {
        icon(Icon::Check, 14.0).style(theme::icon_in(|t| t.primary)).into()
    } else {
        space().width(14).into()
    };
    button(
        row![
            mark,
            text(label).size(13).style(move |theme: &Theme| {
                let t = tokens(theme);
                text::Style { color: Some(if enabled { t.text } else { t.muted.scale_alpha(0.6) }) }
            }),
        ]
        .spacing(8)
        .align_y(Alignment::Center),
    )
    .width(Length::Fill)
    .padding([6, 10])
    .on_press_maybe(message)
    .style(theme::ghost)
    .into()
}

fn menu_divider<'a>() -> Element<'a, Message> {
    container(container(space()).width(Length::Fill).height(1).style(theme::divider))
        .padding([4, 6])
        .into()
}

fn progress_style(state: DownloadState) -> impl Fn(&Theme) -> iced::widget::progress_bar::Style {
    move |theme| match state {
        DownloadState::Completed => theme::progress_in(|t| t.success)(theme),
        DownloadState::Failed => theme::progress_in(|t| t.danger)(theme),
        DownloadState::Paused => theme::progress_in(|t| t.warning)(theme),
        DownloadState::Queued => theme::progress_in(|t| t.muted)(theme),
        _ => theme::progress(theme),
    }
}

fn state_tone(state: DownloadState) -> fn(&Tokens) -> Color {
    match state {
        DownloadState::Downloading | DownloadState::Probing | DownloadState::Verifying => {
            |t| t.primary
        }
        DownloadState::Queued => |t| t.muted,
        DownloadState::Paused => |t| t.warning,
        DownloadState::Completed => |t| t.success,
        DownloadState::Failed => |t| t.danger,
    }
}

/// A category's color, toned down in the dark theme.
fn category_tone(c: Category, t: &Tokens) -> Color {
    let base = category_color(c);
    if t.is_dark { theme::mix(base, t.text, 0.3) } else { base }
}

fn category_color(c: Category) -> Color {
    match c {
        Category::Video => color!(0xec4899),
        Category::Music => color!(0xf59e0b),
        Category::Documents => color!(0x3b82f6),
        Category::Compressed => color!(0x8b5cf6),
        Category::Programs => color!(0x10b981),
        Category::Other => color!(0x94a3b8),
    }
}

fn category_icon(c: Category) -> Icon {
    match c {
        Category::Video => Icon::Video,
        Category::Music => Icon::Music,
        Category::Documents => Icon::Document,
        Category::Compressed => Icon::Archive,
        Category::Programs => Icon::App,
        Category::Other => Icon::File,
    }
}

fn section_label<'a>(label: String) -> Element<'a, Message> {
    container(text(label.to_uppercase()).size(11).font(semibold()).style(theme::muted))
        .padding([6, 10])
        .into()
}

fn nav_button<'a>(
    ic: Icon,
    label: String,
    count: Option<usize>,
    selected: bool,
    message: Message,
) -> Element<'a, Message> {
    let tint = move |theme: &Theme, _| {
        let t = tokens(theme);
        svg::Style { color: Some(if selected { t.primary } else { t.muted }) }
    };
    let mut line = row![icon(ic, 17.0).style(tint), text(label).size(14).width(Length::Fill)]
        .spacing(10)
        .align_y(Alignment::Center);
    if let Some(n) = count.filter(|&n| n > 0) {
        line = line.push(text(n.to_string()).size(12).style(theme::muted));
    }
    button(line)
        .padding([7, 10])
        .width(Length::Fill)
        .on_press(message)
        .style(theme::nav(selected))
        .into()
}

/// A toolbar icon with a tooltip; dimmed while it can't be used.
fn tool<'a>(
    ic: Icon,
    label: String,
    message: Option<Message>,
    danger: bool,
) -> Element<'a, Message> {
    let enabled = message.is_some();
    let tint = move |theme: &Theme, _| {
        let t = tokens(theme);
        let c = if !enabled {
            t.muted.scale_alpha(0.45)
        } else if danger {
            t.danger
        } else {
            t.text
        };
        svg::Style { color: Some(c) }
    };
    let b =
        button(icon(ic, 18.0).style(tint)).padding(8).on_press_maybe(message).style(theme::ghost);
    tooltip(
        b,
        container(text(label).size(12)).padding([4, 8]).style(theme::tip),
        tooltip::Position::Bottom,
    )
    .gap(4)
    .into()
}

/// "Add download"; just the plus (with a tooltip) when `compact`.
fn add_button<'a>(compact: bool) -> Element<'a, Message> {
    let plus =
        icon(Icon::Plus, 16.0).style(|_: &Theme, _| svg::Style { color: Some(Color::WHITE) });
    if compact {
        let b = button(plus).padding(11).on_press(Message::OpenAdd).style(theme::primary);
        return tooltip(
            b,
            container(text(t!("toolbar.add")).size(12)).padding([4, 8]).style(theme::tip),
            tooltip::Position::Bottom,
        )
        .gap(4)
        .into();
    }
    button(
        row![plus, text(t!("toolbar.add")).size(14).font(semibold())]
            .spacing(8)
            .align_y(Alignment::Center),
    )
    .padding([9, 16])
    .on_press(Message::OpenAdd)
    .style(theme::primary)
    .into()
}

fn notice<'a>(
    ic: Icon,
    message: &'a str,
    tone: fn(&Tokens) -> Color,
    dismiss: Message,
) -> Element<'a, Message> {
    container(
        row![
            icon(ic, 18.0)
                .style(move |theme: &Theme, _| svg::Style { color: Some(tone(&tokens(theme))) }),
            text(message).size(13).width(Length::Fill),
            button(text(t!("common.dismiss")).size(13))
                .padding([5, 10])
                .on_press(dismiss)
                .style(theme::ghost),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([8, 12])
    .width(Length::Fill)
    .style(theme::banner(tone))
    .into()
}

/// A dialog centered over a dimmed page; clicks outside it are swallowed.
fn modal(content: Element<'_, Message>, width: f32) -> Element<'_, Message> {
    let dialog = container(content).width(width).padding(24).style(theme::dialog);
    opaque(center(opaque(dialog)).style(theme::scrim))
}

/// Icon tile, title and a line under it. `tone` None uses the brand gradient.
fn dialog_header<'a>(
    ic: Icon,
    tone: Option<fn(&Tokens) -> Color>,
    title: String,
    subtitle: String,
) -> Element<'a, Message> {
    let tile = match tone {
        None => {
            container(icon(ic, 20.0).style(|_: &Theme, _| svg::Style { color: Some(Color::WHITE) }))
                .center(42)
                .style(theme::logo_tile)
        }
        Some(tone) => container(
            icon(ic, 20.0)
                .style(move |theme: &Theme, _| svg::Style { color: Some(tone(&tokens(theme))) }),
        )
        .center(42)
        .style(move |theme: &Theme| theme::badge(tone(&tokens(theme)))(theme)),
    };
    row![
        tile,
        column![text(title).size(19).font(bold()), text(subtitle).size(13).style(theme::muted)]
            .spacing(2),
    ]
    .spacing(14)
    .align_y(Alignment::Center)
    .into()
}

fn section<'a>(title: String) -> Element<'a, Message> {
    column![
        space().height(4),
        text(title).size(15).font(semibold()),
        container(space().height(1)).width(Length::Fill).style(theme::divider),
    ]
    .spacing(6)
    .into()
}

fn field<'a>(label: String, input: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![text(label).size(12).font(semibold()).style(theme::muted), input.into()]
        .spacing(6)
        .into()
}

fn input<'a>(placeholder: &str, value: &str) -> text_input::TextInput<'a, Message> {
    text_input(placeholder, value).padding([9, 11]).size(14).style(theme::input)
}

fn toggle<'a>(on: bool, label: String, message: fn(bool) -> Message) -> Element<'a, Message> {
    checkbox(on)
        .label(label)
        .size(18)
        .text_size(14)
        .spacing(10)
        .on_toggle(message)
        .style(theme::check)
        .into()
}

fn theme_picker<'a>(current: ThemeChoice) -> Element<'a, Message> {
    let items = ThemeChoice::ALL.iter().map(|&choice| {
        button(text(choice.label()).size(13).font(semibold()))
            .padding([6, 16])
            .on_press(Message::SetTheme(choice))
            .style(theme::segment(choice == current))
            .into()
    });
    container(row(items).spacing(2)).padding(3).style(theme::segment_track).into()
}

/// A queue in a drop-down list.
#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueChoice(QueueId, String);

impl std::fmt::Display for QueueChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

/// A "when done" action in a drop-down list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WhenDoneChoice(WhenDone);

impl std::fmt::Display for WhenDoneChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&queues::when_done_label(self.0))
    }
}

/// "The computer shuts down in 42 s" with "Do it now" and "Cancel".
fn power_banner<'a>(power: &super::PowerCountdown) -> Element<'a, Message> {
    let seconds = power.remaining();
    let key_text = match power.action {
        WhenDone::Exit => t!("power.exit", queue = power.queue, seconds = seconds),
        WhenDone::Sleep => t!("power.sleep", queue = power.queue, seconds = seconds),
        WhenDone::Hibernate => t!("power.hibernate", queue = power.queue, seconds = seconds),
        WhenDone::ShutDown | WhenDone::Nothing => {
            t!("power.shut_down", queue = power.queue, seconds = seconds)
        }
    };
    container(
        row![
            icon(Icon::Clock, 18.0).style(theme::icon_in(|t| t.warning)),
            text(key_text).size(13).width(Length::Fill),
            button(text(t!("power.now")).size(13))
                .padding([5, 10])
                .on_press(Message::PowerNow)
                .style(theme::ghost),
            button(text(t!("power.cancel")).size(13).font(semibold()))
                .padding([6, 14])
                .on_press(Message::PowerCancel)
                .style(theme::secondary),
        ]
        .spacing(10)
        .align_y(Alignment::Center),
    )
    .padding([8, 12])
    .width(Length::Fill)
    .style(theme::banner(|t| t.warning))
    .into()
}

/// A compact button with an icon and a label.
fn small_icon_button<'a>(ic: Icon, label: String, message: Message) -> Element<'a, Message> {
    button(row![icon(ic, 14.0), text(label).size(13)].spacing(6).align_y(Alignment::Center))
        .padding([6, 12])
        .on_press(message)
        .style(theme::secondary)
        .into()
}

/// A label with a hint under it on the left, and a control on the right.
fn setting_row<'a>(
    label: String,
    hint: String,
    control: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![
        column![text(label).size(14), text(hint).size(12).style(theme::muted)]
            .spacing(2)
            .width(Length::Fill),
        control.into(),
    ]
    .spacing(12)
    .align_y(Alignment::Center)
    .into()
}

/// One entry of the language list: a language, or "follow the system".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LangChoice(Option<Lang>);

impl std::fmt::Display for LangChoice {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(lang) => f.write_str(lang.native_name()),
            None => f.write_str(&t!(
                "settings.language_system",
                language = Lang::system().native_name()
            )),
        }
    }
}

fn language_picker<'a>(current: Option<Lang>) -> Element<'a, Message> {
    let options: Vec<LangChoice> = std::iter::once(LangChoice(None))
        .chain(Lang::ALL.into_iter().map(|l| LangChoice(Some(l))))
        .collect();
    pick_list(options, Some(LangChoice(current)), |c: LangChoice| Message::SetLanguage(c.0))
        .text_size(13)
        .padding([7, 12])
        .style(theme::pick)
        .menu_style(theme::pick_menu)
        .into()
}

/// A text input with a "Browse…" button that picks a folder for it.
fn with_browse<'a>(
    input: text_input::TextInput<'a, Message>,
    target: BrowseFor,
) -> Element<'a, Message> {
    row![
        input.width(Length::Fill),
        secondary_button(t!("common.browse"), Some(Message::Browse(target)))
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn buttons<'a>(
    cancel: impl Into<Element<'a, Message>>,
    ok: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    row![space().width(Length::Fill), cancel.into(), ok.into()].spacing(10).into()
}

fn primary_button<'a>(label: String, message: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(14).font(semibold()))
        .padding([9, 20])
        .on_press_maybe(message)
        .style(theme::primary)
        .into()
}

fn secondary_button<'a>(label: String, message: Option<Message>) -> Element<'a, Message> {
    button(text(label).size(14))
        .padding([9, 16])
        .on_press_maybe(message)
        .style(theme::secondary)
        .into()
}

fn small_button<'a>(label: String, message: Message) -> Element<'a, Message> {
    button(text(label).size(13)).padding([6, 12]).on_press(message).style(theme::secondary).into()
}

fn error_text<'a>(message: String) -> Element<'a, Message> {
    row![
        icon(Icon::Alert, 15.0).style(theme::icon_in(|t| t.danger)),
        text(message).size(13).style(theme::tone_text(|t| t.danger)),
    ]
    .spacing(8)
    .align_y(Alignment::Center)
    .into()
}

fn thin() -> scrollable::Direction {
    scrollable::Direction::Vertical(
        scrollable::Scrollbar::new().width(6).scroller_width(6).margin(2),
    )
}

/// The host part of a URL ("example.com"), or the URL itself.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let host = host.rsplit_once('@').map_or(host, |(_, h)| h);
    if host.is_empty() { url } else { host }
}

/// Shorten a file name in the middle, keeping its start and its extension.
fn ellipsize(name: &str, max: usize) -> String {
    let count = name.chars().count();
    if count <= max || max < 8 {
        return name.to_owned();
    }
    let tail = (max / 3).min(14);
    let head = max - tail - 1;
    let start: String = name.chars().take(head).collect();
    let end: String = name.chars().skip(count - tail).collect();
    format!("{start}\u{2026}{end}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://cdn.example.com/a/b.zip?x=1"), "cdn.example.com");
        assert_eq!(host_of("ftp://user:pw@files.example.org/x"), "files.example.org");
        assert_eq!(host_of("https://example.com"), "example.com");
        assert_eq!(host_of("not a url"), "not a url");
    }

    #[test]
    fn ellipsize_keeps_the_extension() {
        assert_eq!(ellipsize("short.zip", 46), "short.zip");
        let long = format!("{}.tar.gz", "a".repeat(60));
        let out = ellipsize(&long, 30);
        assert_eq!(out.chars().count(), 30);
        assert!(out.ends_with("aaa.tar.gz") && out.contains('\u{2026}'));
    }

    #[test]
    fn filters() {
        let row = |name: &str, state| Row { file_name: name.to_owned(), state, ..Row::for_tests() };
        let video = row("clip.mp4", DownloadState::Downloading);
        let zip = row("a.zip", DownloadState::Paused);
        assert!(Filter::All.matches(&video));
        assert!(Filter::Active.matches(&video) && !Filter::Active.matches(&zip));
        assert!(Filter::Waiting.matches(&zip));
        assert!(Filter::Category(Category::Video).matches(&video));
        assert!(!Filter::Category(Category::Video).matches(&zip));
        assert!(Filter::Category(Category::Compressed).matches(&zip));
        assert!(!Filter::Completed.matches(&zip) && !Filter::Failed.matches(&zip));
    }
}
