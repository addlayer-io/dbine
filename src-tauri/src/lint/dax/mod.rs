//! DAX rules (Power BI semantic models). Text only, like every family: the
//! model isn't read, so the two rules about names (`unqualified-column`,
//! `qualified-measure`) only know what the script itself shows. The
//! optimizer's DAX rewrites (`crate::optimizer::dax`) come from these same
//! matches, so the editor and "Optimizar consulta" agree on what they see.
//!
//! The rules are the well-known DAX best practices; each one cites its
//! source. "BPA" is the Best Practice Analyzer rule set Microsoft publishes
//! for tabular models:
//! https://github.com/microsoft/Analysis-Services/tree/master/BestPracticeRules

pub(crate) mod lex;
#[cfg(test)]
mod tests;

use super::Finding;
use lex::{Dax, K};
use std::collections::{HashMap, HashSet};
use std::ops::Range;

/// A rule's match: where the editor marks it, the values its message shows
/// and, when one can be offered, the optimizer's rewrite.
pub(crate) struct Hit {
    pub rule: &'static str,
    pub start: usize,
    pub end: usize,
    pub params: Vec<(&'static str, String)>,
    pub fix: Option<Fix>,
}

/// A rewrite: `start..end` of the script becomes `text`.
pub(crate) struct Fix {
    /// The optimizer's rule (`optimizer:rules.<rule>` in the UI).
    pub rule: &'static str,
    pub start: usize,
    pub end: usize,
    pub text: String,
    /// Not equivalent in every case: the UI asks to compare it first.
    pub verify: bool,
    pub params: Vec<(&'static str, String)>,
}

fn hit(rule: &'static str, start: usize, end: usize) -> Hit {
    Hit {
        rule,
        start,
        end,
        params: Vec::new(),
        fix: None,
    }
}

impl Hit {
    fn param(mut self, key: &'static str, value: impl Into<String>) -> Self {
        self.params.push((key, value.into()));
        self
    }

