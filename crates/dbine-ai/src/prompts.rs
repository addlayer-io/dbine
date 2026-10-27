//! What DBine tells the AI model (the assistant's rules, the tools, the
//! optimizer's format…), in the UI language: one file per language in
//! `prompts/<lang>.json`, embedded at compile time. Spanish is the source,
//! like the UI strings; every language has the same keys, `{placeholders}`
//! and `<tags>` (a test checks it). A value is a string or an array of lines.
//!
//! The tags (`<tool>`, `<result>`, `<alternative>`…) are the same in every
//! language: DBine reads the model's answer by them.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::OnceLock;

/// The languages with a prompt file; anything else gets English.
/// The same list as scripts/i18n/languages.json (check.py verifies it).
pub const LANGS: &[&str] = &[
    "es", "en", "pt", "fr", "it", "de", "ru", "zh", "ja", "ko", "uk", "pl", "id",
];

fn source(lang: &str) -> &'static str {
    match lang {
        "es" => include_str!("../prompts/es.json"),
        "pt" => include_str!("../prompts/pt.json"),
        "fr" => include_str!("../prompts/fr.json"),
        "it" => include_str!("../prompts/it.json"),
        "de" => include_str!("../prompts/de.json"),
        "ru" => include_str!("../prompts/ru.json"),
        "zh" => include_str!("../prompts/zh.json"),
        "ja" => include_str!("../prompts/ja.json"),
        "ko" => include_str!("../prompts/ko.json"),
        "uk" => include_str!("../prompts/uk.json"),
        "pl" => include_str!("../prompts/pl.json"),
        "id" => include_str!("../prompts/id.json"),
        _ => include_str!("../prompts/en.json"),
    }
}

/// The prompt language for a UI language code ("en", "pt-BR", "es_AR",
/// "zh-TW", which gets the Simplified Chinese file): English when there's no
/// file for it.
pub fn lang(code: &str) -> &'static str {
    let base = code
        .split(['-', '_'])
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    LANGS.iter().copied().find(|l| *l == base).unwrap_or("en")
}

pub struct Prompts {
    lang: &'static str,
    texts: HashMap<String, String>,
}

