//! Multi-language (i18n) string tables and lookup logic.
//!
//! Translation tables for every supported language are embedded at compile
//! time from `i18n/*.json` (see [`TABLES`]) — there is no runtime file I/O
//! for translations, only for the `AppConfig.language` preference which is
//! read/written by `config.rs`. [`Translations`] holds all loaded tables in
//! memory plus which language is currently active, and provides key lookup
//! with fallback to English and then to the raw key (so a missing
//! translation is visibly wrong rather than silently blank).
//!
//! `controller.rs` owns the single [`Translations`] instance for the app
//! (behind a `std::sync::Mutex`, since it's read/written from both UI
//! callbacks and background tasks), calls [`Translations::snapshot`] to
//! build the table that `tr_generated::apply` uses to populate the Slint
//! `Tr` global, and calls [`detect_system_language`] once at startup when no
//! language preference has been saved yet.

use std::collections::HashMap;

/// Supported language codes paired with their human-readable display names,
/// in the order they appear in the UI's language picker. `set_language*`
/// methods and [`detect_system_language`] only ever select from this list.
pub const LANGUAGES: &[(&str, &str)] = &[("en", "English"), ("zh", "中文")];

/// Raw JSON translation tables embedded at compile time, one per supported
/// language, keyed by the same language code as [`LANGUAGES`].
const TABLES: &[(&str, &str)] = &[
    ("en", include_str!("../i18n/en.json")),
    ("zh", include_str!("../i18n/zh.json")),
];

/// Holds every embedded translation table in memory along with the
/// currently active language.
pub struct Translations {
    tables: HashMap<&'static str, HashMap<String, String>>,
    current: &'static str,
}

impl Translations {
    /// Parses every embedded JSON translation table into memory, defaulting
    /// the active language to English.
    ///
    /// Panics if an embedded `i18n/*.json` file fails to parse as a
    /// `HashMap<String, String>` — this is treated as a build-time
    /// programming error (a malformed translation file shipped with the
    /// binary) rather than a recoverable runtime condition.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use crate::i18n::Translations;
    ///
    /// let translations = Translations::load();
    /// assert_eq!(translations.current(), "en");
    /// ```
    pub fn load() -> Self {
        let tables = TABLES
            .iter()
            .map(|(code, json)| {
                let table: HashMap<String, String> = serde_json::from_str(json)
                    .unwrap_or_else(|e| panic!("embedded translation {code}.json is invalid: {e}"));
                (*code, table)
            })
            .collect();

        Self {
            tables,
            current: "en",
        }
    }

    /// Returns the currently active language code (e.g. `"en"`, `"zh"`).
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut translations = crate::i18n::Translations::load();
    /// translations.set_language("zh");
    /// assert_eq!(translations.current(), "zh");
    /// ```
    pub fn current(&self) -> &'static str {
        self.current
    }

    /// Returns the index of the currently active language within
    /// [`LANGUAGES`], for driving the UI's language picker selection.
    /// Defaults to `0` in the (unreachable in practice) case the current
    /// code is not found.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut translations = crate::i18n::Translations::load();
    /// translations.set_language("zh");
    /// assert_eq!(crate::i18n::LANGUAGES[translations.current_index()].0, "zh");
    /// ```
    pub fn current_index(&self) -> usize {
        LANGUAGES
            .iter()
            .position(|(code, _)| *code == self.current)
            .unwrap_or(0)
    }

    /// Switches the active language to `code`, if it is one of the
    /// supported [`LANGUAGES`]; unrecognized codes are silently ignored,
    /// leaving the previous language active.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut translations = crate::i18n::Translations::load();
    /// translations.set_language("zh");
    /// translations.set_language("klingon"); // ignored
    /// assert_eq!(translations.current(), "zh");
    /// ```
    pub fn set_language(&mut self, code: &str) {
        if let Some((known, _)) = LANGUAGES.iter().find(|(c, _)| *c == code) {
            self.current = known;
        }
    }

    /// Switches the active language by its index into [`LANGUAGES`] (as
    /// used by the UI's language picker); an out-of-range index is
    /// silently ignored.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut translations = crate::i18n::Translations::load();
    /// translations.set_language_by_index(1);
    /// assert_eq!(translations.current(), crate::i18n::LANGUAGES[1].0);
    /// ```
    pub fn set_language_by_index(&mut self, index: usize) {
        if let Some((code, _)) = LANGUAGES.get(index) {
            self.current = code;
        }
    }

    /// Looks up `key` in the active language's table, falling back to the
    /// English table if the key is missing there, and finally to the raw
    /// `key` string itself if it's missing from every table. Returning the
    /// key rather than an empty string on a total miss makes a missing
    /// translation visibly obvious in the UI instead of rendering blank.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut translations = crate::i18n::Translations::load();
    /// translations.set_language("zh");
    /// assert_eq!(translations.get("tabSettings"), "设置");
    /// assert_eq!(translations.get("no-such-key"), "no-such-key");
    /// ```
    pub fn get<'a>(&'a self, key: &'a str) -> &'a str {
        self.tables
            .get(self.current)
            .and_then(|table| table.get(key))
            .or_else(|| self.tables.get("en").and_then(|table| table.get(key)))
            .map(String::as_str)
            .unwrap_or(key)
    }

    /// Looks up `key` (with the same fallback behavior as [`get`](Self::get))
    /// and substitutes each `{name}` placeholder in the resulting string
    /// with its corresponding value from `args`.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let translations = crate::i18n::Translations::load();
    /// let text = translations.format("eventsCaptured", &[("count", "42")]);
    /// assert!(text.contains("42"));
    /// ```
    pub fn format(&self, key: &str, args: &[(&str, &str)]) -> String {
        let mut text = self.get(key).to_string();
        for (name, value) in args {
            text = text.replace(&format!("{{{name}}}"), value);
        }
        text
    }

    /// Builds a flat key -> string map representing the active language,
    /// used as the lookup source for `tr_generated::apply` when populating
    /// the Slint `Tr` global.
    ///
    /// Starts from the full English table (guaranteeing every key the UI
    /// expects has *some* value) and then overlays every key present in the
    /// active language's table on top, so a language with partial coverage
    /// still renders complete text (falling back per-key to English) rather
    /// than gaps.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let mut translations = crate::i18n::Translations::load();
    /// translations.set_language("zh");
    /// let table = translations.snapshot();
    /// assert_eq!(table["tabSettings"], "设置");
    /// ```
    pub fn snapshot(&self) -> HashMap<String, String> {
        let english = self.tables.get("en");
        let mut table = english.cloned().unwrap_or_default();
        if let Some(current) = self.tables.get(self.current) {
            for (key, value) in current {
                table.insert(key.clone(), value.clone());
            }
        }
        table
    }
}

