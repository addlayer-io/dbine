//! Splits a HANA script into statements. `;` ends a statement, except
//! inside an SQLScript body (`CREATE PROCEDURE | FUNCTION | TRIGGER …
//! BEGIN … END`, `DO BEGIN … END`), which runs to the `;` after its
//! outermost `END`. `END IF`, `END FOR`, `END WHILE` and `END LOOP` close
//! control blocks that don't open with `BEGIN`.

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    Single,
    Double,
    Line,
    Block,
}

pub fn split(sql: &str) -> Vec<String> {
    pieces(sql).into_iter().map(|(text, _)| text).collect()
}

/// The statements of a script, each with its byte offset in it.
pub fn pieces(sql: &str) -> Vec<(String, usize)> {
    let chars: Vec<char> = sql.chars().collect();
    let bytes: Vec<usize> = sql.char_indices().map(|(b, _)| b).chain(std::iter::once(sql.len())).collect();
    let mut seg = 0usize;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut state = State::Code;
    let mut word = String::new();
    let mut unit = Unit::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match state {
            State::Line => {
                cur.push(c);
                if c == '\n' {
                    state = State::Code;
                }
            }
            State::Block => {
                cur.push(c);
                if c == '*' && next == Some('/') {
                    cur.push('/');
                    i += 1;
                    state = State::Code;
                }
            }
            State::Single | State::Double => {
                cur.push(c);
                let q = if state == State::Single { '\'' } else { '"' };
                if c == q {
                    if next == Some(q) {
                        cur.push(q);
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::Code => {
                if c.is_alphanumeric() || c == '_' || c == '$' || c == '#' {
                    word.push(c);
                } else if !word.is_empty() {
                    unit.word(&std::mem::take(&mut word));
                }
                if c == ';' && unit.may_end() {
                    push(&mut out, &std::mem::take(&mut cur), bytes[seg]);
                    unit = Unit::default();
                    i += 1;
                    seg = i;
                    continue;
                }
                match c {
                    '-' if next == Some('-') => state = State::Line,
                    '/' if next == Some('*') => {
                        cur.push_str("/*");
                        i += 2;
                        state = State::Block;
                        continue;
                    }
                    '\'' => state = State::Single,
                    '"' => state = State::Double,
                    _ => {}
                }
                cur.push(c);
            }
        }
        i += 1;
    }
    push(&mut out, &cur, bytes[seg.min(chars.len())]);
    out
}

/// `stmt` (found at byte `at`) without its leading comments and spaces.
fn push(out: &mut Vec<(String, usize)>, stmt: &str, at: usize) {
    let lead = strip_leading_comments(stmt);
    let text = lead.trim_end();
    if !text.is_empty() {
        out.push((text.to_string(), at + stmt.len() - lead.len()));
    }
}

/// The text without leading whitespace and comments.
pub fn strip_leading_comments(s: &str) -> &str {
    let mut s = s.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.find('\n').map_or("", |i| &rest[i + 1..]).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map_or("", |i| &rest[i + 2..]).trim_start();
        } else {
            return s;
        }
    }
}

#[derive(Default)]
struct Unit {
    head: Vec<String>,
    /// An SQLScript unit: its `;`s end statements inside the body.
    script: bool,
    seen_begin: bool,
    depth: i32,
    /// Saw `END`; the next word says whether it closes a BEGIN/CASE.
    pending_end: bool,
}

impl Unit {
    fn word(&mut self, w: &str) {
        let w = w.to_ascii_uppercase();
        if self.head.len() < 5 {
            self.head.push(w.clone());
            self.script = self.script || is_script_head(&self.head);
        }
        if !self.script {
            return;
        }
        if self.pending_end {
            self.pending_end = false;
            if !matches!(w.as_str(), "IF" | "FOR" | "WHILE" | "LOOP") {
                self.depth -= 1;
            } else {
                return;
            }
        }
        match w.as_str() {
            "BEGIN" => {
                self.seen_begin = true;
                self.depth += 1;
            }
            "CASE" if self.seen_begin => self.depth += 1,
            "END" if self.seen_begin => self.pending_end = true,
            _ => {}
        }
    }

    fn may_end(&mut self) -> bool {
        if self.pending_end {
            self.pending_end = false;
            self.depth -= 1;
        }
        !self.script || !self.seen_begin || self.depth <= 0
    }
}

fn is_script_head(head: &[String]) -> bool {
    let h: Vec<&str> = head.iter().map(String::as_str).collect();
    let object = |w: &str| matches!(w, "PROCEDURE" | "FUNCTION" | "TRIGGER" | "LIBRARY");
    match h.as_slice() {
        ["DO", ..] => true,
        ["CREATE", "OR", "REPLACE", o, ..] | ["CREATE", o, ..] | ["ALTER", o, ..] => object(o),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_statements() {
        assert_eq!(split("select 1 from dummy; select ';' from dummy;"), vec![
            "select 1 from dummy",
            "select ';' from dummy"
        ]);
    }

    #[test]
    fn procedures_keep_their_body() {
        let sql = "CREATE PROCEDURE p (IN a INT, OUT b INT) LANGUAGE SQLSCRIPT AS\nBEGIN\n  \
                   IF :a > 0 THEN b = 1; ELSE b = CASE WHEN :a = 0 THEN 0 ELSE -1 END; END IF;\n  \
                   FOR i IN 1..3 DO b = :b + 1; END FOR;\n  BEGIN b = :b; END;\nEND;\n\
                   select 1 from dummy;\nDO BEGIN SELECT 1 FROM dummy; END;\nALTER PROCEDURE p RECOMPILE;";
        let s = split(sql);
        assert_eq!(s.len(), 4, "{s:#?}");
        assert!(s[0].ends_with("BEGIN b = :b; END;\nEND"));
        assert_eq!(s[1], "select 1 from dummy");
        assert_eq!(s[2], "DO BEGIN SELECT 1 FROM dummy; END");
        assert_eq!(s[3], "ALTER PROCEDURE p RECOMPILE");
    }

    #[test]
    fn pieces_know_where_they_are() {
        let sql = "-- x\nselect 'ñ;' from dummy;\n  DO BEGIN SELECT 1 FROM dummy; END;\nselect 2 from dummy";
        let p = pieces(sql);
        assert_eq!(p.len(), 3, "{p:#?}");
        for (t, at) in &p {
            assert_eq!(&sql[*at..*at + t.len()], t);
        }
    }

    #[test]
    fn triggers() {
        let s = split("create trigger t after insert on x for each row begin insert into y values (1); end; select 1 from dummy");
        assert_eq!(s.len(), 2, "{s:#?}");
    }
}
