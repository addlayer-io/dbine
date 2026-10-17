//! "Pedir alternativas a la IA": what goes to the configured AI (the query,
//! the structure of the tables it mentions, a summary of the plan; never
//! rows) and how its answer is read. The answer is a proposal: each
//! alternative becomes a candidate marked "IA" that the user compares like
//! any other, and nothing runs unless the user clicks.

use super::{Candidate, Source};
use dbine_ai::prompts::Prompts;
use dbine_driver::TableSchema;

/// At most this many alternatives are taken from an answer.
pub const MAX: usize = 3;

/// What the model is told: the rules and the answer's format, in the UI
/// language (`prompts/<lang>.json` in dbine-ai).
pub fn system_prompt(p: &Prompts, engine: &str, language: &str) -> String {
    p.fill("optimizer.system", &[("engine", engine), ("language", language), ("max", &MAX.to_string())])
}

/// The query, the structure and the plan, in tags (the same in every language).
pub fn user_prompt(query: &str, structure: &str, plan: &str) -> String {
    let mut s = format!("<original_query>\n{}\n</original_query>\n", query.trim());
    if !structure.trim().is_empty() {
        s.push_str(&format!("\n<structure>\n{structure}</structure>\n"));
    }
    if !plan.trim().is_empty() {
        s.push_str(&format!("\n<plan>\n{plan}\n</plan>\n"));
    }
    s
}

/// The tables the query names (as whole words), with columns, keys and indexes.
pub fn structure(tables: &[TableSchema], query: &str, budget: usize, p: &Prompts) -> String {
    let q = query.to_lowercase();
    let named = |t: &TableSchema| {
        let n = t.name.to_lowercase();
        !n.is_empty()
            && q.match_indices(&n).any(|(i, _)| {
                let before = q[..i].chars().last().is_none_or(|c| !c.is_alphanumeric() && c != '_');
                let after = q[i + n.len()..].chars().next().is_none_or(|c| !c.is_alphanumeric() && c != '_');
                before && after
            })
    };
    let mut out = String::new();
    for t in tables.iter().filter(|t| named(t)) {
        let line = table_line(t, p);
        if out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
    }
    out
}

fn table_line(t: &TableSchema, p: &Prompts) -> String {
    let pk: Vec<&str> = t.primary_key.as_ref().map(|k| k.columns.iter().map(String::as_str).collect()).unwrap_or_default();
    let cols: Vec<String> = t
        .columns
        .iter()
        .map(|c| {
            let mut s = format!("{} {}", c.name, c.data_type);
            if pk.contains(&c.name.as_str()) {
                s.push_str(" PK");
            } else if !c.nullable {
                s.push_str(" NOT NULL");
            }
            s
        })
        .collect();
    let name = match t.schema.as_deref().filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{}", t.name),
        None => t.name.clone(),
    };
    let mut line = format!("{name}({})\n", cols.join(", "));
    for ix in &t.indexes {
        line.push_str(&format!(
            "  {} {}{} ({}){}\n",
            p.text("optimizer.index"),
            ix.name,
            if ix.unique { " UNIQUE" } else { "" },
            ix.columns.join(", "),
            if ix.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", ix.include.join(", ")) }
        ));
    }
    line
}

/// The text in `<name>…</name>`; `names` are the tag and its older Spanish
/// name (an answer to the Spanish-only prompt).
fn tag<'a>(text: &'a str, names: [&str; 2]) -> Option<&'a str> {
    names.iter().find_map(|name| {
        let open = format!("<{name}>");
        let close = format!("</{name}>");
        let a = text.find(&open)? + open.len();
        let b = text[a..].find(&close).map_or(text.len(), |b| a + b);
        Some(text[a..b].trim())
    })
}

/// The follow-up when some alternatives don't compile: each one with the
/// engine's error, to be fixed in the same format or left out.
pub fn repair_prompt(p: &Prompts, failed: &[(Candidate, String)]) -> String {
    let mut s = format!("{}\n", p.text("optimizer.repair"));
    for (c, error) in failed {
        s.push_str(&format!("\n<failed>\n<query>\n{}\n</query>\n<error>{}</error>\n</failed>\n", c.sql.trim(), error.trim()));
    }
    s
}

/// The model answered that it sees no improvement ("NONE"; "NINGUNA" is
/// what the Spanish-only prompt asked for).
pub fn says_none(answer: &str) -> bool {
    let a = answer.to_uppercase();
    a.contains("NONE") || a.contains("NINGUNA")
}

/// Code fences the model wrapped the query in.
fn unfence(s: &str) -> String {
    let t = s.trim();
    if let Some(rest) = t.strip_prefix("```") {
        let body = rest.split_once('\n').map_or("", |(_, b)| b);
        return body.trim_end().strip_suffix("```").unwrap_or(body).trim().to_string();
    }
    t.to_string()
}

fn normalized(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").trim_end_matches(';').trim().to_lowercase()
}

