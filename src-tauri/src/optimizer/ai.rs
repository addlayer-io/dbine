//! "Pedir alternativas a la IA": what goes to the configured AI (the query,
//! the structure of the tables it mentions, a summary of the plan; never
//! rows) and how its answer is read. The answer is a proposal: each
//! alternative becomes a candidate marked "IA" that the user compares like
//! any other, and nothing runs unless the user clicks.

use super::{Candidate, Source};
use dbine_driver::TableSchema;

/// At most this many alternatives are taken from an answer.
pub const MAX: usize = 3;

/// The UI language's name, for "write the title and explanation in …".
pub fn answer_language(code: &str) -> &'static str {
    match code.split(['-', '_']).next().unwrap_or("") {
        "en" => "inglés",
        "pt" => "portugués",
        "fr" => "francés",
        "it" => "italiano",
        _ => "español",
    }
}

pub fn system_prompt(engine: &str, language: &str, answer: &str) -> String {
    format!(
        "Sos un experto en rendimiento de {engine}. Te paso una consulta ({language}), la estructura de las tablas que usa y un resumen de su plan de ejecución.\n\
         Proponé como máximo {MAX} reescrituras EQUIVALENTES que puedan ser más rápidas en {engine}:\n\
         - Equivalente quiere decir que devuelve exactamente las mismas filas y columnas, en el mismo orden de columnas, y en el mismo orden de filas si la original tiene ORDER BY. \
           Cuidá los NULL, los duplicados y los tipos.\n\
         - No agregues ni quites filtros, no cambies límites de filas, no uses tablas o columnas que no estén en la estructura.\n\
         - No propongas crear índices ni cambiar la estructura: solo reescrituras de la consulta.\n\
         - Si no ves ninguna mejora real, respondé solo NINGUNA.\n\
         Respondé SOLO con este formato, sin texto antes ni después:\n\
         <alternativa>\n<titulo>qué cambia, en pocas palabras</titulo>\n<explicacion>por qué puede ser más rápida, en una o dos oraciones</explicacion>\n<consulta>\nla consulta completa\n</consulta>\n</alternativa>\n\
         Escribí el título y la explicación en {answer}."
    )
}

pub fn user_prompt(query: &str, structure: &str, plan: &str) -> String {
    let mut s = format!("<consulta_original>\n{}\n</consulta_original>\n", query.trim());
    if !structure.trim().is_empty() {
        s.push_str(&format!("\n<estructura>\n{structure}</estructura>\n"));
    }
    if !plan.trim().is_empty() {
        s.push_str(&format!("\n<plan>\n{plan}\n</plan>\n"));
    }
    s
}

/// The tables the query names (as whole words), with columns, keys and indexes.
pub fn structure(tables: &[TableSchema], query: &str, budget: usize) -> String {
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
        let line = table_line(t);
        if out.len() + line.len() > budget {
            break;
        }
        out.push_str(&line);
    }
    out
}

fn table_line(t: &TableSchema) -> String {
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
            "  índice {}{} ({}){}\n",
            ix.name,
            if ix.unique { " UNIQUE" } else { "" },
            ix.columns.join(", "),
            if ix.include.is_empty() { String::new() } else { format!(" INCLUDE ({})", ix.include.join(", ")) }
        ));
    }
    line
}

fn tag<'a>(text: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let a = text.find(&open)? + open.len();
    let b = text[a..].find(&close).map_or(text.len(), |b| a + b);
    Some(text[a..b].trim())
}

/// The follow-up when some alternatives don't compile: each one with the
/// engine's error, to be fixed in the same format or left out.
pub fn repair_prompt(failed: &[(Candidate, String)]) -> String {
    let mut s = String::from(
        "Estas alternativas fallan al compilarlas en la base (sin ejecutarlas). Corregilas para que sean válidas y sigan siendo EQUIVALENTES a la original; \
         una que no puedas corregir, omitila. Respondé SOLO con el mismo formato, solo con las corregidas, o NINGUNA.\n",
    );
    for (c, error) in failed {
        s.push_str(&format!("\n<fallida>\n<consulta>\n{}\n</consulta>\n<error>{}</error>\n</fallida>\n", c.sql.trim(), error.trim()));
    }
    s
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
    for chunk in answer.split("<alternativa>").skip(1) {
        let chunk = chunk.split("</alternativa>").next().unwrap_or(chunk);
        let Some(query) = tag(chunk, "consulta").map(unfence).filter(|q| !q.is_empty()) else { continue };
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
            title: tag(chunk, "titulo").map(str::to_string).filter(|t| !t.is_empty()),
            explanation: tag(chunk, "explicacion").map(str::to_string).filter(|t| !t.is_empty()),
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

    #[test]
    fn answers_are_read_strictly() {
        let answer = "Claro:\n<alternativa>\n<titulo>EXISTS</titulo>\n<explicacion>Corta antes.</explicacion>\n<consulta>\n```sql\nSELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM u)\n```\n</consulta>\n</alternativa>\n\
             <alternativa><titulo>igual</titulo><consulta>select *  from t;</consulta></alternativa>\
             <alternativa><titulo>sin consulta</titulo></alternativa>\
             <alternativa><consulta>SELECT 2</consulta></alternativa>\
             <alternativa><consulta>SELECT 3</consulta></alternativa>\
             <alternativa><consulta>SELECT 4</consulta></alternativa>";
        let c = parse(answer, "SELECT * FROM t");
        assert_eq!(c.len(), 3);
        assert_eq!(c[0].sql, "SELECT 1 FROM t WHERE EXISTS (SELECT 1 FROM u)");
        assert_eq!(c[0].title.as_deref(), Some("EXISTS"));
        assert_eq!(c[0].explanation.as_deref(), Some("Corta antes."));
        assert_eq!(c[1].sql, "SELECT 2");
        assert!(c.iter().all(|x| x.source == Source::Ai && x.verify));
        assert!(parse("NINGUNA", "x").is_empty());
    }

    #[test]
    fn structure_has_only_the_tables_named() {
        let ts = vec![
            table(Some("dbo"), "orders", &[("id", "int", false), ("note", "text", true)], &["id"], &[(&["note"], false)]),
            table(None, "order_items", &[("id", "int", false)], &[], &[]),
        ];
        let s = structure(&ts, "SELECT * FROM dbo.Orders o", 10_000);
        assert_eq!(s, "dbo.orders(id int PK, note text)\n  índice ix0 (note)\n");
        assert_eq!(structure(&ts, "SELECT * FROM orders", 5), "");
    }

    #[test]
    fn prompts_carry_query_structure_and_plan() {
        let u = user_prompt("SELECT 1", "t(a int)\n", "Seq Scan on t");
        assert!(u.contains("<consulta_original>\nSELECT 1\n</consulta_original>") && u.contains("<estructura>") && u.contains("<plan>\nSeq Scan on t"));
        assert!(!user_prompt("SELECT 1", "", "").contains("<plan>"));
        let sys = system_prompt("PostgreSQL", "SQL", answer_language("en-US"));
        assert!(sys.contains("como máximo 3") && sys.contains("en inglés."));
        assert_eq!(answer_language("xx"), "español");
        let c = parse("<alternativa><consulta>SELECT b FROM t</consulta></alternativa>", "SELECT a FROM t");
        let r = repair_prompt(&[(c[0].clone(), "Invalid column name 'b'.".into())]);
        assert!(r.contains("<consulta>\nSELECT b FROM t\n</consulta>\n<error>Invalid column name 'b'.</error>"));
    }
}