    fn fix(mut self, fix: Option<Fix>) -> Self {
        self.fix = fix;
        self
    }
}

pub(super) fn lint(script: &str, out: &mut Vec<Finding>) {
    for h in hits(script) {
        let mut f = Finding::new(h.rule, h.start, h.end);
        f.params = h.params;
        out.push(f);
    }
}

/// Every match in `script`, the optimizer's rewrites with them.
pub(crate) fn hits(script: &str) -> Vec<Hit> {
    let d = Dax::new(script);
    let names = Names::of(&d);
    let cx = Cx {
        d: &d,
        names: &names,
    };
    let mut out = Vec::new();
    for i in 0..d.t.len() {
        match d.t[i].k {
            K::Word => {
                if let Some(open) = d.call(i) {
                    cx.function(i, open, &mut out);
                }
            }
            K::Punct if d.t[i].text == "/" => cx.divide(i, &mut out),
            K::Ref => cx.reference(i, &mut out),
            _ => {}
        }
    }
    cx.topn_without_order_by(&mut out);
    out
}

/// Functions whose arguments, in the positions [`column_args`] gives, take
/// only columns: a `'T'[c]` there is a column, never a measure.
fn column_args(f: &str, args: &[Range<usize>], d: &Dax) -> Range<usize> {
    let n = args.len();
    let before_names = |from: usize| {
        let to = (from..n)
            .find(|&k| args[k].len() == 1 && d.t[args[k].start].k == K::Str)
            .unwrap_or(n);
        from.min(to)..to
    };
    match f {
        "VALUES" | "DISTINCT" | "ALL" | "ALLNOBLANKROW" | "REMOVEFILTERS" | "ALLSELECTED"
        | "HASONEVALUE" | "HASONEFILTER" | "ISFILTERED" | "ISCROSSFILTERED" | "ISINSCOPE"
        | "USERELATIONSHIP" | "FILTERS" => 0..n,
        "ALLEXCEPT" | "TREATAS" => 1.min(n)..n,
        "CROSSFILTER" => 0..2.min(n),
        "MIN" | "MAX" if n == 1 => 0..1,
        "SELECTEDVALUE"
        | "RELATED"
        | "SUM"
        | "AVERAGE"
        | "COUNT"
        | "COUNTA"
        | "COUNTBLANK"
        | "DISTINCTCOUNT"
        | "DISTINCTCOUNTNOBLANK"
        | "MEDIAN"
        | "FIRSTNONBLANK"
        | "LASTNONBLANK"
        | "FIRSTDATE"
        | "LASTDATE"
        | "DATESYTD"
        | "DATESQTD"
        | "DATESMTD"
        | "SAMEPERIODLASTYEAR"
        | "DATEADD"
        | "DATESBETWEEN"
        | "DATESINPERIOD"
        | "PARALLELPERIOD"
        | "PREVIOUSDAY"
        | "PREVIOUSMONTH"
        | "PREVIOUSQUARTER"
        | "PREVIOUSYEAR"
        | "NEXTDAY"
        | "NEXTMONTH"
        | "NEXTQUARTER"
        | "NEXTYEAR"
        | "STARTOFMONTH"
        | "STARTOFQUARTER"
        | "STARTOFYEAR"
        | "ENDOFMONTH"
        | "ENDOFQUARTER"
        | "ENDOFYEAR"
        | "LOOKUPVALUE" => 0..1.min(n),
        // The group-by columns, before the first "Name", expression pair.
        "SUMMARIZECOLUMNS" => before_names(0),
        "SUMMARIZE" | "GROUPBY" => before_names(1),
        _ => 0..0,
    }
}

/// Scalar functions a boolean filter can call without reading the filter
/// context (CALCULATE's rules for boolean filter arguments:
/// https://learn.microsoft.com/dax/calculate-function-dax#remarks).
const SCALAR: &[&str] = &[
    "TRUE",
    "FALSE",
    "BLANK",
    "NOT",
    "AND",
    "OR",
    "ISBLANK",
    "DATE",
    "TIME",
    "DATEVALUE",
    "TIMEVALUE",
    "TODAY",
    "NOW",
    "UTCTODAY",
    "UTCNOW",
    "YEAR",
    "MONTH",
    "DAY",
    "HOUR",
    "MINUTE",
    "SECOND",
    "WEEKDAY",
    "WEEKNUM",
    "LEFT",
    "RIGHT",
    "MID",
    "LEN",
    "UPPER",
    "LOWER",
    "TRIM",
    "VALUE",
    "FORMAT",
    "ABS",
    "ROUND",
    "ROUNDUP",
    "ROUNDDOWN",
    "INT",
    "TRUNC",
    "CONTAINSSTRING",
    "CONTAINSSTRINGEXACT",
    "SEARCH",
    "FIND",
    "EXACT",
    "ISNUMBER",
    "ISTEXT",
    "IF",
    "SWITCH",
    "COALESCE",
    "CONCATENATE",
];

/// What the script itself says about its names.
#[derive(Default)]
struct Names {
    /// `VAR` names and the parameters of user-defined functions: they
    /// aren't tables.
    vars: HashSet<String>,
    /// Measures the script defines (`DEFINE MEASURE 'T'[M] = …`): name →
    /// their home tables.
    measures: HashMap<String, HashSet<String>>,
    /// Columns the script shows with their table: name → the tables (each
    /// as its name and as written).
    columns: HashMap<String, Vec<(String, String)>>,
    /// Names the script gives to new columns (`"Total"` in ADDCOLUMNS,
    /// SUMMARIZECOLUMNS, SELECTCOLUMNS, ROW…): they're referenced without a
    /// table.
    new_columns: HashSet<String>,
}

impl Names {
    fn of(d: &Dax) -> Self {
        let mut n = Names::default();
        let t = &d.t;
        for i in 0..t.len() {
            let x = &t[i];
            if x.is("VAR") {
                if let Some(v) = t.get(i + 1).filter(|v| v.k == K::Word) {
                    n.vars.insert(v.name());
                }
            }
            // `FUNCTION f = (a, t : TABLE) => …`
            if x.is("FUNCTION") && d.p(i + 2, "=") && d.p(i + 3, "(") {
                let params = t[i + 3..d.closing(i + 3)].windows(2);
                for w in params.filter(|w| w[1].k == K::Word && (w[0].p("(") || w[0].p(","))) {
                    n.vars.insert(w[1].name());
                }
            }
            if x.is("MEASURE") && d.table_of(i + 2) == Some(i + 1) {
                n.measures
                    .entry(t[i + 2].name())
                    .or_default()
                    .insert(t[i + 1].name());
            }
            if x.is("COLUMN") && d.table_of(i + 2) == Some(i + 1) {
                n.column(d, i + 2);
            }
            if x.k == K::Str && i > 0 && (d.p(i - 1, "(") || d.p(i - 1, ",")) && d.p(i + 1, ",") {
                n.new_columns.insert(x.name());
            }
            if let Some(open) = d.call(i) {
                let args = d.args(open);
                let f = d.fname(i);
                let mut positions: Vec<usize> = column_args(&f, &args, d).collect();
                if f == "LOOKUPVALUE" {
                    // The result column, then search column / value pairs.
                    positions.extend((1..args.len()).step_by(2));
                }
                for k in positions {
                    if let Some(r) = d.reference(&args[k]).filter(|&r| d.table_of(r).is_some()) {
                        n.column(d, r);
                    }
                }
            }
        }
        n
    }

