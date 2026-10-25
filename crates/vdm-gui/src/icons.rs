//! Line icons as tiny SVGs (24×24, 2 px strokes), tinted by the theme.

use std::sync::LazyLock;

use iced::widget::svg::{self, Svg};
use iced::{Length, Theme};

/// Every icon the app uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Icon {
    Plus,
    Play,
    Pause,
    Close,
    Trash,
    Folder,
    Settings,
    Search,
    Download,
    Check,
    Alert,
    Clock,
    Bolt,
    Video,
    Music,
    Document,
    Archive,
    App,
    File,
    Sparkle,
    Queue,
    Info,
    Copy,
}

impl Icon {
    fn body(self) -> &'static str {
        match self {
            Icon::Plus => r#"<path d="M12 5v14M5 12h14"/>"#,
            Icon::Play => r#"<path d="M8 5.5v13l10.5-6.5z"/>"#,
            Icon::Pause => r#"<path d="M9 5.5v13M15 5.5v13"/>"#,
            Icon::Close => r#"<path d="M6.5 6.5l11 11M17.5 6.5l-11 11"/>"#,
            Icon::Trash => {
                r#"<path d="M4 7h16M9.5 7V4.5h5V7M6.5 7l1 13h9l1-13M10 11v5.5M14 11v5.5"/>"#
            }
            Icon::Folder => {
                r#"<path d="M3.5 7.5a2 2 0 0 1 2-2h3.8l2 2h7.2a2 2 0 0 1 2 2v7.5a2 2 0 0 1-2 2h-13a2 2 0 0 1-2-2z"/>"#
            }
            Icon::Settings => {
                r#"<path d="M4 7h9M17 7h3M4 17h3M11 17h9"/><circle cx="15" cy="7" r="2"/><circle cx="9" cy="17" r="2"/>"#
            }
            Icon::Search => r#"<circle cx="11" cy="11" r="6"/><path d="M15.5 15.5l4 4"/>"#,
            Icon::Download => r#"<path d="M12 4v11M7.5 10.5L12 15l4.5-4.5M5 19.5h14"/>"#,
            Icon::Check => r#"<path d="M5 12.5l4.5 4.5L19 7.5"/>"#,
            Icon::Alert => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5v5.5M12 16.5v.2"/>"#,
            Icon::Clock => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 7.5V12l3 2"/>"#,
            Icon::Bolt => r#"<path d="M13 3.5L5.5 13.5H12l-1 7 7.5-10H12z"/>"#,
            Icon::Video => {
                r#"<rect x="3.5" y="6" width="12" height="12" rx="2"/><path d="M15.5 10.5l5-3v9l-5-3"/>"#
            }
            Icon::Music => {
                r#"<path d="M9 17.5V6l10-2v11.5"/><circle cx="6.5" cy="17.5" r="2.5"/><circle cx="16.5" cy="15.5" r="2.5"/>"#
            }
            Icon::Document => {
                r#"<path d="M14 3.5H7a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5zM14 3.5v5h5M9 13h6M9 16.5h6"/>"#
            }
            Icon::Archive => {
                r#"<rect x="3.5" y="4.5" width="17" height="4" rx="1"/><path d="M5 8.5v10a1.5 1.5 0 0 0 1.5 1.5h11a1.5 1.5 0 0 0 1.5-1.5v-10M10 12h4"/>"#
            }
            Icon::App => {
                r#"<rect x="3.5" y="4.5" width="17" height="15" rx="2"/><path d="M3.5 8.5h17M7 6.5h.2M9.5 6.5h.2"/>"#
            }
            Icon::File => {
                r#"<path d="M14 3.5H7a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h10a2 2 0 0 0 2-2V8.5zM14 3.5v5h5"/>"#
            }
            Icon::Queue => r#"<path d="M4 6.5h16M4 12h10M4 17.5h7M16.5 14.5l3.5 3-3.5 3"/>"#,
            Icon::Info => r#"<circle cx="12" cy="12" r="8.5"/><path d="M12 11v5.5M12 7.5v.2"/>"#,
            Icon::Copy => {
                r#"<rect x="8.5" y="8.5" width="11" height="11" rx="2"/><path d="M15.5 8.5V6.5a2 2 0 0 0-2-2h-7a2 2 0 0 0-2 2v7a2 2 0 0 0 2 2h2"/>"#
            }
            Icon::Sparkle => {
                r#"<path d="M12 3.5l1.8 5.2 5.2 1.8-5.2 1.8L12 17.5l-1.8-5.2L5 10.5l5.2-1.8z"/><path d="M18.5 16v4M16.5 18h4"/>"#
            }
        }
    }

    fn handle(self) -> svg::Handle {
        static HANDLES: LazyLock<Vec<(Icon, svg::Handle)>> = LazyLock::new(|| {
            ALL.iter()
                .map(|&icon| {
                    let doc = format!(
                        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="black" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">{}</svg>"#,
                        icon.body()
                    );
                    (icon, svg::Handle::from_memory(doc.into_bytes()))
                })
                .collect()
        });
        HANDLES
            .iter()
            .find(|(i, _)| *i == self)
            .map(|(_, h)| h.clone())
            .expect("every icon is in ALL")
    }
}

