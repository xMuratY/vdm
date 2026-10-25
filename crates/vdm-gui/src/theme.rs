//! Vibe's look: a violet-to-pink brand on calm surfaces, in light and dark.
//!
//! [`vibe`] builds the iced [`Theme`] (so stock widgets such as checkboxes
//! pick up the brand), and the style functions below draw the custom parts:
//! cards, the sidebar, rows, pills, banners and the gradient progress bar.

use iced::overlay::menu;
use iced::widget::{
    button, checkbox, container, pick_list, progress_bar, scrollable, svg, text, text_input,
};
use iced::{Background, Border, Color, Gradient, Shadow, Theme, Vector, border, color, gradient};
use serde::{Deserialize, Serialize};

/// The user's theme choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeChoice {
    /// Follow the system's light/dark setting.
    #[default]
    System,
    Light,
    Dark,
}

impl ThemeChoice {
    pub const ALL: [ThemeChoice; 3] = [ThemeChoice::System, ThemeChoice::Light, ThemeChoice::Dark];

    pub fn label(self) -> String {
        match self {
            ThemeChoice::System => crate::i18n::t!("settings.theme_system"),
            ThemeChoice::Light => crate::i18n::t!("settings.theme_light"),
            ThemeChoice::Dark => crate::i18n::t!("settings.theme_dark"),
        }
    }

    /// Whether this choice is dark, given the system's mode.
    pub fn is_dark(self, system: iced::theme::Mode) -> bool {
        match self {
            ThemeChoice::System => system == iced::theme::Mode::Dark,
            ThemeChoice::Light => false,
            ThemeChoice::Dark => true,
        }
    }
}

/// The colors everything else is drawn with.
#[derive(Debug, Clone, Copy)]
pub struct Tokens {
    /// Window background.
    pub bg: Color,
    /// Cards, sidebar, dialogs.
    pub surface: Color,
    /// Inputs, hovered rows, quiet buttons.
    pub raised: Color,
    pub border: Color,
    pub text: Color,
    pub muted: Color,
    pub primary: Color,
    /// The second brand color, for small highlights.
    pub accent: Color,
    /// Gradient fills behind white text (buttons, tiles, progress).
    pub brand_a: Color,
    pub brand_b: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub is_dark: bool,
}

/// Dark: neutral charcoal surfaces, with the brand kept soft so it doesn't glare.
const DARK: Tokens = Tokens {
    bg: color!(0x121215),
    surface: color!(0x19191d),
    raised: color!(0x232328),
    border: color!(0x2c2c33),
    text: color!(0xe6e6eb),
    muted: color!(0x9898a3),
    primary: color!(0xa594f9),
    accent: color!(0xeb94c4),
    brand_a: color!(0x6e55d6),
    brand_b: color!(0xa84d82),
    success: color!(0x63c99d),
    warning: color!(0xe2b25e),
    danger: color!(0xec8590),
    is_dark: true,
};

const LIGHT: Tokens = Tokens {
    bg: color!(0xf5f3fb),
    surface: color!(0xffffff),
    raised: color!(0xefecf7),
    border: color!(0xe2ddef),
    text: color!(0x1b1830),
    muted: color!(0x6b6582),
    primary: color!(0x7c3aed),
    accent: color!(0xdb2777),
    brand_a: color!(0x7c3aed),
    brand_b: color!(0xdb2777),
    success: color!(0x059669),
    warning: color!(0xb45309),
    danger: color!(0xe11d48),
    is_dark: false,
};

/// The Vibe theme, light or dark.
pub fn vibe(dark: bool) -> Theme {
    let t = if dark { DARK } else { LIGHT };
    let palette = iced::theme::Palette {
        background: t.bg,
        text: t.text,
        primary: t.primary,
        success: t.success,
        warning: t.warning,
        danger: t.danger,
    };
    Theme::custom(if dark { "Vibe Dark" } else { "Vibe Light" }, palette)
}

/// The tokens for whichever Vibe variant `theme` is.
pub fn tokens(theme: &Theme) -> Tokens {
    if theme.extended_palette().is_dark { DARK } else { LIGHT }
}