    /// The qualified reference at `r` is a column.
    fn column(&mut self, d: &Dax, r: usize) {
        let Some(ti) = d.table_of(r) else { return };
        let table = (d.t[ti].name(), d.t[ti].text.to_string());
        let tables = self.columns.entry(d.t[r].name()).or_default();
        if !tables.iter().any(|(name, _)| *name == table.0) {
            tables.push(table);
        }
    }

    /// `table[name]` (both as [`lex::Tok::name`] gives them) is a measure
    /// the script defines in that table, and the script doesn't show it as
    /// a column of that table. Another table's column with the same name is
    /// a column (`DEFINE MEASURE '_Measures'[Sales] = SUM('Sales'[Sales])`).
    fn measure(&self, table: &str, name: &str) -> bool {
        self.measures
            .get(name)
            .is_some_and(|tables| tables.contains(table))
            && !self
                .columns
                .get(name)
                .is_some_and(|tables| tables.iter().any(|(t, _)| t == table))
    }
}

struct Cx<'a, 'b> {
    d: &'b Dax<'a>,
    names: &'b Names,
}

impl Cx<'_, '_> {
    fn function(&self, i: usize, open: usize, out: &mut Vec<Hit>) {
        let d = self.d;
        let x = &d.t[i];
        let f = d.fname(i);
        let args = d.args(open);
        match f.as_str() {
            "CALCULATE" | "CALCULATETABLE" => {
                self.filter_whole_table(&args, out);
                self.redundant_calculate(i, open, &f, &args, out);
            }
            "IFERROR" => self.iferror(i, open, &args, out),
            "ISERROR" => self.iserror(i, out),
            "IF" => self.hasonevalue_values(i, open, &args, out),
            "SUMMARIZE" => self.summarize(i, open, &args, out),
            // EARLIER reads an outer row context; a variable says the same
            // and reads top to bottom. SQLBI, "Variables in DAX":
            // https://www.sqlbi.com/articles/variables-in-dax/
            "EARLIER" | "EARLIEST" => out.push(hit("earlier", x.start, x.end).param("fn", x.text)),
            _ => {}
        }
    }

    /// `'T'` or `T` that names a table (not a variable).
    fn is_table(&self, i: usize) -> bool {
        let x = &self.d.t[i];
        match x.k {
            K::Table => true,
            K::Word => !self.names.vars.contains(&x.name()) && self.d.call(i).is_none(),
            _ => false,
        }
    }

    /// The end (exclusive, in bytes) of the call whose `(` is at `open`,
    /// when it is closed.
    fn call_end(&self, open: usize) -> Option<usize> {
        self.d.t.get(self.d.closing(open)).map(|c| c.end)
    }

    /// `FILTER('T', …)` as a filter argument of CALCULATE / CALCULATETABLE:
    /// it iterates the whole table (expanded with its related tables) and
    /// filters every column of it. Microsoft Learn, "Avoid using FILTER as
    /// a filter argument":
    /// https://learn.microsoft.com/dax/best-practices/dax-avoid-avoid-filter-as-filter-argument
    /// (BPA: "Filter column values with proper syntax", "Filter measure
    /// values by columns, not tables").
    ///
    /// The rewrite: the condition alone under KEEPFILTERS, when it is a
    /// valid boolean filter (columns of that table, constants, scalar
    /// functions). It filters only those columns, not the expanded table,
    /// and keeps the filters already there (inside an iterator, the row's
    /// own), so the result can change: compare it first.
    fn filter_whole_table(&self, args: &[Range<usize>], out: &mut Vec<Hit>) {
        let d = self.d;
        for a in args.iter().skip(1) {
            let (r, kept) = match d.whole_call(a) {
                Some(k) if d.fname(k) == "KEEPFILTERS" => match d.args(k + 1).as_slice() {
                    [inner] => (inner.clone(), true),
                    _ => continue,
                },
                _ => (a.clone(), false),
            };
            let Some(fi) = d.whole_call(&r).filter(|&k| d.fname(k) == "FILTER") else {
                continue;
            };
            let fargs = d.args(fi + 1);
            if fargs.len() != 2 || fargs[0].len() != 1 || !self.is_table(fargs[0].start) {
                continue;
            }
            let table = &d.t[fargs[0].start];
            let fix = self.boolean(&fargs[1], Some(&table.name())).then(|| {
                let pred = d.text(&fargs[1]);
                Fix {
                    rule: "dax_filter_to_keepfilters",
                    start: d.t[r.start].start,
                    end: d.t[r.end - 1].end,
                    text: if kept {
                        pred.to_string()
                    } else {
                        format!("KEEPFILTERS({pred})")
                    },
                    verify: true,
                    params: vec![("table", table.text.to_string())],
                }
            });
            out.push(
                hit("filter-whole-table", d.t[fi].start, table.end)
                    .param("table", table.text)
                    .fix(fix),
            );
        }
    }