fn parse(lang: &'static str) -> Prompts {
    let raw: serde_json::Map<String, Value> =
        serde_json::from_str(source(lang)).unwrap_or_else(|e| panic!("prompts/{lang}.json: {e}"));
    let texts = raw
        .into_iter()
        .map(|(k, v)| {
            let text = match v {
                Value::String(s) => s,
                Value::Array(lines) => lines
                    .iter()
                    .map(|l| {
                        l.as_str()
                            .unwrap_or_else(|| panic!("prompts/{lang}.json: {k}: not text"))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => panic!("prompts/{lang}.json: {k}: not text"),
            };
            (k, text)
        })
        .collect();
    Prompts { lang, texts }
}

/// The prompts in the language for this UI language code.
pub fn get(code: &str) -> &'static Prompts {
    static ALL: OnceLock<Vec<Prompts>> = OnceLock::new();
    let all = ALL.get_or_init(|| LANGS.iter().map(|l| parse(l)).collect());
    let l = lang(code);
    all.iter()
        .find(|p| p.lang == l)
        .expect("a prompt file per language")
}

impl Prompts {
    pub fn lang(&self) -> &'static str {
        self.lang
    }

    /// A text as is (a key no file has is a bug: it panics in tests).
    pub fn text(&self, key: &str) -> &str {
        match self.texts.get(key) {
            Some(t) => t,
            None => {
                debug_assert!(false, "prompt key {key} missing in {}", self.lang);
                ""
            }
        }
    }

    /// A text with its `{name}` placeholders filled in. One pass: a value
    /// that itself has braces (SQL, JSON) is never filled again.
    pub fn fill(&self, key: &str, values: &[(&str, &str)]) -> String {
        let t = self.text(key);
        let mut out =
            String::with_capacity(t.len() + values.iter().map(|(_, v)| v.len()).sum::<usize>());
        let mut rest = t;
        while let Some(i) = rest.find('{') {
            out.push_str(&rest[..i]);
            let after = &rest[i + 1..];
            let name_len = after
                .find(|c: char| !(c.is_ascii_lowercase() || c == '_'))
                .unwrap_or(after.len());
            let value = (name_len > 0 && after[name_len..].starts_with('}'))
                .then(|| values.iter().find(|(n, _)| *n == &after[..name_len]))
                .flatten();
            match value {
                Some((_, v)) => {
                    out.push_str(v);
                    rest = &after[name_len + 1..];
                }
                None => {
                    out.push('{');
                    rest = after;
                }
            }
        }
        out.push_str(rest);
        out
    }

    #[cfg(test)]
    fn keys(&self) -> Vec<&String> {
        let mut k: Vec<&String> = self.texts.keys().collect();
        k.sort();
        k
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn found(text: &str, open: char, close: char, ok: fn(char) -> bool) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        let mut rest = text;
        while let Some(i) = rest.find(open) {
            let after = &rest[i + 1..];
            let n = after.find(|c: char| !ok(c)).unwrap_or(after.len());
            if n > 0 && after[n..].starts_with(close) {
                out.insert(after[..n].to_string());
            }
            rest = after;
        }
        out
    }

    fn placeholders(t: &str) -> BTreeSet<String> {
        found(t, '{', '}', |c| c.is_ascii_lowercase() || c == '_')
    }

    fn tags(t: &str) -> BTreeSet<String> {
        found(t, '<', '>', |c| {
            c.is_ascii_lowercase() || c == '_' || c == '/'
        })
    }

    #[test]
    fn every_language_has_the_same_keys_placeholders_and_tags_as_spanish() {
        let es = get("es");
        assert_eq!(es.lang(), "es");
        for l in LANGS {
            let p = get(l);
            assert_eq!(p.lang(), *l);
            assert_eq!(p.keys(), es.keys(), "{l}: keys differ from es");
            for k in es.keys() {
                let (a, b) = (es.text(k), p.text(k));
                assert!(!b.trim().is_empty(), "{l}: {k} is empty");
                assert_eq!(placeholders(b), placeholders(a), "{l}: {k}: placeholders");
                assert_eq!(tags(b), tags(a), "{l}: {k}: tags");
                assert_eq!(b.lines().count(), a.lines().count(), "{l}: {k}: lines");
            }
        }
    }

    #[test]
    fn unknown_languages_get_english() {
        assert_eq!(lang("pt-BR"), "pt");
        assert_eq!(lang("es_AR"), "es");
        assert_eq!(lang("FR"), "fr");
        assert_eq!(lang("zh-TW"), "zh");
        assert_eq!(lang("ja-JP"), "ja");
        assert_eq!(lang("sv"), "en");
        assert_eq!(lang(""), "en");
        assert_eq!(get("sv").lang(), "en");
    }

    #[test]
    fn fill_replaces_only_known_placeholders_once() {
        let p = get("es");
        let s = p.fill(
            "assistant.objects_failed",
            &[("error", "falta {error} y {x}")],
        );
        assert_eq!(
            s,
            "(No se pudo listar los objetos de la base: falta {error} y {x}.)"
        );
        // The JSON examples in the tools text keep their braces.
        let t = p.fill(
            "assistant.tools",
            &[("connection", "c1"), ("database", "b1")],
        );
        assert!(t.contains("«c1»") && t.contains("«b1»"));
        assert!(t.contains("<tool>{\"name\": \"list_databases\", \"arguments\": {}}</tool>"));
        assert!(!t.contains("{connection}"));
    }

    #[test]
    fn every_language_keeps_the_rules() {
        for l in LANGS {
            let p = get(l);
            // The language rule: the file's language, unless the user writes in another.
            assert!(p.text("assistant.system").lines().count() >= 10, "{l}");
            // The optimizer and the tools protocol: what the answer is read by.
            assert!(
                p.text("optimizer.system").contains("NONE")
                    && p.text("optimizer.repair").contains("NONE"),
                "{l}"
            );
            for t in [
                "list_databases",
                "list_objects",
                "describe_object",
                "index_usage",
                "sample_rows",
                "run_query",
                "explain",
            ] {
                assert!(p.text("assistant.tools").contains(t), "{l}: {t}");
            }
        }
    }
}
