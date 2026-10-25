//! Translations.
//!
//! Each language is a TOML file under `locales/`, embedded at build time. Its
//! tables group the strings, and a string's key is `table.name`. Values may
//! hold `{name}` placeholders that [`t!`] fills in. A key missing from a
//! language falls back to English, then to the key itself.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde::{Deserialize, Serialize};

/// A language the app is translated into.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Lang {
    #[serde(rename = "en")]
    En,
    #[serde(rename = "es")]
    Es,
    #[serde(rename = "fr")]
    Fr,
    #[serde(rename = "de")]
    De,
    #[serde(rename = "pt-BR")]
    PtBr,
    #[serde(rename = "it")]
    It,
    #[serde(rename = "ru")]
    Ru,
    #[serde(rename = "tr")]
    Tr,
    #[serde(rename = "pl")]
    Pl,
    #[serde(rename = "zh-CN")]
    ZhCn,
    #[serde(rename = "ja")]
    Ja,
    #[serde(rename = "ko")]
    Ko,
}

impl Lang {
    pub const ALL: [Lang; 12] = [
        Lang::En,
        Lang::Es,
        Lang::Fr,
        Lang::De,
        Lang::PtBr,
        Lang::It,
        Lang::Ru,
        Lang::Tr,
        Lang::Pl,
        Lang::ZhCn,
        Lang::Ja,
        Lang::Ko,
    ];

    /// The language's name in that language, e.g. "Deutsch".
    pub fn native_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Es => "Español",
            Lang::Fr => "Français",
            Lang::De => "Deutsch",
            Lang::PtBr => "Português (Brasil)",
            Lang::It => "Italiano",
            Lang::Ru => "Русский",
            Lang::Tr => "Türkçe",
            Lang::Pl => "Polski",
            Lang::ZhCn => "简体中文",
            Lang::Ja => "日本語",
            Lang::Ko => "한국어",
        }
    }

    /// The best match for a BCP 47 tag such as "de-AT" or "zh-Hans-CN".
    pub fn from_tag(tag: &str) -> Option<Lang> {
        let tag = tag.trim().replace('_', "-").to_ascii_lowercase();
        let primary = tag.split('-').next().unwrap_or_default();
        Some(match primary {
            "en" => Lang::En,
            "es" => Lang::Es,
            "fr" => Lang::Fr,
            "de" => Lang::De,
            "pt" => Lang::PtBr,
            "it" => Lang::It,
            "ru" => Lang::Ru,
            "tr" => Lang::Tr,
            "pl" => Lang::Pl,
            "zh" => Lang::ZhCn,
            "ja" => Lang::Ja,
            "ko" => Lang::Ko,
            _ => return None,
        })
    }

    /// The system's language when we have it, else English.
    pub fn system() -> Lang {
        sys_locale::get_locale().as_deref().and_then(Lang::from_tag).unwrap_or(Lang::En)
    }

    fn index(self) -> usize {
        Lang::ALL.iter().position(|&l| l == self).unwrap_or(0)
    }

    fn source(self) -> &'static str {
        match self {
            Lang::En => include_str!("../locales/en.toml"),
            Lang::Es => include_str!("../locales/es.toml"),
            Lang::Fr => include_str!("../locales/fr.toml"),
            Lang::De => include_str!("../locales/de.toml"),
            Lang::PtBr => include_str!("../locales/pt-BR.toml"),
            Lang::It => include_str!("../locales/it.toml"),
            Lang::Ru => include_str!("../locales/ru.toml"),
            Lang::Tr => include_str!("../locales/tr.toml"),
            Lang::Pl => include_str!("../locales/pl.toml"),
            Lang::ZhCn => include_str!("../locales/zh-CN.toml"),
            Lang::Ja => include_str!("../locales/ja.toml"),
            Lang::Ko => include_str!("../locales/ko.toml"),
        }
    }
}

/// Every language's strings, by `table.name` key, in [`Lang::ALL`] order.
static TABLES: LazyLock<Vec<HashMap<String, String>>> = LazyLock::new(|| {
    Lang::ALL
        .iter()
        .map(|&lang| match parse(lang.source()) {
            Ok(map) => map,
            Err(err) => {
                tracing::error!(?lang, %err, "invalid translation file");
                HashMap::new()
            }
        })
        .collect()
});

/// The language in use, as an index into [`Lang::ALL`].
static CURRENT: AtomicUsize = AtomicUsize::new(0);

pub fn set_language(lang: Lang) {
    CURRENT.store(lang.index(), Ordering::Relaxed);
}

pub fn language() -> Lang {
    Lang::ALL[CURRENT.load(Ordering::Relaxed).min(Lang::ALL.len() - 1)]
}

/// The font for the current language: on Windows, the system's UI font for
/// Chinese, Japanese and Korean (the default font's fallback spaces those
/// scripts unevenly); otherwise the default.
pub fn ui_font() -> iced::Font {
    let family = match language() {
        _ if !cfg!(windows) => None,
        Lang::Ja => Some("Yu Gothic"),
        Lang::ZhCn => Some("Microsoft YaHei UI"),
        Lang::Ko => Some("Malgun Gothic"),
        _ => None,
    };
    family.map_or(iced::Font::DEFAULT, iced::Font::with_name)
}