    /// A boolean filter that reads no filter context: columns of one table
    /// (`table`, when given), constants, variables and [`SCALAR`] functions.
    fn boolean(&self, r: &Range<usize>, table: Option<&str>) -> bool {
        let d = self.d;
        let mut tables = HashSet::new();
        for j in r.clone() {
            let x = &d.t[j];
            let qualifier = j + 1 < r.end && d.table_of(j + 1) == Some(j);
            match x.k {
                K::Ref => match d.table_of(j) {
                    // A boolean filter can't read a measure.
                    Some(ti) if self.names.measure(&d.t[ti].name(), &x.name()) => return false,
                    Some(ti) => {
                        tables.insert(d.t[ti].name());
                    }
                    // `[x]`: a measure, or a column of a row context.
                    None => return false,
                },
                K::Table if !qualifier => return false,
                K::Word if qualifier => {}
                K::Word if d.call(j).is_some() => {
                    if !SCALAR.contains(&d.fname(j).as_str()) {
                        return false;
                    }
                }
                K::Word => {
                    let keyword = ["TRUE", "FALSE", "IN", "NOT", "AND", "OR"]
                        .iter()
                        .any(|w| x.is(w));
                    if !keyword && !self.names.vars.contains(&x.name()) {
                        return false;
                    }
                }
                _ => {}
            }
        }
        tables.len() == 1 && table.is_none_or(|t| tables.contains(t))
    }

    /// A filter argument whose value doesn't depend on the context it is
    /// evaluated in: a boolean filter, ALL / REMOVEFILTERS / ALLEXCEPT /
    /// ALLNOBLANKROW / USERELATIONSHIP / CROSSFILTER over names, TREATAS of
    /// constants, KEEPFILTERS of one of them.
    fn static_filter(&self, r: &Range<usize>) -> bool {
        let d = self.d;
        if let Some(k) = d.whole_call(r) {
            let args = d.args(k + 1);
            let names = |rs: &[Range<usize>]| {
                rs.iter()
                    .all(|a| d.reference(a).is_some() || (a.len() == 1 && self.is_table(a.start)))
            };
            match d.fname(k).as_str() {
                "KEEPFILTERS" => return args.len() == 1 && self.static_filter(&args[0]),
                "ALL" | "REMOVEFILTERS" | "ALLNOBLANKROW" | "ALLEXCEPT" | "USERELATIONSHIP" => {
                    return names(&args)
                }
                "CROSSFILTER" => {
                    return args.len() == 3
                        && names(&args[..2])
                        && args[2].len() == 1
                        && d.t[args[2].start].k == K::Word
                }
                "TREATAS" => {
                    return args.len() >= 2
                        && self.constant_table(&args[0])
                        && args[1..].iter().all(|a| d.reference(a).is_some())
                }
                _ => {}
            }
        }
        self.boolean(r, None)
    }

    /// `{ "a", 1, (2, DATE(2024, 1, 1)) }`: constants only.
    fn constant_table(&self, r: &Range<usize>) -> bool {
        let d = self.d;
        !r.is_empty()
            && d.p(r.start, "{")
            && d.closing(r.start) == r.end - 1
            && r.clone().all(|j| {
                let x = &d.t[j];
                match x.k {
                    K::Str | K::Num | K::Punct => true,
                    K::Word => ["TRUE", "FALSE", "BLANK", "DATE", "TIME"]
                        .iter()
                        .any(|w| x.is(w)),
                    _ => false,
                }
            })
    }

