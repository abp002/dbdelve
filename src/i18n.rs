//! Interface language. The English text is the key: a string with no entry in
//! the catalogue, or a catalogue that falls behind a reworded label, shows the
//! English rather than nothing, so a translation can only ever be incomplete,
//! never wrong about which control it names.
//!
//! ponytail: the language is chosen once at startup (`init`), so changing it in
//! Settings takes effect on the next launch. Live switching would mean every
//! view re-reading its labels on a global's change.

use std::collections::HashMap;
use std::fmt::Display;
use std::sync::OnceLock;

/// Every language the interface can be shown in, by the code `profiles.toml`
/// stores. English is the source text and has no catalogue.
pub(crate) const LANGUAGES: [(&str, &str); 2] = [("en", "English"), ("es", "Español")];

const SPANISH: &str = include_str!("../locales/es.toml");

static CATALOGUE: OnceLock<HashMap<String, String>> = OnceLock::new();

/// Picks the language: the one saved in Settings, else `DBDELVE_LANG`, else
/// the system's. Called once, before the first window.
pub(crate) fn init(saved: Option<&str>) {
    let code = saved
        .map(str::to_owned)
        .or_else(|| std::env::var("DBDELVE_LANG").ok())
        .or_else(sys_locale::get_locale)
        .unwrap_or_default();
    let catalogue = match resolve(&code) {
        "es" => parse(SPANISH),
        _ => HashMap::new(),
    };
    let _ = CATALOGUE.set(catalogue);
}

/// The code of a supported language a locale tag such as `es-ES` or `es_AR`
/// belongs to, English for any other.
pub(crate) fn resolve(tag: &str) -> &'static str {
    let primary = tag.split(['-', '_', '.']).next().unwrap_or("");
    LANGUAGES
        .iter()
        .map(|(code, _)| *code)
        .find(|code| code.eq_ignore_ascii_case(primary))
        .unwrap_or("en")
}

fn parse(text: &str) -> HashMap<String, String> {
    toml::from_str(text).expect("the bundled catalogue is valid TOML")
}

/// The interface text for `english` in the chosen language.
pub(crate) fn tr(english: &'static str) -> &'static str {
    CATALOGUE
        .get()
        .and_then(|catalogue| catalogue.get(english))
        .map_or(english, |translated| translated.as_str())
}

/// `template` with each `{}` replaced, in order, by the next of `args`. The
/// translated template may not move arguments, which keeps the catalogue to
/// plain text a translator can edit without knowing Rust.
pub(crate) fn fill(template: &str, args: &[&dyn Display]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut args = args.iter();
    let mut rest = template;
    while let Some(at) = rest.find("{}") {
        out.push_str(&rest[..at]);
        if let Some(arg) = args.next() {
            out.push_str(&arg.to_string());
        }
        rest = &rest[at + 2..];
    }
    out.push_str(rest);
    out
}

/// `format!` over a translated template: `trf!("Row {} of {}", row, total)`.
/// Positional `{}` only, for the reason on [`fill`].
macro_rules! trf {
    ($template:literal $(, $arg:expr)* $(,)?) => {
        $crate::i18n::fill($crate::i18n::tr($template), &[$(&$arg as &dyn std::fmt::Display),*])
    };
}
pub(crate) use trf;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_tags_resolve_to_their_language() {
        assert_eq!(resolve("es-ES"), "es");
        assert_eq!(resolve("es_AR.UTF-8"), "es");
        assert_eq!(resolve("ES"), "es");
        assert_eq!(resolve("en-GB"), "en");
        assert_eq!(resolve("fr-FR"), "en");
        assert_eq!(resolve(""), "en");
    }

    #[test]
    fn fill_substitutes_in_order_and_tolerates_a_short_list() {
        assert_eq!(fill("Fila {} de {}", &[&3, &10]), "Fila 3 de 10");
        assert_eq!(fill("{} filas", &[]), " filas");
    }

    /// Each translation keeps its source's placeholders, so `trf!` never
    /// drops an argument or prints a stray `{}`.
    #[test]
    fn spanish_keeps_every_placeholder() {
        for (english, spanish) in parse(SPANISH) {
            assert_eq!(
                english.matches("{}").count(),
                spanish.matches("{}").count(),
                "{english:?} -> {spanish:?}"
            );
        }
    }

    /// A key no `tr`/`trf!` call names any more is a label upstream reworded:
    /// the control now shows English, and this is what says so.
    #[test]
    fn every_spanish_key_is_still_used() {
        let mut source = String::new();
        let mut stack = vec![std::path::PathBuf::from(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src"
        ))];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().is_some_and(|ext| ext == "rs") {
                    source.push_str(&std::fs::read_to_string(path).unwrap());
                }
            }
        }
        let stale: Vec<_> = parse(SPANISH)
            .into_keys()
            .filter(|key| !source.contains(&format!("{key:?}")))
            .collect();
        assert!(stale.is_empty(), "unused catalogue keys: {stale:?}");
    }
}