/// Text color mixed toward a background: `amount` 0 = `a`, 1 = `b`.
pub fn mix(a: Color, b: Color, amount: f32) -> Color {
    Color {
        r: a.r + (b.r - a.r) * amount,
        g: a.g + (b.g - a.g) * amount,
        b: a.b + (b.b - a.b) * amount,
        a: a.a + (b.a - a.a) * amount,
    }
}

/// The brand gradient, left to right.
pub fn brand_gradient(t: &Tokens) -> Background {
    Background::Gradient(Gradient::Linear(
        gradient::Linear::new(std::f32::consts::FRAC_PI_2)
            .add_stop(0.0, t.brand_a)
            .add_stop(1.0, t.brand_b),
    ))
}

fn soft_shadow(t: &Tokens) -> Shadow {
    Shadow {
        color: Color::BLACK.scale_alpha(if t.is_dark { 0.45 } else { 0.10 }),
        offset: Vector::new(0.0, 6.0),
        blur_radius: 24.0,
    }
}

// ---------------------------------------------------------------- containers

pub fn sidebar(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(t.surface.into()),
        border: Border { color: t.border, width: 1.0, radius: 0.into() },
        text_color: Some(t.text),
        ..container::Style::default()
    }
}

/// A rounded surface with a hairline border.
pub fn card(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(t.surface.into()),
        border: border::rounded(12).width(1).color(t.border),
        text_color: Some(t.text),
        ..container::Style::default()
    }
}

/// A dialog: a card that floats.
pub fn dialog(theme: &Theme) -> container::Style {
    container::Style { shadow: soft_shadow(&tokens(theme)), ..card(theme) }
}

/// Nothing itself; casts the floating details panel's shadow.
pub fn floating_edge(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        shadow: Shadow {
            color: Color::BLACK.scale_alpha(if t.is_dark { 0.5 } else { 0.12 }),
            offset: Vector::new(-4.0, 0.0),
            blur_radius: 28.0,
        },
        ..container::Style::default()
    }
}

/// The dimmed page behind a dialog.
pub fn scrim(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(Color::BLACK.scale_alpha(if t.is_dark { 0.6 } else { 0.35 }).into()),
        ..container::Style::default()
    }
}

/// A tinted banner in `tone` (primary, danger, …).
pub fn banner(tone: fn(&Tokens) -> Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let t = tokens(theme);
        let c = tone(&t);
        container::Style {
            background: Some(mix(t.surface, c, if t.is_dark { 0.09 } else { 0.08 }).into()),
            border: border::rounded(10).width(1).color(mix(
                t.surface,
                c,
                if t.is_dark { 0.3 } else { 0.45 },
            )),
            text_color: Some(t.text),
            ..container::Style::default()
        }
    }
}

/// A small rounded label in `tone`, e.g. a status pill.
pub fn pill(tone: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let t = tokens(theme);
        container::Style {
            background: Some(mix(t.surface, tone, 0.12).into()),
            border: border::rounded(999),
            text_color: Some(tone),
            ..container::Style::default()
        }
    }
}

/// A column title while a dragged column would land on it.
pub fn drop_target(theme: &Theme, active: bool) -> container::Style {
    if !active {
        return container::Style::default();
    }
    let t = tokens(theme);
    container::Style {
        background: Some(mix(t.surface, t.primary, if t.is_dark { 0.2 } else { 0.12 }).into()),
        border: border::rounded(6),
        ..container::Style::default()
    }
}

/// A pill in a theme `tone`.
pub fn pill_in(tone: fn(&Tokens) -> Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| pill(tone(&tokens(theme)))(theme)
}

/// A tooltip bubble.
pub fn tip(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(t.surface.into()),
        border: border::rounded(6).width(1).color(t.border),
        text_color: Some(t.text),
        shadow: Shadow {
            color: Color::BLACK.scale_alpha(0.2),
            offset: Vector::new(0.0, 2.0),
            blur_radius: 8.0,
        },
        ..container::Style::default()
    }
}

/// A quietly raised strip, e.g. a saved login.
pub fn raised(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(t.raised.scale_alpha(0.6).into()),
        border: border::rounded(9),
        text_color: Some(t.text),
        ..container::Style::default()
    }
}