    /// CALCULATE without filter arguments around a measure reference (the
    /// measure already has an implicit CALCULATE), or around another
    /// CALCULATE whose filters don't depend on the context (the inner one
    /// already does the context transition). SQLBI, "Understanding context
    /// transition":
    /// https://www.sqlbi.com/articles/understanding-context-transition-in-dax/
    ///
    /// The rewrite keeps the inner expression: equivalent, the inner
    /// filters being evaluated the same before and after the transition.
    fn redundant_calculate(
        &self,
        i: usize,
        open: usize,
        f: &str,
        args: &[Range<usize>],
        out: &mut Vec<Hit>,
    ) {
        let d = self.d;
        let [a] = args else { return };
        let redundant = if f == "CALCULATE" && d.reference(a).is_some() {
            true
        } else if let Some(k) = d.whole_call(a).filter(|&k| d.fname(k) == f) {
            let inner = d.args(k + 1);
            !inner.is_empty() && inner[1..].iter().all(|r| self.static_filter(r))
        } else {
            false
        };
        let Some(end) = self.call_end(open).filter(|_| redundant) else {
            return;
        };
        let x = &d.t[i];
        let fix = Fix {
            rule: "dax_redundant_calculate",
            start: x.start,
            end,
            text: d.text(a).to_string(),
            verify: false,
            params: vec![("fn", x.text.to_string())],
        };
        out.push(
            hit("redundant-calculate", x.start, x.end)
                .param("fn", x.text)
                .fix(Some(fix)),
        );
    }

    /// `a / b` as the whole expression `r` (the last `*` or `/` at its top
    /// level is that `/`): the numerator and the denominator.
    fn division(&self, r: &Range<usize>) -> Option<(Range<usize>, Range<usize>)> {
        let d = self.d;
        let base = d.t.get(r.start)?.depth;
        let mut slash = None;
        for j in r.clone() {
            let x = &d.t[j];
            if x.depth != base {
                continue;
            }
            if x.is("IN") || x.is("NOT") {
                return None;
            }
            if x.k != K::Punct {
                continue;
            }
            match x.text {
                "/" => slash = Some(j),
                // `a / b * c` is `(a / b) * c`.
                "*" if slash.is_some() => return None,
                "*" | "^" | "(" | ")" | "{" | "}" => {}
                "-" | "+" => {
                    // A sign (at the start or after an operator) binds
                    // tighter than `/`; a binary + or - binds looser.
                    let sign = j == r.start
                        || (d.t[j - 1].k == K::Punct && !matches!(d.t[j - 1].text, ")" | "}"));
                    if !sign {
                        return None;
                    }
                }
                _ => return None,
            }
        }
        let s = slash?;
        (s > r.start && s + 1 < r.end).then(|| (r.start..s, s + 1..r.end))
    }

    fn divide_call(&self, num: &Range<usize>, den: &Range<usize>, alt: &Range<usize>) -> String {
        let d = self.d;
        let blank = d
            .whole_call(alt)
            .is_some_and(|k| d.fname(k) == "BLANK" && d.args(k + 1).is_empty());
        if blank || alt.is_empty() {
            format!("DIVIDE({}, {})", d.text(num), d.text(den))
        } else {
            format!("DIVIDE({}, {}, {})", d.text(num), d.text(den), d.text(alt))
        }
    }

    /// IFERROR and ISERROR: they force a step-by-step evaluation and hide
    /// the model's real errors. Microsoft Learn, "Appropriate use of error
    /// functions":
    /// https://learn.microsoft.com/dax/best-practices/dax-error-functions
    /// (BPA: "Avoid using the IFERROR function").
    ///
    /// The rewrite: `IFERROR(a / b, alt)` → `DIVIDE(a, b, alt)`. In DAX a
    /// division by zero isn't an error (it gives infinity), so IFERROR
    /// didn't catch it and DIVIDE does: compare it first.
    fn iferror(&self, i: usize, open: usize, args: &[Range<usize>], out: &mut Vec<Hit>) {
        let x = &self.d.t[i];
        let fix = match (args, self.call_end(open)) {
            ([e, alt], Some(end)) => self.division(e).map(|(num, den)| Fix {
                rule: "dax_iferror_to_divide",
                start: x.start,
                end,
                text: self.divide_call(&num, &den, alt),
                verify: true,
                params: vec![("division", self.d.text(e).to_string())],
            }),
            _ => None,
        };
        out.push(
            hit("error-functions", x.start, x.end)
                .param("fn", x.text)
                .fix(fix),
        );
    }

