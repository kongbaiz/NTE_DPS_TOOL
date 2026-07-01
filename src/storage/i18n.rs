//! Lightweight UI localization.
//!
//! The source keeps every user-facing string in **English**; that English text is
//! the lookup key. A localized build overlays a `res/languages/<code>.json` map of
//! `"English key" -> "localized value"`. English therefore needs no file: when the
//! active language is [`Language::English`] (or a key is missing from a locale map)
//! the key itself is returned unchanged.
//!
//! Game-specific proper nouns and descriptions were sourced from the official
//! `NTE_Assets` localization when a match existed, and left at their original value
//! otherwise; both simply live as entries in the locale JSON.
//!
//! The store is a process-wide [`RwLock`] because egui draws immediate-mode on the
//! UI thread while background workers may also format status text. Swapping the
//! language (settings dropdown / startup) reloads the map in place.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, RwLock};

use serde::{Deserialize, Serialize};

use crate::storage::resource::read_resource_text;

/// Languages the UI can render. English is the key language; every other variant
/// has a matching `res/languages/<code>.json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Language {
    #[serde(rename = "en")]
    English,
    /// Default so existing (Chinese-only) installs keep their current UI; English
    /// is opt-in via the settings dropdown.
    #[default]
    #[serde(rename = "zh-CN")]
    SimplifiedChinese,
}

const LANGUAGES: [Language; 2] = [Language::English, Language::SimplifiedChinese];

impl Language {
    pub fn all() -> &'static [Self] {
        &LANGUAGES
    }

    /// Stable code used for the config value and the `res/languages/<code>.json` filename.
    pub fn code(self) -> &'static str {
        match self {
            Self::English => "en",
            Self::SimplifiedChinese => "zh-CN",
        }
    }

    /// Endonym shown in the language dropdown, written in the language itself so a
    /// user can find their language without already reading the current one.
    pub fn native_name(self) -> &'static str {
        match self {
            Self::English => "English",
            Self::SimplifiedChinese => "简体中文",
        }
    }

    /// Resource path of the overlay map, or `None` for the key language (English).
    fn resource_path(self) -> Option<String> {
        match self {
            Self::English => None,
            other => Some(format!("res/languages/{}.json", other.code())),
        }
    }
}

#[derive(Default)]
struct Store {
    language: Language,
    /// `"English key" -> "localized value"`; empty for English.
    map: HashMap<String, String>,
}

static STORE: LazyLock<RwLock<Store>> = LazyLock::new(|| RwLock::new(Store::default()));

/// Load the overlay map for `language`. Missing/invalid files degrade to an empty
/// map (keys fall back to their English text) rather than failing startup.
fn load_map(language: Language) -> HashMap<String, String> {
    let Some(path) = language.resource_path() else {
        return HashMap::new();
    };
    read_resource_text(Path::new(&path))
        .ok()
        .and_then(|text| serde_json::from_str::<HashMap<String, String>>(&text).ok())
        .unwrap_or_default()
}

/// Switch the active UI language and load its overlay map. Call once at startup and
/// whenever the settings dropdown changes.
pub fn set_language(language: Language) {
    let map = load_map(language);
    let mut store = STORE.write().expect("i18n store lock poisoned");
    store.language = language;
    store.map = map;
}

/// Translate an English key into the active language. Returns the key unchanged for
/// English or when the locale map has no entry for it.
pub fn t(key: &str) -> String {
    let store = STORE.read().expect("i18n store lock poisoned");
    if matches!(store.language, Language::English) {
        return key.to_owned();
    }
    match store.map.get(key) {
        Some(value) => value.clone(),
        None => key.to_owned(),
    }
}

/// Translate `key`, then substitute each `{}` placeholder left-to-right with `args`.
///
/// Runtime substitution (rather than `format!`) is required because the template
/// text is chosen at runtime from the locale map. Extra `{}` beyond `args` are left
/// literal; unused `args` are dropped.
pub fn tf(key: &str, args: &[&str]) -> String {
    let template = t(key);
    let mut out = String::with_capacity(template.len());
    let mut args = args.iter();
    let mut chars = template.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '{' && chars.peek() == Some(&'}') {
            chars.next();
            match args.next() {
                Some(arg) => out.push_str(arg),
                None => out.push_str("{}"),
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_codes_and_names_are_stable() {
        assert_eq!(Language::English.code(), "en");
        assert_eq!(Language::SimplifiedChinese.code(), "zh-CN");
        assert_eq!(Language::English.native_name(), "English");
        assert_eq!(Language::SimplifiedChinese.native_name(), "简体中文");
        assert_eq!(Language::default(), Language::SimplifiedChinese);
    }

    #[test]
    fn english_returns_key_unchanged() {
        set_language(Language::English);
        assert_eq!(t("Settings"), "Settings");
        assert_eq!(tf("Loaded {} rows", &["12"]), "Loaded 12 rows");
    }

    #[test]
    fn config_serializes_to_stable_codes() {
        assert_eq!(
            serde_json::to_string(&Language::SimplifiedChinese).unwrap(),
            "\"zh-CN\""
        );
        assert_eq!(
            serde_json::from_str::<Language>("\"en\"").unwrap(),
            Language::English
        );
    }

    #[test]
    fn tf_leaves_extra_placeholders_literal() {
        set_language(Language::English);
        assert_eq!(tf("{} of {}", &["3"]), "3 of {}");
    }
}
