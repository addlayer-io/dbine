//! Backend messages in the UI language, for what a task sends out of DBine
//! (a message's `{results}` and `{alerts}`), which runs with DBine closed and
//! so can't go through the UI's translation (web/src/i18n/backend.ts).
//!
//! Same catalogs and rules as the UI: `web/src/locales/<lang>/backend.json`
//! maps a Spanish message to its translation; a message made with `format!`
//! is a pattern whose values (`{0}`, `{1}`…) are translated again; a message
//! of several lines is translated line by line; what isn't in the catalog
//! stays as it came. The catalogs are parsed only when a task sends a message
//! in that language, once per run of DBine.

use std::collections::HashMap;
use std::sync::OnceLock;

use crate::state::AppState;

/// The synced preference the UI writes (Configuración › General › Idioma).
const SETTING_KEY: &str = "ui.language";

/// The UI languages other than Spanish (the source), as in
/// scripts/i18n/languages.json.
const CATALOGS: [(&str, &str); 9] = [
    ("en", include_str!("../../../web/src/locales/en/backend.json")),
    ("pt", include_str!("../../../web/src/locales/pt/backend.json")),
    ("fr", include_str!("../../../web/src/locales/fr/backend.json")),
    ("it", include_str!("../../../web/src/locales/it/backend.json")),
    ("de", include_str!("../../../web/src/locales/de/backend.json")),
    ("ru", include_str!("../../../web/src/locales/ru/backend.json")),
    ("zh", include_str!("../../../web/src/locales/zh/backend.json")),
    ("ja", include_str!("../../../web/src/locales/ja/backend.json")),
    ("ko", include_str!("../../../web/src/locales/ko/backend.json")),
];

/// The UI language: the one picked in Settings, else the system's when it's
/// one of DBine's, else English (as the UI does).
pub fn ui_language(state: &AppState) -> String {
    let picked = state.store.get_setting(SETTING_KEY).ok().flatten();
    if let Some(code) = picked.as_ref().and_then(|v| v.as_str()).and_then(lang_of_tag) {
        return code.to_string();
    }
    sys_locale::get_locales().find_map(|t| lang_of_tag(&t)).unwrap_or("en").to_string()
}

/// Ours for a BCP 47 tag (`pt-BR`, `zh-Hant-TW`, `de_AT`): its primary subtag.
fn lang_of_tag(tag: &str) -> Option<&'static str> {
    let primary = tag.split(['-', '_']).next()?.to_ascii_lowercase();
    if primary == "es" {
        return Some("es");
    }
    CATALOGS.iter().map(|(c, _)| *c).find(|c| *c == primary)
}

/// Longer lines aren't matched against the patterns (only looked up whole):
/// a summary can be a server's error echoing data, and matching is costly.
const MAX_PATTERN_LINE: usize = 600;

/// The most matching steps one message may take, whatever its shape, so text
/// crafted to make the matcher backtrack can't stall a task.
const MAX_STEPS: usize = 200_000;

struct Pattern {
    /// Literal text and placeholder numbers, in order.
    parts: Vec<Part>,
    to: String,
    literal: usize,
}

enum Part {
    Text(String),
    Value(String),
}

struct Catalog {
    exact: HashMap<String, String>,
    patterns: Vec<Pattern>,
}

fn catalog(lang: &str) -> Option<&'static Catalog> {
    // Only the languages asked for are parsed: a OnceLock per language.
    static SLOTS: [OnceLock<Option<Catalog>>; 9] = [const { OnceLock::new() }; 9];
    let i = CATALOGS.iter().position(|(c, _)| *c == lang)?;
    SLOTS[i].get_or_init(|| compile(CATALOGS[i].1)).as_ref()
}

fn compile(json: &str) -> Option<Catalog> {
    let map: HashMap<String, String> = serde_json::from_str(json).ok()?;
    let mut exact = HashMap::new();
    let mut patterns = Vec::new();
    for (from, to) in map {
        if to.is_empty() {
            continue;
        }
        let parts = split_placeholders(&from);
        if !parts.iter().any(|p| matches!(p, Part::Value(_))) {
            exact.insert(from, to);
            continue;
        }
        let literal = parts.iter().map(|p| if let Part::Text(t) = p { t.chars().count() } else { 0 }).sum();
        patterns.push(Pattern { parts, to, literal });
    }
    // The most specific first (more fixed text wins over a looser pattern).
    patterns.sort_by(|a, b| b.literal.cmp(&a.literal));
    Some(Catalog { exact, patterns })
}

/// `"no existe la base {0}"` → [Text("no existe la base "), Value("0")].
fn split_placeholders(s: &str) -> Vec<Part> {
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut rest = s;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
        if !digits.is_empty() && after[digits.len()..].starts_with('}') {
            text.push_str(&rest[..open]);
            if !text.is_empty() {
                parts.push(Part::Text(std::mem::take(&mut text)));
            }
            parts.push(Part::Value(digits.clone()));
            rest = &after[digits.len() + 1..];
        } else {
            text.push_str(&rest[..open + 1]);
            rest = after;
        }
    }
    text.push_str(rest);
    if !text.is_empty() {
        parts.push(Part::Text(text));
    }
    parts
}

