//! FTL (Fluent) translations for the launcher's user-facing notifications.
//! Mirrors the SPA's dioxus-i18n FTL approach; the launcher isn't Dioxus, so it
//! uses fluent directly. Language comes from the OS locale (LANG/LC_*) — en-US
//! and de-DE, matching the SPA's locales.

use fluent::{FluentBundle, FluentResource};
use unic_langid::LanguageIdentifier;

const EN_US: &str = include_str!("i18n/en-US.ftl");
const DE_DE: &str = include_str!("i18n/de-DE.ftl");

/// "de" if the OS locale starts with German, else "en".
fn detect_lang() -> &'static str {
    for key in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.to_ascii_lowercase();
            if v.starts_with("de") {
                return "de";
            }
            if !v.is_empty() && v != "c" && v != "posix" {
                return "en";
            }
        }
    }
    "en"
}

/// Translate an FTL message key for the current locale. Falls back to the key.
pub fn t(key: &str) -> String {
    let (ftl, tag) = if detect_lang() == "de" { (DE_DE, "de-DE") } else { (EN_US, "en-US") };
    let langid: LanguageIdentifier = tag.parse().expect("valid langid");
    let mut bundle = FluentBundle::new(vec![langid]);
    bundle.set_use_isolating(false); // no FSI/PDI marks around plain strings
    if let Ok(res) = FluentResource::try_new(ftl.to_string()) {
        let _ = bundle.add_resource(res);
    }
    if let Some(msg) = bundle.get_message(key) {
        if let Some(pattern) = msg.value() {
            let mut errs = Vec::new();
            return bundle.format_pattern(pattern, None, &mut errs).into_owned();
        }
    }
    key.to_string()
}