    /// ISERROR (see [`Cx::iferror`]). The rewrite, for Microsoft's own
    /// example of the pattern: `IF(ISERROR(a / b), alt, a / b)` →
    /// `DIVIDE(a, b, alt)`.
    fn iserror(&self, i: usize, out: &mut Vec<Hit>) {
        let d = self.d;
        let x = &d.t[i];
        let mut fix = None;
        if i >= 2 && d.p(i - 1, "(") && d.is(i - 2, "IF") {
            let outer = d.args(i - 1);
            let inner = d.args(i + 1);
            if let ([cond, alt, then], [e], Some(end)) =
                (outer.as_slice(), inner.as_slice(), self.call_end(i - 1))
            {
                if d.whole_call(cond) == Some(i) && d.same(e, then) {
                    fix = self.division(e).map(|(num, den)| Fix {
                        rule: "dax_iserror_to_divide",
                        start: d.t[i - 2].start,
                        end,
                        text: self.divide_call(&num, &den, alt),
                        verify: true,
                        params: vec![("division", d.text(e).to_string())],
                    });
                }
            }
        }
        out.push(
            hit("error-functions", x.start, x.end)
                .param("fn", x.text)
                .fix(fix),
        );
    }

    /// `IF(HASONEVALUE(c), VALUES(c)[, alt])` is SELECTEDVALUE's own
    /// definition. Microsoft Learn, "Use SELECTEDVALUE instead of VALUES":
    /// https://learn.microsoft.com/dax/best-practices/dax-selectedvalue
    /// and https://learn.microsoft.com/dax/selectedvalue-function#remarks
    /// (the equivalence). The rewrite is equivalent.
    fn hasonevalue_values(&self, i: usize, open: usize, args: &[Range<usize>], out: &mut Vec<Hit>) {
        let d = self.d;
        if !(args.len() == 2 || args.len() == 3) {
            return;
        }
        let (Some(h), Some(v)) = (d.whole_call(&args[0]), d.whole_call(&args[1])) else {
            return;
        };
        if d.fname(h) != "HASONEVALUE" || d.fname(v) != "VALUES" {
            return;
        }
        let (ha, va) = (d.args(h + 1), d.args(v + 1));
        let ([c], [c2]) = (ha.as_slice(), va.as_slice()) else {
            return;
        };
        if d.reference(c).is_none() || !d.same(c, c2) {
            return;
        }
        let Some(end) = self.call_end(open) else {
            return;
        };
        let column = d.text(c);
        let text = match args.get(2).filter(|a| !a.is_empty()) {
            Some(alt) => format!("SELECTEDVALUE({column}, {})", d.text(alt)),
            None => format!("SELECTEDVALUE({column})"),
        };
        let x = &d.t[i];
        let fix = Fix {
            rule: "dax_selectedvalue",
            start: x.start,
            end,
            text,
            verify: false,
            params: vec![("column", column.to_string())],
        };
        out.push(
            hit("hasonevalue-values", x.start, x.end)
                .param("column", column)
                .fix(Some(fix)),
        );
    }

    /// SUMMARIZE with extension columns ("Name", expression): they're
    /// evaluated in a filter context that's hard to predict (it can give
    /// wrong results) and slower. SQLBI, "All the secrets of SUMMARIZE":
    /// https://www.sqlbi.com/articles/all-the-secrets-of-summarize/ and
    /// "Best practices using SUMMARIZE and ADDCOLUMNS":
    /// https://www.sqlbi.com/articles/best-practices-using-summarize-and-addcolumns/
    ///
    /// The rewrite: ADDCOLUMNS over the grouping SUMMARIZE, each expression
    /// in CALCULATE (a measure as it is). Not the same context in every
    /// model: compare it first.
    fn summarize(&self, i: usize, open: usize, args: &[Range<usize>], out: &mut Vec<Hit>) {
        let d = self.d;
        let Some(k) =
            (1..args.len()).find(|&k| args[k].len() == 1 && d.t[args[k].start].k == K::Str)
        else {
            return;
        };
        if k + 1 >= args.len() {
            return;
        }
        let x = &d.t[i];
        let name = d.t[args[k].start].text;
        out.push(
            hit("summarize-extension-columns", x.start, x.end)
                .param("name", name)
                .fix(self.summarize_fix(i, open, args, k)),
        );
    }

