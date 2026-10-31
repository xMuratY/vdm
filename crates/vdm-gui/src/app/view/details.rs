//! The details panel beside the list: one download's pieces, speed, file,
//! server and request facts, and what happened to it.

use iced::widget::{button, column, container, row, scrollable, space, svg, text, tooltip};
use iced::{Alignment, Color, Element, Length, Theme};
use vdm_proto::DownloadState;
use vdm_store::DownloadRecord;

use super::{bold, category_color, category_icon, semibold, state_tone, thin};
use crate::app::{App, Message};
use crate::categories::Category;
use crate::details::{self, Live, Palette, Rgb};
use crate::i18n::t;
use crate::icons::{Icon, icon};
use crate::model::{self, Row};
use crate::queues;
use crate::theme::{self, Tokens, tokens};

/// The panel's width, and the width of what it holds.
pub(super) const WIDTH: f32 = 348.0;
/// The narrowest window that shows the panel beside the list; below it the
/// panel floats over the list.
pub(super) const BESIDE_FROM: f32 = 1140.0;
const PAD: f32 = 18.0;
const INNER: f32 = WIDTH - 2.0 * PAD;
/// The panel's scroll area, for jumping to its end in screenshots.
pub(in crate::app) const DETAILS_SCROLL: &str = "details-scroll";
/// Activity entries shown, newest first.
const LOG_SHOWN: usize = 40;

impl App {
    pub(super) fn details_panel(&self) -> Element<'_, Message> {
        let selection = self.downloads.selection();
        let content: Element<'_, Message> = match selection.as_slice() {
            [r] => {
                let record = self.details_record.as_ref().filter(|rec| rec.id == r.id);
                self.details_of(r, self.live.get(r.id), record)
            }
            many => self.details_hint(many.len()),
        };
        container(content).width(WIDTH).height(Length::Fill).style(theme::sidebar).into()
    }

    fn details_hint(&self, selected: usize) -> Element<'_, Message> {
        let message =
            if selected == 0 { t!("details.pick") } else { t!("details.many", count = selected) };
        let body = column![
            icon(Icon::Info, 30.0).style(theme::icon_in(|t| t.muted)),
            text(message).size(13).style(theme::muted).align_x(Alignment::Center),
        ]
        .spacing(12)
        .align_x(Alignment::Center);
        column![self.details_top_bar(), container(body).center(Length::Fill).padding(PAD),].into()
    }

    fn details_top_bar(&self) -> Element<'_, Message> {
        let close = tooltip(
            button(icon(Icon::Close, 15.0).style(theme::icon_in(|t| t.muted)))
                .padding(6)
                .on_press(Message::ToggleDetails)
                .style(theme::ghost),
            container(text(t!("details.close")).size(12)).padding([4, 8]).style(theme::tip),
            tooltip::Position::Left,
        )
        .gap(4);
        row![
            text(t!("details.title")).size(13).font(semibold()).style(theme::muted),
            space().width(Length::Fill),
            close
        ]
        .align_y(Alignment::Center)
        .padding(iced::Padding { top: 10.0, right: 10.0, bottom: 4.0, left: PAD })
        .into()
    }

    fn details_of<'a>(
        &'a self,
        r: &'a Row,
        live: Option<&'a Live>,
        record: Option<&'a DownloadRecord>,
    ) -> Element<'a, Message> {
        let t = tokens(&self.theme());
        let mut body = column![self.details_header(r)].spacing(22).width(INNER);
        body = body.push(pieces(r, live, &t));
        body = body.push(stats(r, live));
        if let Some(live) = live.filter(|l| !l.speeds.is_empty()) {
            body = body.push(speed(live, &t));
        }
        body = body.push(self.file_section(r, record));
        body = body.push(server_section(r, record));
        if let Some(record) = record {
            body = body.push(request_section(record));
        }
        body = body.push(self.activity(live));

        column![
            self.details_top_bar(),
            scrollable(container(body).padding(iced::Padding {
                top: 0.0,
                right: PAD,
                bottom: PAD,
                left: PAD
            }))
            .id(iced::widget::Id::new(DETAILS_SCROLL))
            .height(Length::Fill)
            .direction(thin())
            .style(theme::scrollbar),
        ]
        .into()
    }

    fn details_header<'a>(&self, r: &'a Row) -> Element<'a, Message> {
        let category = Category::from_name(&r.file_name);
        let badge_color = category_color(category);
        let badge = container(
            icon(category_icon(category), 20.0)
                .style(move |_: &Theme, _| svg::Style { color: Some(badge_color) }),
        )
        .center_x(40)
        .center_y(40)
        .style(theme::badge(badge_color));
        let tone_of = state_tone(r.state);
        let state = container(text(model::state_label(r.state)).size(12).font(semibold()))
            .padding([3, 10])
            .style(move |theme: &Theme| theme::pill(tone_of(&tokens(theme)))(theme));
        let mut head = column![
            row![
                badge,
                text(r.file_name.as_str())
                    .size(16)
                    .font(bold())
                    .wrapping(text::Wrapping::WordOrGlyph)
                    .width(Length::Fill),
            ]
            .spacing(12)
            .align_y(Alignment::Center),
            row![
                state,
                space().width(Length::Fill),
                small_action(Icon::Folder, t!("toolbar.show_in_folder"), Message::RevealRow(r.id)),
                small_action(
                    Icon::Copy,
                    t!("details.copy_address"),
                    Message::CopyText(r.url.clone())
                ),
            ]
            .spacing(6)
            .align_y(Alignment::Center),
        ]
        .spacing(12);
        if let Some(err) = &r.error {
            head = head.push(
                container(
                    text(err.as_str())
                        .size(13)
                        .style(theme::tone_text(|t| t.danger))
                        .wrapping(text::Wrapping::WordOrGlyph),
                )
                .padding([8, 10])
                .width(Length::Fill)
                .style(theme::banner(|t| t.danger)),
            );
        }
        head.into()
    }

    fn file_section<'a>(
        &self,
        r: &'a Row,
        record: Option<&'a DownloadRecord>,
    ) -> Element<'a, Message> {
        let path = std::path::Path::new(&r.save_dir).join(&r.file_name).display().to_string();
        let mut items = column![
            fact(t!("details.saved_to"), path.clone(), Some(path)),
            fact(t!("details.address"), details::mask_url(&r.url), Some(r.url.clone())),
        ]
        .spacing(12);
        let queue = self.queues.iter().find(|q| q.id == r.queue).map(queues::name);
        if let Some(queue) = queue {
            items = items.push(fact(t!("details.queue"), queue, None));
        }
        if let Some(rec) = record {
            items =
                items.push(fact(t!("details.added"), details::local_time(rec.created_at), None));
            let checksum = rec.checksum.clone().unwrap_or_else(|| t!("details.none"));
            items = items.push(fact(t!("details.checksum"), checksum, None));
        }
        group(t!("details.file"), items)
    }

    fn activity<'a>(&self, live: Option<&'a Live>) -> Element<'a, Message> {
        let queue_name = |id| {
            self.queues.iter().find(|q| q.id == id).map_or_else(|| id.0.to_string(), queues::name)
        };
        let entries: Vec<_> =
            live.map(|l| l.log.iter().rev().take(LOG_SHOWN).collect()).unwrap_or_default();
        let mut list = column![].spacing(9);
        if entries.is_empty() {
            list = list.push(text(t!("details.no_activity")).size(13).style(theme::muted));
        }
        for entry in entries {
            let failure = entry.what.is_failure();
            let words = text(entry.what.describe(queue_name))
                .size(13)
                .wrapping(text::Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .style(move |theme: &Theme| {
                    let t = tokens(theme);
                    text::Style { color: Some(if failure { t.danger } else { t.text }) }
                });
            list = list.push(
                row![
                    text(entry.at.format("%H:%M:%S").to_string())
                        .size(12)
                        .style(theme::muted)
                        .width(62),
                    words,
                ]
                .spacing(8),
            );
        }
        group(t!("details.activity"), list)
    }
}