/// `key` in the current language.
pub fn tr(key: &str) -> String {
    lookup(language(), key)
}

/// `key` in the current language, with `{name}` placeholders filled in.
pub fn tr_args(key: &str, args: &[(&str, String)]) -> String {
    fill(&tr(key), args)
}

/// `key` in `lang`, else in English, else the key itself.
pub fn lookup(lang: Lang, key: &str) -> String {
    let tables = &*TABLES;
    tables[lang.index()]
        .get(key)
        .or_else(|| tables[Lang::En.index()].get(key))
        .cloned()
        .unwrap_or_else(|| key.to_owned())
}

fn fill(template: &str, args: &[(&str, String)]) -> String {
    let mut out = template.to_owned();
    for (name, value) in args {
        out = out.replace(&format!("{{{name}}}"), value);
    }
    out
}

/// Flatten `[table] name = "text"` into `"table.name" -> "text"`.
fn parse(source: &str) -> Result<HashMap<String, String>, String> {
    let root: toml::Table = toml::from_str(source).map_err(|e| e.to_string())?;
    let mut out = HashMap::new();
    for (table, value) in root {
        let toml::Value::Table(entries) = value else {
            return Err(format!("`{table}` is not a table"));
        };
        for (name, value) in entries {
            let toml::Value::String(text) = value else {
                return Err(format!("`{table}.{name}` is not a string"));
            };
            out.insert(format!("{table}.{name}"), text);
        }
    }
    Ok(out)
}

/// A translated string: `t!("table.name")`, or with placeholders
/// `t!("table.name", count = n, name = file)`.
macro_rules! t {
    ($key:literal) => {
        $crate::i18n::tr($key)
    };
    ($key:literal, $($name:ident = $value:expr),+ $(,)?) => {
        $crate::i18n::tr_args($key, &[$((stringify!($name), $value.to_string())),+])
    };
}
pub(crate) use t;

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    fn placeholders(text: &str) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = text;
        while let Some(start) = rest.find('{') {
            let Some(len) = rest[start..].find('}') else { break };
            out.insert(rest[start..start + len + 1].to_owned());
            rest = &rest[start + len + 1..];
        }
        out
    }

    #[test]
    fn every_language_parses_and_matches_english() {
        let english = parse(Lang::En.source()).unwrap();
        for lang in Lang::ALL {
            let table = parse(lang.source()).unwrap_or_else(|e| panic!("{lang:?}: {e}"));
            for key in table.keys() {
                assert!(english.contains_key(key), "{lang:?} has unknown key {key}");
            }
            for (key, text) in &english {
                let Some(translated) = table.get(key) else {
                    panic!("{lang:?} is missing {key}");
                };
                assert!(!translated.trim().is_empty(), "{lang:?} {key} is empty");
                assert_eq!(
                    placeholders(translated),
                    placeholders(text),
                    "{lang:?} {key}: placeholders differ"
                );
            }
        }
    }

    /// Every `t!("...")` key in the source exists in English.
    #[test]
    fn keys_used_in_code_exist() {
        let english = parse(Lang::En.source()).unwrap();
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = vec![dir.clone()];
        let mut checked = 0;
        while let Some(path) = files.pop() {
            if path.is_dir() {
                files.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("i18n.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            for (at, _) in source.match_indices("t!(\"") {
                // `t!` itself, not the end of `format!` or `print!`.
                let before = source[..at].chars().next_back();
                if before.is_some_and(|c| c.is_alphanumeric() || c == '_') {
                    continue;
                }
                let key = source[at + 4..].split('"').next().unwrap();
                assert!(english.contains_key(key), "{}: unknown key {key}", path.display());
                checked += 1;
            }
        }
        assert!(checked > 50, "only found {checked} keys");
    }

    #[test]
    fn tags_map_to_languages() {
        assert_eq!(Lang::from_tag("de-AT"), Some(Lang::De));
        assert_eq!(Lang::from_tag("pt-PT"), Some(Lang::PtBr));
        assert_eq!(Lang::from_tag("zh-Hans-CN"), Some(Lang::ZhCn));
        assert_eq!(Lang::from_tag("en_US"), Some(Lang::En));
        assert_eq!(Lang::from_tag("ko"), Some(Lang::Ko));
        assert_eq!(Lang::from_tag("xx"), None);
    }

    #[test]
    fn lookup_falls_back_and_fills() {
        assert_eq!(lookup(Lang::En, "common.cancel"), "Cancel");
        assert_eq!(lookup(Lang::De, "no.such.key"), "no.such.key");
        assert_eq!(fill("{a} and {b}", &[("a", "1".into()), ("b", "2".into())]), "1 and 2");
        // Placeholders that aren't arguments stay as they are.
        assert_eq!(fill("run {path}", &[("x", "1".into())]), "run {path}");
    }

    #[test]
    fn language_codes_round_trip() {
        #[derive(Serialize, Deserialize)]
        struct W {
            language: Lang,
        }
        for lang in Lang::ALL {
            let text = toml::to_string(&W { language: lang }).unwrap();
            assert_eq!(toml::from_str::<W>(&text).unwrap().language, lang);
        }
        assert_eq!(
            toml::to_string(&W { language: Lang::PtBr }).unwrap().trim(),
            "language = \"pt-BR\""
        );
    }
}