    fn summarize_fix(&self, i: usize, open: usize, args: &[Range<usize>], k: usize) -> Option<Fix> {
        let d = self.d;
        let end = self.call_end(open)?;
        let groups_ok = k >= 2
            && args[0].len() == 1
            && self.is_table(args[0].start)
            && args[1..k]
                .iter()
                .all(|a| d.reference(a).is_some_and(|r| d.table_of(r).is_some()));
        let rollup = (open..d.closing(open)).any(|j| {
            let w = &d.t[j];
            w.k == K::Word
                && (w.text.to_ascii_uppercase().starts_with("ROLLUP") || w.is("ISSUBTOTAL"))
        });
        if !groups_ok || rollup || !(args.len() - k).is_multiple_of(2) {
            return None;
        }
        let x = &d.t[i];
        let mut text = format!(
            "ADDCOLUMNS({})",
            &d.src[x.start..d.t[args[k - 1].end - 1].end]
        );
        for pair in args[k..].chunks(2) {
            let (n, e) = (&pair[0], &pair[1]);
            if n.len() != 1 || d.t[n.start].k != K::Str || e.is_empty() {
                return None;
            }
            let measure = d.reference(e).is_some_and(|r| d.table_of(r).is_none());
            let calculate = d.whole_call(e).is_some_and(|c| d.fname(c) == "CALCULATE");
            let expr = if measure || calculate {
                d.text(e).to_string()
            } else {
                format!("CALCULATE({})", d.text(e))
            };
            text.push_str(&format!(", {}, {expr}", d.text(n)));
        }
        text.push(')');
        Some(Fix {
            rule: "dax_summarize_to_addcolumns",
            start: x.start,
            end,
            text,
            verify: true,
            params: vec![("name", d.t[args[k].start].text.to_string())],
        })
    }

    /// `/` with a denominator that isn't a constant: in DAX a division by
    /// zero or BLANK gives infinity or NaN, not an error. Microsoft Learn,
    /// "DIVIDE function vs. divide operator":
    /// https://learn.microsoft.com/dax/best-practices/dax-divide-function-operator
    /// (BPA: "Use the DIVIDE function for division"). A constant
    /// denominator is fine with `/`, as the article says.
    fn divide(&self, i: usize, out: &mut Vec<Hit>) {
        let d = self.d;
        let mut j = i + 1;
        if d.p(j, "-") || d.p(j, "+") {
            j += 1;
        }
        let Some(den) = d.t.get(j) else { return };
        if den.k == K::Num && !d.p(j + 1, "^") {
            return;
        }
        let x = &d.t[i];
        out.push(hit("divide-operator", x.start, x.end));
    }

    /// Columns are written with their table and measures without it.
    /// Microsoft Learn, "Column and measure references":
    /// https://learn.microsoft.com/dax/best-practices/dax-column-measure-references
    /// (BPA: "Column references should be fully qualified", "Measure
    /// references should be unqualified"). Only the names the script shows
    /// are known: the measures it defines, and the columns it writes with
    /// their table where only a column fits. A measure is flagged only with
    /// its own table: another table's `[name]` is a column of that table.
    fn reference(&self, i: usize, out: &mut Vec<Hit>) {
        let d = self.d;
        let x = &d.t[i];
        let name = x.name();
        let n = self.names;
        if let Some(ti) = d.table_of(i) {
            // `MEASURE 'T'[M] =` names its table by definition.
            let header = ti > 0 && (d.is(ti - 1, "MEASURE") || d.is(ti - 1, "COLUMN"));
            if !header && n.measure(&d.t[ti].name(), &name) {
                out.push(hit("qualified-measure", d.t[ti].start, x.end).param("measure", x.text));
            }
            return;
        }
        if n.measures.contains_key(&name) || n.new_columns.contains(&name) {
            return;
        }
        if let Some([(_, table)]) = n.columns.get(&name).map(Vec::as_slice) {
            out.push(
                hit("unqualified-column", x.start, x.end)
                    .param("column", x.text)
                    .param("qualified", format!("{table}{}", x.text)),
            );
        }
    }