/// Matches `msg` against a pattern, each value as short as possible (like
/// the UI's lazy `[\s\S]*?` groups, anchored at both ends). A value ends
/// only where the text after it starts. `budget` counts the steps; when it
/// runs out the match fails (the message stays untranslated).
fn capture<'a>(parts: &[Part], msg: &'a str, out: &mut Vec<(String, &'a str)>, budget: &mut usize) -> bool {
    if *budget == 0 {
        return false;
    }
    *budget -= 1;
    match parts.split_first() {
        None => msg.is_empty(),
        Some((Part::Text(t), rest)) => msg.strip_prefix(t.as_str()).is_some_and(|m| capture(rest, m, out, budget)),
        Some((Part::Value(n), rest)) => {
            let ends: Vec<usize> = match rest.first() {
                // The last value takes the rest.
                None => vec![msg.len()],
                Some(Part::Text(next)) => msg.match_indices(next.as_str()).map(|(i, _)| i).collect(),
                // Two values in a row: any boundary.
                Some(Part::Value(_)) => msg.char_indices().map(|(i, _)| i).chain(std::iter::once(msg.len())).collect(),
            };
            for end in ends {
                out.push((n.clone(), &msg[..end]));
                if capture(rest, &msg[end..], out, budget) {
                    return true;
                }
                out.pop();
                if *budget == 0 {
                    return false;
                }
            }
            false
        }
    }
}

fn translate_with(msg: &str, c: &Catalog, depth: u8, budget: &mut usize) -> String {
    if let Some(hit) = c.exact.get(msg) {
        return hit.clone();
    }
    if depth > 3 {
        return msg.to_string();
    }
    if msg.contains('\n') {
        return msg.split('\n').map(|l| translate_with(l, c, depth + 1, budget)).collect::<Vec<_>>().join("\n");
    }
    if msg.len() > MAX_PATTERN_LINE {
        return msg.to_string();
    }
    for p in &c.patterns {
        let mut values = Vec::new();
        if *budget == 0 {
            break;
        }
        if !capture(&p.parts, msg, &mut values, budget) {
            continue;
        }
        let mut out = p.to.clone();
        for (n, v) in values {
            let v = if v.is_empty() { String::new() } else { translate_with(v, c, depth + 1, budget) };
            out = out.replace(&format!("{{{n}}}"), &v);
        }
        return out;
    }
    msg.to_string()
}

/// A backend message in `lang` (as it came when unknown or Spanish).
pub fn translate(lang: &str, msg: &str) -> String {
    match catalog(lang) {
        Some(c) if !msg.is_empty() => translate_with(msg, c, 0, &mut MAX_STEPS.clone()),
        _ => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_patterns_and_lines() {
        assert_eq!(translate("en", "No se ejecutó: ningún paso anterior falló."), "Not run: no previous step failed.");
        assert_eq!(translate("en", "Mensaje enviado a Slack"), "Message sent to Slack");
        // Line by line, and what isn't in the catalog stays as it came.
        assert_eq!(
            translate("en", "No se ejecutó: ningún paso anterior falló.\nERROR 42P01: relation \"x\" does not exist"),
            "Not run: no previous step failed.\nERROR 42P01: relation \"x\" does not exist"
        );
        assert_eq!(translate("es", "Mensaje enviado a Slack"), "Mensaje enviado a Slack");
        assert_eq!(translate("xx", "Mensaje enviado a Slack"), "Mensaje enviado a Slack");
    }

    #[test]
    fn every_language_loads() {
        for (code, _) in CATALOGS {
            assert!(catalog(code).is_some_and(|c| !c.exact.is_empty()), "{code}");
        }
    }

    #[test]
    fn tags_map_to_ui_languages() {
        assert_eq!(lang_of_tag("pt-BR"), Some("pt"));
        assert_eq!(lang_of_tag("zh-Hant-TW"), Some("zh"));
        assert_eq!(lang_of_tag("es_AR"), Some("es"));
        assert_eq!(lang_of_tag("nl-NL"), None);
    }

    #[test]
    fn crafted_text_cannot_stall_the_matcher() {
        // Separators repeated to make a three-value pattern backtrack: it
        // returns quickly, the text as it came.
        let evil = format!("ERROR: invalid input syntax for type integer: \"{}\"", " de (".repeat(2000));
        let start = std::time::Instant::now();
        assert_eq!(translate("en", &evil), evil);
        let short = " de (".repeat(110);
        let _ = translate("en", &short);
        assert!(start.elapsed() < std::time::Duration::from_secs(2), "{:?}", start.elapsed());
    }

    #[test]
    fn placeholders_split_and_match_lazily() {
        let parts = split_placeholders("de {0} a {1}.");
        let mut v = Vec::new();
        assert!(capture(&parts, "de a a b a c.", &mut v, &mut MAX_STEPS.clone()));
        assert_eq!(v.iter().map(|(_, s)| *s).collect::<Vec<_>>(), ["a", "b a c"]);
        assert!(matches!(split_placeholders("{x} {12}")[..], [Part::Text(_), Part::Value(ref n)] if n == "12"));
    }
}