/// The piece map with its caption.
fn pieces<'a>(r: &Row, live: Option<&Live>, t: &Tokens) -> Element<'a, Message> {
    let segments = live.map_or(&[][..], |l| l.segments.as_slice());
    let (a, b) = match r.state {
        DownloadState::Completed => (t.success, t.success),
        DownloadState::Failed => (t.danger, t.danger),
        DownloadState::Paused => (t.warning, t.warning),
        DownloadState::Queued => (t.muted, t.muted),
        _ => (t.brand_a, t.brand_b),
    };
    let palette =
        Palette { track: rgb(t.raised), fill_a: rgb(a), fill_b: rgb(b), gap: rgb(t.surface) };
    let doc = details::piece_map(segments, r.progress(), INNER, 14.0, &palette);
    // Torrents send a map of up to 400 cells, not their pieces: no count then.
    let caption = match segments.len() {
        0 | 65.. => r.progress().map(|p| format!("{:.0}%", p * 100.0)).unwrap_or_default(),
        n => t!(
            "details.pieces",
            count = n,
            percent = format!("{:.0}", r.progress().unwrap_or(0.0) * 100.0)
        ),
    };
    column![
        svg(svg::Handle::from_memory(doc.into_bytes())).width(INNER).height(14),
        text(caption).size(12).style(theme::muted),
    ]
    .spacing(8)
    .into()
}

/// Downloaded, speed, time left and connections, two by two.
fn stats<'a>(r: &Row, live: Option<&Live>) -> Element<'a, Message> {
    let downloaded = match r.total {
        Some(total) => {
            t!("details.of", done = model::fmt_bytes(r.downloaded), total = model::fmt_bytes(total))
        }
        None => model::fmt_bytes(r.downloaded),
    };
    let dash = || "\u{2014}".to_owned();
    let speed =
        Some(model::fmt_speed(r.bytes_per_sec)).filter(|s| !s.is_empty()).unwrap_or_else(dash);
    let left = r.eta_secs().map_or_else(dash, model::fmt_eta);
    let active = live.map_or(0, |l| l.connections);
    let connections = if r.state == DownloadState::Downloading && active > 0 {
        t!("details.connections_value", active = active, max = r.connections.max(active))
    } else {
        r.connections.to_string()
    };
    column![
        row![stat(t!("details.downloaded"), downloaded), stat(t!("details.speed"), speed)]
            .spacing(12),
        row![stat(t!("details.time_left"), left), stat(t!("details.connections"), connections)]
            .spacing(12),
    ]
    .spacing(14)
    .into()
}