    /// An `EVALUATE` whose rows come from TOPN, without `ORDER BY`: TOPN
    /// doesn't sort its result. Microsoft Learn, TOPN, remarks ("TOPN does
    /// not guarantee any sort order for the results"):
    /// https://learn.microsoft.com/dax/topn-function-dax#remarks
    ///
    /// Only a TOPN that ranks: `TOPN(n, table)` without an order expression
    /// takes a sample (the explorer's own browse, the `ev` snippet), and
    /// `TOPN(1, …)` gives one row or its ties, equal on every order
    /// expression: neither has an order to keep.
    ///
    /// The rewrite adds the `ORDER BY` of TOPN's own order (descending
    /// when TOPN doesn't say), when the `EVALUATE` is the TOPN itself.
    fn topn_without_order_by(&self, out: &mut Vec<Hit>) {
        let d = self.d;
        let t = &d.t;
        for e in 0..t.len() {
            if !(t[e].depth == 0 && t[e].is("EVALUATE")) {
                continue;
            }
            let stop = (e + 1..t.len())
                .find(|&j| {
                    t[j].depth == 0
                        && (t[j].is("EVALUATE")
                            || t[j].is("DEFINE")
                            || (t[j].is("ORDER") && d.is(j + 1, "BY")))
                })
                .unwrap_or(t.len());
            if stop < t.len() && t[stop].is("ORDER") {
                continue;
            }
            let mut r = e + 1..stop;
            // `VAR … RETURN <table>`: the rows are RETURN's.
            if let Some(ret) = r
                .clone()
                .rev()
                .find(|&j| t[j].depth == 0 && t[j].is("RETURN"))
            {
                r = ret + 1..stop;
            }
            let mut wrapped = false;
            let topn = loop {
                let Some(k) = d.whole_call(&r) else {
                    break None;
                };
                match d.fname(k).as_str() {
                    "TOPN" => break Some(k),
                    // They keep the rows (and their lack of order).
                    "ADDCOLUMNS" | "SELECTCOLUMNS" | "CALCULATETABLE" | "FILTER" => {
                        let Some(first) = d.args(k + 1).into_iter().next() else {
                            break None;
                        };
                        r = first;
                        wrapped = true;
                    }
                    _ => break None,
                }
            };
            let Some(k) = topn else { continue };
            let args = d.args(k + 1);
            let ranks = args.len() >= 3 && !args[2].is_empty();
            if !ranks || (args[0].len() == 1 && d.t[args[0].start].text == "1") {
                continue;
            }
            let local: HashSet<String> = (e + 1..stop)
                .filter(|&j| t[j].is("VAR"))
                .filter_map(|j| t.get(j + 1).map(|v| v.name()))
                .collect();
            let fix = (!wrapped)
                .then(|| self.order_by(&args, &local))
                .flatten()
                .map(|order| {
                    let at = t[stop - 1].end;
                    Fix {
                        rule: "dax_topn_order_by",
                        start: at,
                        end: at,
                        text: format!("\n{order}"),
                        verify: false,
                        params: vec![("order", order)],
                    }
                });
            out.push(hit("topn-without-order-by", t[k].start, t[k].end).fix(fix));
        }
    }

    /// TOPN's order (`TOPN(n, table, e1[, order1[, e2[, order2]…]])`) as
    /// an `ORDER BY`, when its expressions don't use the EVALUATE's own
    /// variables (ORDER BY can't see them).
    fn order_by(&self, args: &[Range<usize>], local: &HashSet<String>) -> Option<String> {
        let d = self.d;
        if args.len() < 3 {
            return None;
        }
        let mut parts = Vec::new();
        let mut k = 2;
        while k < args.len() {
            let e = &args[k];
            let uses_local = e
                .clone()
                .any(|j| d.t[j].k == K::Word && local.contains(&d.t[j].name()));
            if e.is_empty() || uses_local {
                return None;
            }
            let dir = args.get(k + 1).and_then(|a| self.direction(a));
            parts.push(format!("{} {}", d.text(e), dir.unwrap_or("DESC")));
            k += if dir.is_some() { 2 } else { 1 };
        }
        Some(format!("ORDER BY {}", parts.join(", ")))
    }

    /// TOPN's order argument: 0 / FALSE / DESC descending, 1 / TRUE / ASC
    /// ascending.
    fn direction(&self, r: &Range<usize>) -> Option<&'static str> {
        let d = self.d;
        let word = match r.len() {
            1 => d.t[r.start].text,
            3 if d.whole_call(r).is_some() => d.t[r.start].text,
            _ => return None,
        };
        match word.to_ascii_uppercase().as_str() {
            "0" | "FALSE" | "DESC" => Some("DESC"),
            "1" | "TRUE" | "ASC" => Some("ASC"),
            _ => None,
        }
    }
}