/// A hairline rule.
pub fn divider(theme: &Theme) -> container::Style {
    container::Style {
        background: Some(tokens(theme).border.into()),
        ..container::Style::default()
    }
}

/// The square file-type badge at the start of a row.
pub fn badge(tone: Color) -> impl Fn(&Theme) -> container::Style {
    move |theme| {
        let t = tokens(theme);
        container::Style {
            background: Some(mix(t.surface, tone, if t.is_dark { 0.12 } else { 0.14 }).into()),
            border: border::rounded(9),
            text_color: Some(tone),
            ..container::Style::default()
        }
    }
}

/// The logo tile: the brand gradient.
pub fn logo_tile(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(brand_gradient(&t)),
        border: border::rounded(10),
        text_color: Some(Color::WHITE),
        ..container::Style::default()
    }
}

// ---------------------------------------------------------------- buttons

/// The main call to action: brand gradient, white text.
pub fn primary(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let background = match status {
        button::Status::Disabled => mix(t.raised, t.brand_a, 0.3).into(),
        button::Status::Hovered => Background::Gradient(Gradient::Linear(
            gradient::Linear::new(std::f32::consts::FRAC_PI_2)
                .add_stop(0.0, mix(t.brand_a, Color::WHITE, 0.08))
                .add_stop(1.0, mix(t.brand_b, Color::WHITE, 0.08)),
        )),
        _ => brand_gradient(&t),
    };
    button::Style {
        background: Some(background),
        text_color: if status == button::Status::Disabled {
            Color::WHITE.scale_alpha(0.6)
        } else {
            Color::WHITE
        },
        border: border::rounded(9),
        shadow: match status {
            // No glow in the dark theme; a soft lift in the light one.
            button::Status::Active | button::Status::Hovered if !t.is_dark => Shadow {
                color: t.brand_a.scale_alpha(0.25),
                offset: Vector::new(0.0, 3.0),
                blur_radius: 12.0,
            },
            _ => Shadow::default(),
        },
        snap: true,
    }
}

