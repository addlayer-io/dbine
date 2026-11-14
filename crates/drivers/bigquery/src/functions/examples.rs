//! A guard for the static catalogs scraped from documentation (BigQuery's
//! and Spanner's, both GoogleSQL): a line whose arguments are values is one
//! of the page's example calls, not a syntax form. `RANGE_BUCKET(20, [0,
//! 10])`, `FORMAT('%s', 1)`, `CAST(x=1 AS STRING)`, `SEARCH(tl, 'big')`,
//! `OBJ.MAKE_REF(JSON '{…}')` and `COUNT(DISTINCT IF(c, e, NULL))` are
//! examples; `ARRAY_AGG([ DISTINCT ] expression …)`, `JSON_ARRAY([value][,
//! ...])` and `PARSE_JSON(json_expr [, mode => { 'exact' | 'round' } ])`
//! are syntax.

/// Whether `args` (with its parentheses) passes a value where a syntax form
/// names a parameter, at the top level or in a nested call.
pub(crate) fn is_example(args: &str) -> bool {
    let Some(inner) = args
        .trim()
        .strip_prefix('(')
        .and_then(|a| a.strip_suffix(')'))
    else {
        return false;
    };
    split_top(inner)
        .into_iter()
        .any(|arg| literal(arg) || nested_example(arg))
}

/// The arguments of `s`: split at the commas outside brackets and quotes.
fn split_top(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut quote, mut start) = (0i32, None, 0);
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"') => quote = Some(c),
            (None, '(' | '[' | '{') => depth += 1,
            (None, ')' | ']' | '}') => depth -= 1,
            (None, ',') if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(s[start..].trim());
    out
}

/// A number, a string, `NULL`, an array of values, a typed literal (`JSON
/// '…'`) or an assignment (`x=1`, not `name => value`).
fn literal(arg: &str) -> bool {
    let value = |s: &str| {
        let s = s.trim_start();
        let null = s.get(..4).is_some_and(|w| w.eq_ignore_ascii_case("NULL"))
            && !s[4..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_');
        null || s.starts_with(|c: char| c.is_ascii_digit() || c == '\'' || c == '"')
            || s.strip_prefix('-')
                .is_some_and(|r| r.starts_with(|c: char| c.is_ascii_digit()))
    };
    if value(arg) {
        return true;
    }
    if let Some(rest) = arg.strip_prefix('[') {
        let rest = rest.trim_start();
        return rest.starts_with(']') || value(rest);
    }
    if let Some((word, rest)) = arg.split_once(' ') {
        let typed = !word.is_empty() && word.chars().all(|c| c.is_ascii_uppercase());
        if typed && rest.trim_start().starts_with(['\'', '"']) {
            return true;
        }
    }
    let first = arg.split_whitespace().next().unwrap_or("");
    first
        .find('=')
        .is_some_and(|i| i > 0 && !first[i + 1..].starts_with('>'))
}

/// Whether a call inside `arg` (`IF(…)` in `DISTINCT IF(…)`) is an example.
fn nested_example(arg: &str) -> bool {
    let bytes = arg.as_bytes();
    for (i, &b) in bytes.iter().enumerate() {
        if b != b'(' || i == 0 || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            continue;
        }
        let mut depth = 0;
        for (j, &c) in bytes[i..].iter().enumerate() {
            match c {
                b'(' => depth += 1,
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        if is_example(&arg[i..=i + j]) {
                            return true;
                        }
                        break;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

#[test]
fn examples_are_told_from_syntax() {
    for example in [
        "(x=1 AS STRING)",
        "('%s', 1)",
        "('%')",
        "('00-%t-00', NULL_expression)",
        "(20, [0, 10, 20, 30, 40])",
        "(-10, [5, 10, 20, 30, 40])",
        "(80, [])",
        "(NULL, [0, 10, 20, 30, 40])",
        "(1.2, [1, 2, 4, 6])",
        "(tl, 'big')",
        "(JSON '{\"uri\": \"gs://b/x.png\"}')",
        "(DISTINCT IF(condition, expression, NULL))",
    ] {
        assert!(is_example(example), "{example}");
    }
    for syntax in [
        "(expression AS typename [format_clause])",
        "([ DISTINCT ] expression [ WHERE where_expression ])",
        "([value][, ...])",
        "([ [ year => ] value ] [, [ month => ] value ])",
        "(json_string_expr [, wide_number_mode => { 'exact' | 'round' } ])",
        "(vector1, vector2, options=>value)",
        "(TABLE table, start_timestamp DEFAULT NULL, end_timestamp)",
        "(string_or_bytes_value [, [ level => ] 3])",
        "(X1,...,XN)",
        "(value_to_tokenize, [, comparison_type => { \"all\" | \"equality\" } ])",
        "(NULL_expression)",
        "((time_series_subquery), time_series_column, bucket_width)",
        "()",
        "(*)",
    ] {
        assert!(!is_example(syntax), "{syntax}");
    }
}
