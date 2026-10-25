//! System tray icon with a small menu (Windows and macOS; elsewhere
//! [`Tray::new`] fails and the app runs without one).
//!
//! The icon must be created on the main thread while the event loop runs,
//! which is where iced calls `boot`. Clicks and menu choices arrive on
//! global channels; [`actions`] turns them into a stream for a subscription.

pub use imp::{Tray, actions};

/// Whether this platform has a tray icon to hide the window into.
pub const SUPPORTED: bool = cfg!(any(windows, target_os = "macos"));

/// What the user asked for through the tray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    Show,
    PauseAll,
    ResumeAll,
    Quit,
}

#[cfg_attr(not(any(windows, target_os = "macos")), allow(dead_code))]
/// Menu id, label key, action.
const MENU: [(&str, &str, Action); 4] = [
    ("show", "tray.show", Action::Show),
    ("resume-all", "tray.resume_all", Action::ResumeAll),
    ("pause-all", "tray.pause_all", Action::PauseAll),
    ("quit", "tray.quit", Action::Quit),
];

#[cfg(any(windows, target_os = "macos"))]
mod imp {
    use iced::futures::Stream;
    use iced::futures::channel::mpsc;
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    use super::{Action, MENU};
    use crate::icon::icon_rgba;

    pub struct Tray {
        icon: TrayIcon,
        items: Vec<MenuItem>,
        tooltip: String,
    }

    impl Tray {
        pub fn new() -> anyhow::Result<Self> {
            let menu = Menu::new();
            let items: Vec<MenuItem> = MENU
                .iter()
                .map(|(id, key, _)| MenuItem::with_id(*id, crate::i18n::tr(key), true, None))
                .collect();
            let separator = PredefinedMenuItem::separator();
            menu.append_items(&[
                &items[0], &separator, &items[1], &items[2], &separator, &items[3],
            ])?;
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .with_tooltip("Vibe Download Manager")
                .with_icon(Icon::from_rgba(icon_rgba(32), 32, 32)?)
                .build()?;
            Ok(Self { icon, items, tooltip: "Vibe Download Manager".to_owned() })
        }

        /// Relabel the menu in the current language.
        pub fn relabel(&self) {
            for (item, (_, key, _)) in self.items.iter().zip(MENU) {
                item.set_text(crate::i18n::tr(key));
            }
        }

        /// Update the hover text (only touches the OS when it changed).
        pub fn set_tooltip(&mut self, text: &str) {
            if self.tooltip != text {
                if let Err(err) = self.icon.set_tooltip(Some(text)) {
                    tracing::debug!(%err, "could not update tray tooltip");
                }
                self.tooltip = text.to_owned();
            }
        }
    }

    /// Tray clicks and menu choices, for `Subscription::run`.
    pub fn actions() -> impl Stream<Item = Action> {
        let (tx, rx) = mpsc::unbounded();
        let menu_tx = tx.clone();
        std::thread::spawn(move || {
            while let Ok(event) = MenuEvent::receiver().recv() {
                let action = MENU.iter().find(|(id, ..)| event.id.0 == *id).map(|(.., a)| *a);
                if let Some(action) = action
                    && menu_tx.unbounded_send(action).is_err()
                {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(event) = TrayIconEvent::receiver().recv() {
                let show = matches!(
                    event,
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. }
                );
                if show && tx.unbounded_send(Action::Show).is_err() {
                    break;
                }
            }
        });
        rx
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod imp {
    use iced::futures::Stream;

    use super::Action;

    pub struct Tray;

    impl Tray {
        pub fn new() -> anyhow::Result<Self> {
            anyhow::bail!("no tray icon on this platform yet")
        }

        pub fn set_tooltip(&mut self, _text: &str) {}

        pub fn relabel(&self) {}
    }

    pub fn actions() -> impl Stream<Item = Action> {
        iced::futures::stream::empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_labels_are_translated() {
        for (_, key, _) in MENU {
            assert_ne!(crate::i18n::lookup(crate::i18n::Lang::En, key), key, "{key}");
        }
    }

    #[test]
    fn menu_ids_are_unique() {
        let mut ids: Vec<_> = MENU.iter().map(|(id, ..)| *id).collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), MENU.len());
    }
}