const ALL: [Icon; 23] = [
    Icon::Plus,
    Icon::Play,
    Icon::Pause,
    Icon::Close,
    Icon::Trash,
    Icon::Folder,
    Icon::Settings,
    Icon::Search,
    Icon::Download,
    Icon::Check,
    Icon::Alert,
    Icon::Clock,
    Icon::Bolt,
    Icon::Video,
    Icon::Music,
    Icon::Document,
    Icon::Archive,
    Icon::App,
    Icon::File,
    Icon::Sparkle,
    Icon::Queue,
    Icon::Info,
    Icon::Copy,
];

/// `icon` at `size` px, colored like text unless restyled.
pub fn icon<'a>(icon: Icon, size: f32) -> Svg<'a, Theme> {
    Svg::new(icon.handle())
        .width(Length::Fixed(size))
        .height(Length::Fixed(size))
        .style(crate::theme::icon_text)
}

/// The app logo: a white arrow landing on a wave, on the brand gradient.
pub fn logo<'a>(size: f32) -> Svg<'a, Theme> {
    static LOGO: LazyLock<svg::Handle> =
        LazyLock::new(|| svg::Handle::from_memory(LOGO_SVG.as_bytes()));
    Svg::new(LOGO.clone()).width(Length::Fixed(size)).height(Length::Fixed(size))
}

/// The logo as an SVG document (also the source for the raster icon's look).
pub const LOGO_SVG: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 64 64">
<defs><linearGradient id="g" x1="0" y1="0" x2="1" y2="1">
<stop offset="0" stop-color="#8b5cf6"/><stop offset="1" stop-color="#ec4899"/></linearGradient></defs>
<rect width="64" height="64" rx="16" fill="url(#g)"/>
<g fill="none" stroke="#fff" stroke-width="5.5" stroke-linecap="round" stroke-linejoin="round">
<path d="M32 13v22M22.5 26.5 32 36l9.5-9.5"/>
<path d="M15 47c4.3-4 8.7-4 13 0s8.7 4 13 0 6.5-3 8 0" stroke-width="5"/>
</g></svg>"##;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_has_a_handle() {
        for icon in ALL {
            let _ = icon.handle();
        }
        assert_eq!(ALL.len(), 23);
    }

    #[test]
    fn icon_documents_are_well_formed() {
        for icon in ALL {
            let body = icon.body();
            assert_eq!(body.matches('<').count(), body.matches("/>").count(), "{icon:?}");
        }
        assert!(LOGO_SVG.starts_with("<svg") && LOGO_SVG.ends_with("</svg>"));
    }
}