/// The alternatives in an answer, in the format the prompt asks for. What
/// doesn't follow it, repeats the original or another one, is dropped.
pub fn parse(answer: &str, original: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen = vec![normalized(original)];
    let open = if answer.contains("<alternative>") { "<alternative>" } else { "<alternativa>" };
    for chunk in answer.split(open).skip(1) {
        let end = ["</alternative>", "</alternativa>"].iter().filter_map(|c| chunk.find(c)).min().unwrap_or(chunk.len());
        let chunk = &chunk[..end];
        let Some(query) = tag(chunk, ["query", "consulta"]).map(unfence).filter(|q| !q.is_empty()) else { continue };
        let n = normalized(&query);
        if seen.contains(&n) {
            continue;
        }
        seen.push(n);
        out.push(Candidate {
            id: String::new(),
            source: Source::Ai,
            rule: None,
            params: Default::default(),
            title: tag(chunk, ["title", "titulo"]).map(str::to_string).filter(|t| !t.is_empty()),
            explanation: tag(chunk, ["explanation", "explicacion"]).map(str::to_string).filter(|t| !t.is_empty()),
            sql: query,
            verify: true,
        });
        if out.len() == MAX {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::optimizer::catalog::tests::table;
    use dbine_ai::prompts;

    #[test]
    fn answers_are_read_strictly() {
        let answer = "Sure:\n<alternative>\n<title>EXISTS</title>\n<explanation>Stops early.</explanation>\n<query>\n```sql\nSELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM u)\n```\n</query>\n</alternative>\n\
             <alternative><title>same</title><query>select *  from t;</query></alternative>\
             <alternative><title>no query</title></alternative>\
             <alternative><query>SELECT 2</query></alternative>\
             <alternative><query>SELECT 3</query></alternative>\
             <alternative><query>SELECT 4</query></alternative>";
        let c = parse(answer, "SELECT * FROM t");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].sql, "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM u)");
        assert_eq!(c[0].title.as_deref(), Some("EXISTS"));
        assert_eq!(c[0].explanation.as_deref(), Some("Stops early."));
        assert_eq!(c[1].sql, "SELECT 2");
        assert!(c.iter().all(|x| x.source == Source::Ai && x.verify));
        assert!(parse("NONE", "x").is_empty());
        assert!(says_none("NONE") && says_none("ninguna") && !says_none("<alternative>"));
        // The tags the Spanish-only prompt asked for still read.
        let old = parse("<alternativa><titulo>EXISTS</titulo><explicacion>Corta antes.</explicacion><consulta>SELECT 9</consulta></alternativa>", "x");
        assert_eq!((old[0].sql.as_str(), old[0].title.as_deref(), old[0].explanation.as_deref()), ("SELECT 9", Some("EXISTS"), Some("Corta antes.")));
    }

    #[test]
    fn structure_has_only_the_tables_named() {
        let ts = vec![
            table(Some("dbo"), "orders", &[("id", "int", false), ("note", "text", true)], &["id"], &[(&["note"], false)]),
            table(None, "order_items", &[("id", "int", false)], &[], &[]),
        ];
        let es = prompts::get("es");
        let s = structure(&ts, "SELECT * FROM dbo.Orders o", 10_000, es);
        assert_eq!(s, "dbo.orders(id int PK, note text)\n  índice ix0 (note)\n");
        assert_eq!(structure(&ts, "SELECT * FROM orders", 5, es), "");
        assert!(structure(&ts, "SELECT * FROM orders", 10_000, prompts::get("en")).contains("  index ix0 (note)"));
    }

    #[test]
    fn prompts_carry_query_structure_and_plan() {
        let u = user_prompt("SELECT 1", "t(a int)\n", "Seq Scan on t");
        assert!(u.contains("<original_query>\nSELECT 1\n</original_query>") && u.contains("<structure>") && u.contains("<plan>\nSeq Scan on t"));
        assert!(!user_prompt("SELECT 1", "", "").contains("<plan>"));
        let sys = system_prompt(prompts::get("es"), "PostgreSQL", "SQL");
        assert!(sys.contains("como máximo 3") && sys.contains("en español."));
        // The UI language's file: the rules and the answer language in it.
        let en = system_prompt(prompts::get("en-US"), "PostgreSQL", "SQL");
        assert!(en.contains("PostgreSQL") && en.contains("English") && !en.contains("Respondé"), "{en}");
        for l in prompts::LANGS {
            let s = system_prompt(prompts::get(l), "PostgreSQL", "SQL");
            assert!(["<alternative>", "<title>", "<explanation>", "<query>", "NONE", " 3 "].iter().all(|t| s.contains(t)), "{l}: {s}");
        }
        let c = parse("<alternative><query>SELECT b FROM t</query></alternative>", "SELECT a FROM t");
        let r = repair_prompt(prompts::get("en"), &[(c[0].clone(), "Invalid column name 'b'.".into())]);
        assert!(r.contains("<query>\nSELECT b FROM t\n</query>\n<error>Invalid column name 'b'.</error>") && r.contains("NONE"));
    }
}
