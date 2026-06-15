//! Brand-parameterized FTL (Fluent) translations for the loader lifecycle's
//! user-facing notifications. The strings are generic — `{$brand}` is filled in
//! with the consuming project's product name — so every consumer gets the same
//! "first run", "applying update", "safe to unplug" messaging for free. Language
//! comes from the OS locale (LANG/LC_*): en-US and de-DE.

use fluent::{FluentArgs, FluentBundle, FluentResource, FluentValue};
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

/// Translate a lifecycle message key for the current locale, interpolating the
/// product `brand`. Falls back to the key.
pub fn t(key: &str, brand: &str) -> String {
    t_args(key, brand, &[])
}

/// Like [`t`], but also interpolates Fluent `{$name}` numeric variables (e.g. the
/// `done`/`total` of the provisioning progress line). `brand` is always available.
pub fn t_args(key: &str, brand: &str, args: &[(&str, i64)]) -> String {
    let (ftl, tag) = if detect_lang() == "de" { (DE_DE, "de-DE") } else { (EN_US, "en-US") };
    let langid: LanguageIdentifier = tag.parse().expect("valid langid");
    let mut bundle = FluentBundle::new(vec![langid]);
    bundle.set_use_isolating(false); // no FSI/PDI marks around plain strings
    if let Ok(res) = FluentResource::try_new(ftl.to_string()) {
        let _ = bundle.add_resource(res);
    }
    let mut fargs = FluentArgs::new();
    fargs.set("brand", FluentValue::from(brand));
    for (k, v) in args {
        fargs.set(*k, *v);
    }
    if let Some(msg) = bundle.get_message(key) {
        if let Some(pattern) = msg.value() {
            let mut errs = Vec::new();
            return bundle.format_pattern(pattern, Some(&fargs), &mut errs).into_owned();
        }
    }
    key.to_string()
}