/// Best-effort detection of the user's OS-level locale, mapped down to one
/// of the supported [`LANGUAGES`]; falls back to `"en"` if detection fails
/// or the detected locale isn't one Lanius has translations for. Used once
/// at first-run bootstrap, before any `language` preference has been saved
/// to `AppConfig`.
///
/// # Examples
///
/// ```ignore
/// let mut translations = crate::i18n::Translations::load();
/// translations.set_language(crate::i18n::detect_system_language());
/// ```
pub fn detect_system_language() -> &'static str {
    let raw = system_locale().to_ascii_lowercase();
    let primary = raw.split(['-', '_', '.']).next().unwrap_or("");
    LANGUAGES
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == primary)
        .unwrap_or("en")
}

/// Reads the raw OS locale string using whatever mechanism is available on
/// the current platform: on macOS, shells out to `defaults read -g
/// AppleLocale` (spawning a subprocess); on Windows, asks the OS for the
/// user's default locale name (GUI apps there rarely see `LANG`); on every
/// platform, then falls back to the POSIX `LC_ALL`/`LC_MESSAGES`/`LANG`
/// environment variables, checked in that priority order. Returns an empty
/// string if nothing usable is found.
fn system_locale() -> String {
    #[cfg(target_os = "windows")]
    if let Some(value) = crate::windows::user_locale() {
        return value;
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(output) = std::process::Command::new("defaults")
            .args(["read", "-g", "AppleLocale"])
            .output()
        {
            if output.status.success() {
                let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
                if !value.is_empty() {
                    return value;
                }
            }
        }
    }

    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() {
                return value;
            }
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_language_has_the_full_english_key_set() {
        let translations = Translations::load();
        let english = translations
            .tables
            .get("en")
            .expect("english table present");

        for (code, _) in LANGUAGES {
            let table = translations
                .tables
                .get(code)
                .unwrap_or_else(|| panic!("missing table for {code}"));
            for key in english.keys() {
                assert!(
                    table.contains_key(key),
                    "language {code} is missing key {key}"
                );
            }
        }
    }

    #[test]
    fn lookup_falls_back_to_english_then_to_the_key() {
        let mut translations = Translations::load();
        translations.set_language("zh");
        assert_eq!(translations.get("tabSettings"), "设置");
        assert_eq!(
            translations.get("no-such-key"),
            "no-such-key",
            "unknown keys must be visible, not empty"
        );
    }

    #[test]
    fn unknown_language_codes_are_ignored() {
        let mut translations = Translations::load();
        translations.set_language("zh");
        translations.set_language("klingon");
        assert_eq!(
            translations.current(),
            "zh",
            "bad codes must not reset state"
        );
        translations.set_language_by_index(999);
        assert_eq!(translations.current(), "zh");
    }

    #[test]
    fn placeholders_are_substituted() {
        let mut translations = Translations::load();
        translations.set_language("en");
        let text = translations.format("eventsCaptured", &[("count", "42")]);
        assert!(text.contains("42"), "got {text:?}");
        assert!(!text.contains("{count}"));
    }

    #[test]
    fn language_index_round_trips() {
        let mut translations = Translations::load();
        for (index, (code, _)) in LANGUAGES.iter().enumerate() {
            translations.set_language(code);
            assert_eq!(translations.current_index(), index);
        }
    }

    #[test]
    fn detected_language_is_always_supported() {
        let detected = detect_system_language();
        assert!(LANGUAGES.iter().any(|(code, _)| *code == detected));
    }
}