/// A quiet button on a raised surface.
pub fn secondary(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let (bg, fg) = match status {
        button::Status::Disabled => (t.raised.scale_alpha(0.5), t.muted.scale_alpha(0.5)),
        button::Status::Hovered => (mix(t.raised, t.primary, 0.12), t.text),
        button::Status::Pressed => (mix(t.raised, t.primary, 0.2), t.text),
        button::Status::Active => (t.raised, t.text),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: fg,
        border: border::rounded(9).width(1).color(if status == button::Status::Disabled {
            Color::TRANSPARENT
        } else {
            t.border
        }),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// No background until hovered (toolbar icons, "Dismiss").
pub fn ghost(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let (bg, fg) = match status {
        button::Status::Disabled => (Color::TRANSPARENT, t.muted.scale_alpha(0.4)),
        button::Status::Hovered => (t.raised, t.text),
        button::Status::Pressed => (mix(t.raised, t.primary, 0.15), t.text),
        button::Status::Active => (Color::TRANSPARENT, t.text),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: fg,
        border: border::rounded(9),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// Destructive actions.
pub fn danger(theme: &Theme, status: button::Status) -> button::Style {
    let t = tokens(theme);
    let (bg, fg) = match status {
        button::Status::Disabled => (Color::TRANSPARENT, t.muted.scale_alpha(0.4)),
        button::Status::Hovered | button::Status::Pressed => (t.danger, Color::WHITE),
        button::Status::Active => (mix(t.surface, t.danger, 0.15), t.danger),
    };
    button::Style {
        background: Some(bg.into()),
        text_color: fg,
        border: border::rounded(9),
        shadow: Shadow::default(),
        snap: true,
    }
}

/// A sidebar entry; `selected` gets the brand tint.
pub fn nav(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = tokens(theme);
        let bg = if selected {
            mix(t.surface, t.primary, if t.is_dark { 0.12 } else { 0.1 })
        } else {
            match status {
                button::Status::Hovered | button::Status::Pressed => t.raised,
                _ => Color::TRANSPARENT,
            }
        };
        button::Style {
            background: Some(bg.into()),
            text_color: if selected {
                mix(t.text, t.primary, if t.is_dark { 0.2 } else { 0.35 })
            } else {
                t.text
            },
            border: border::rounded(8),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// One choice of a segmented control.
pub fn segment(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = tokens(theme);
        if selected {
            return button::Style {
                background: Some(t.surface.into()),
                text_color: t.text,
                border: border::rounded(7).width(1).color(t.border),
                shadow: Shadow {
                    color: Color::BLACK.scale_alpha(0.12),
                    offset: Vector::new(0.0, 1.0),
                    blur_radius: 3.0,
                },
                snap: true,
            };
        }
        button::Style {
            background: None,
            text_color: if status == button::Status::Hovered { t.text } else { t.muted },
            border: border::rounded(7),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

/// The track a segmented control sits in.
pub fn segment_track(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(t.raised.into()),
        border: border::rounded(9),
        ..container::Style::default()
    }
}

/// A download row; hover and selection tint it.
pub fn row(selected: bool) -> impl Fn(&Theme, button::Status) -> button::Style {
    move |theme, status| {
        let t = tokens(theme);
        let hovered = matches!(status, button::Status::Hovered | button::Status::Pressed);
        let (bg, line) = if selected {
            if t.is_dark {
                (mix(t.surface, t.primary, 0.08), mix(t.border, t.primary, 0.55))
            } else {
                (mix(t.surface, t.primary, 0.07), t.primary)
            }
        } else if hovered {
            (mix(t.surface, t.raised, 0.7), mix(t.border, t.primary, 0.25))
        } else {
            (t.surface, t.border)
        };
        button::Style {
            background: Some(bg.into()),
            text_color: t.text,
            border: border::rounded(12).width(1).color(line),
            shadow: Shadow::default(),
            snap: true,
        }
    }
}

// ---------------------------------------------------------------- inputs

pub fn input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    let t = tokens(theme);
    let line = match status {
        text_input::Status::Focused { .. } => t.primary,
        text_input::Status::Hovered => mix(t.border, t.primary, 0.4),
        _ => t.border,
    };
    text_input::Style {
        background: if status == text_input::Status::Disabled {
            t.raised.scale_alpha(0.5).into()
        } else {
            mix(t.surface, t.raised, 0.6).into()
        },
        border: border::rounded(9).width(1).color(line),
        icon: t.muted,
        placeholder: t.muted.scale_alpha(0.8),
        value: t.text,
        selection: t.primary.scale_alpha(0.35),
    }
}

/// An input with no box of its own (the search field sits in [`search_box`]).
pub fn bare_input(theme: &Theme, status: text_input::Status) -> text_input::Style {
    text_input::Style {
        background: Color::TRANSPARENT.into(),
        border: Border::default(),
        ..input(theme, status)
    }
}

pub fn search_box(theme: &Theme) -> container::Style {
    let t = tokens(theme);
    container::Style {
        background: Some(t.surface.into()),
        border: border::rounded(10).width(1).color(t.border),
        text_color: Some(t.text),
        ..container::Style::default()
    }
}

pub fn check(theme: &Theme, status: checkbox::Status) -> checkbox::Style {
    let t = tokens(theme);
    let (checked, hovered, disabled) = match status {
        checkbox::Status::Active { is_checked } => (is_checked, false, false),
        checkbox::Status::Hovered { is_checked } => (is_checked, true, false),
        checkbox::Status::Disabled { is_checked } => (is_checked, false, true),
    };
    let fill = if checked { t.brand_a } else { mix(t.surface, t.raised, 0.6) };
    let line = if checked {
        t.brand_a
    } else if hovered {
        t.primary
    } else {
        mix(t.border, t.muted, 0.45)
    };
    checkbox::Style {
        background: (if disabled { fill.scale_alpha(0.5) } else { fill }).into(),
        icon_color: Color::WHITE,
        border: border::rounded(5).width(1.5).color(line),
        text_color: Some(t.text),
    }
}

/// A drop-down (the language list), styled like the inputs.
pub fn pick(theme: &Theme, status: pick_list::Status) -> pick_list::Style {
    let t = tokens(theme);
    let line = match status {
        pick_list::Status::Opened { .. } => t.primary,
        pick_list::Status::Hovered => mix(t.border, t.primary, 0.4),
        pick_list::Status::Active => t.border,
    };
    pick_list::Style {
        text_color: t.text,
        placeholder_color: t.muted,
        handle_color: t.muted,
        background: mix(t.surface, t.raised, 0.6).into(),
        border: border::rounded(9).width(1).color(line),
    }
}

/// The open drop-down list.
pub fn pick_menu(theme: &Theme) -> menu::Style {
    let t = tokens(theme);
    menu::Style {
        background: t.surface.into(),
        border: border::rounded(9).width(1).color(t.border),
        text_color: t.text,
        selected_text_color: t.text,
        selected_background: mix(t.surface, t.primary, if t.is_dark { 0.18 } else { 0.1 }).into(),
        shadow: soft_shadow(&t),
    }
}

/// Brand gradient on a soft track.
pub fn progress(theme: &Theme) -> progress_bar::Style {
    let t = tokens(theme);
    progress_bar::Style {
        background: t.raised.into(),
        bar: brand_gradient(&t),
        border: border::rounded(999),
    }
}

/// A finished or stopped bar in a single `tone`.
pub fn progress_in(tone: fn(&Tokens) -> Color) -> impl Fn(&Theme) -> progress_bar::Style {
    move |theme| {
        let t = tokens(theme);
        progress_bar::Style {
            background: t.raised.into(),
            bar: tone(&t).into(),
            border: border::rounded(999),
        }
    }
}

/// Thin scrollbars that only stand out on hover.
pub fn scrollbar(theme: &Theme, status: scrollable::Status) -> scrollable::Style {
    let t = tokens(theme);
    let mut style = scrollable::default(theme, status);
    let hovered =
        matches!(status, scrollable::Status::Hovered { .. } | scrollable::Status::Dragged { .. });
    for rail in [&mut style.vertical_rail, &mut style.horizontal_rail] {
        rail.background = None;
        rail.border = border::rounded(999);
        rail.scroller.background =
            (if hovered { mix(t.border, t.primary, 0.5) } else { t.border }).into();
        rail.scroller.border = border::rounded(999);
    }
    style
}

// ---------------------------------------------------------------- text and icons

pub fn muted(theme: &Theme) -> text::Style {
    text::Style { color: Some(tokens(theme).muted) }
}

pub fn tone_text(tone: fn(&Tokens) -> Color) -> impl Fn(&Theme) -> text::Style {
    move |theme| text::Style { color: Some(tone(&tokens(theme))) }
}

/// Tint a monochrome icon.
pub fn icon_in(tone: fn(&Tokens) -> Color) -> impl Fn(&Theme, svg::Status) -> svg::Style {
    move |theme, _| svg::Style { color: Some(tone(&tokens(theme))) }
}

/// Icon colored like the text around it would be.
pub fn icon_text(theme: &Theme, _status: svg::Status) -> svg::Style {
    svg::Style { color: Some(tokens(theme).text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_resolve_against_the_system_mode() {
        use iced::theme::Mode;
        assert!(ThemeChoice::System.is_dark(Mode::Dark));
        assert!(!ThemeChoice::System.is_dark(Mode::Light));
        assert!(!ThemeChoice::System.is_dark(Mode::None));
        assert!(ThemeChoice::Dark.is_dark(Mode::Light));
        assert!(!ThemeChoice::Light.is_dark(Mode::Dark));
    }

    #[test]
    fn tokens_follow_the_variant() {
        assert!(tokens(&vibe(true)).is_dark);
        assert!(!tokens(&vibe(false)).is_dark);
    }

    #[test]
    fn mix_ends() {
        let (a, b) = (Color::BLACK, Color::WHITE);
        assert_eq!(mix(a, b, 0.0), a);
        assert_eq!(mix(a, b, 1.0), b);
    }

    #[test]
    fn choice_round_trips_as_lowercase() {
        #[derive(Serialize, Deserialize)]
        struct W {
            theme: ThemeChoice,
        }
        let text = toml::to_string(&W { theme: ThemeChoice::Dark }).unwrap();
        assert_eq!(text.trim(), "theme = \"dark\"");
        let back: W = toml::from_str("theme = \"light\"").unwrap();
        assert_eq!(back.theme, ThemeChoice::Light);
    }
}