fn stat<'a>(label: String, value: String) -> Element<'a, Message> {
    column![
        text(label).size(12).style(theme::muted),
        text(value).size(15).font(semibold()).wrapping(text::Wrapping::WordOrGlyph),
    ]
    .spacing(3)
    .width(Length::Fill)
    .into()
}

/// The speed graph: the last 90 seconds, with peak and average.
fn speed<'a>(live: &Live, t: &Tokens) -> Element<'a, Message> {
    let palette = Palette {
        track: rgb(t.border),
        fill_a: rgb(t.brand_a),
        fill_b: rgb(t.brand_b),
        gap: rgb(t.surface),
    };
    let doc = details::speed_graph(&live.speeds, INNER, 64.0, &palette);
    let peak = t!("details.peak", speed = model::fmt_bytes(live.peak()) + "/s");
    let average = t!("details.average", speed = model::fmt_bytes(live.average()) + "/s");
    column![
        row![
            text(t!("details.speed_graph", seconds = details::HISTORY))
                .size(12)
                .style(theme::muted),
            space().width(Length::Fill),
        ],
        svg(svg::Handle::from_memory(doc.into_bytes())).width(INNER).height(64),
        row![
            text(peak).size(12).style(theme::muted),
            space().width(Length::Fill),
            text(average).size(12).style(theme::muted),
        ],
    ]
    .spacing(8)
    .into()
}

fn server_section<'a>(r: &Row, record: Option<&DownloadRecord>) -> Element<'a, Message> {
    let unknown = || t!("details.unknown");
    let size = r.total.or(record.and_then(|rec| rec.size)).map_or_else(unknown, |s| {
        format!("{} ({} {})", model::fmt_bytes(s), s, t!("details.bytes"))
    });
    let resume = match r.resumable {
        Some(true) => t!("details.resume_yes"),
        Some(false) => t!("details.resume_no"),
        None => unknown(),
    };
    let mut items =
        column![fact(t!("details.size"), size, None), fact(t!("details.resume"), resume, None),]
            .spacing(12);
    if let Some(rec) = record {
        if let Some(modified) = &rec.last_modified {
            items = items.push(fact(t!("details.modified"), modified.clone(), None));
        }
        if let Some(etag) = &rec.etag {
            items = items.push(fact(t!("details.etag"), etag.clone(), None));
        }
    }
    group(t!("details.server"), items)
}

fn request_section<'a>(rec: &DownloadRecord) -> Element<'a, Message> {
    let none = || t!("details.none");
    let referrer = details::header(&rec.headers, "Referer").map(str::to_owned);
    let agent = details::header(&rec.headers, "User-Agent").map(str::to_owned);
    let cookies = details::header(&rec.headers, "Cookie")
        .map(details::cookie_count)
        .filter(|&n| n > 0)
        .map_or_else(none, |n| t!("details.cookies_value", count = n));
    let items = column![
        fact(t!("details.referrer"), referrer.clone().unwrap_or_else(none), referrer),
        fact(t!("details.user_agent"), agent.unwrap_or_else(|| t!("details.default_agent")), None),
        fact(t!("details.cookies"), cookies, None),
    ]
    .spacing(12);
    group(t!("details.request"), items)
}

/// A titled group of facts.
fn group<'a>(title: String, items: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    column![
        text(title).size(13).font(semibold()),
        container(space().height(1)).width(Length::Fill).style(theme::divider),
        items.into(),
    ]
    .spacing(10)
    .into()
}

/// A label over a value, with a copy button when `copy` is given.
fn fact<'a>(label: String, value: String, copy: Option<String>) -> Element<'a, Message> {
    let value = text(value).size(13).wrapping(text::Wrapping::WordOrGlyph).width(Length::Fill);
    let mut line = row![value].spacing(6).align_y(Alignment::Start);
    if let Some(copy) = copy {
        line = line.push(small_action(Icon::Copy, t!("details.copy"), Message::CopyText(copy)));
    }
    column![text(label).size(12).style(theme::muted), line].spacing(3).into()
}

fn small_action<'a>(ic: Icon, label: String, message: Message) -> Element<'a, Message> {
    tooltip(
        button(icon(ic, 14.0).style(theme::icon_in(|t| t.muted)))
            .padding(5)
            .on_press(message)
            .style(theme::ghost),
        container(text(label).size(12)).padding([4, 8]).style(theme::tip),
        tooltip::Position::Left,
    )
    .gap(4)
    .into()
}

fn rgb(c: Color) -> Rgb {
    let [r, g, b, _] = c.into_rgba8();
    Rgb(r, g, b)
}
