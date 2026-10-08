//! "Buscar en la base" from the catalog ([`dbine_driver::Session::search_code`]).
//!
//! The objects are the explorer's (`list_objects`), and each one's source
//! is the text `definition` gives it, put together by the same builders
//! from one query per kind: RDB$RELATIONS (views), RDB$PROCEDURES and
//! RDB$FUNCTIONS with their parameters, RDB$PACKAGES, RDB$TRIGGERS,
//! RDB$GENERATORS and RDB$FIELDS (domains). Lines are matched with the
//! contract's rule, so the hits equal those of the app's per-object scan.
//! Nothing is narrowed on the server: the builders add text of their own
//! (names, parameters, trigger events) around the stored source, and a
//! database's PSQL is small enough to read whole.

use crate::{
    domain_sql, function_sql, package_sql, param, procedure_sql, sequence_sql, text, trigger_sql, view_sql, Column,
    Conn, FirebirdSession, PACKAGE,
};
use dbine_driver::search::{hits_in, CodeSearch, CodeSearchReport};
use dbine_driver::{Result, Session};
use std::collections::HashMap;

/// Each query's last column is the object's name; the others are what
/// the per-object query reads, in its order.
const VIEWS: &str = "SELECT RDB$VIEW_SOURCE, TRIM(RDB$RELATION_NAME) FROM RDB$RELATIONS
  WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$VIEW_BLR IS NOT NULL";

const PROCEDURES: &str = "SELECT RDB$PROCEDURE_SOURCE, TRIM(RDB$PROCEDURE_NAME) FROM RDB$PROCEDURES
  WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL";

const PROCEDURE_PARAMS: &str = "
SELECT TRIM(p.RDB$PARAMETER_NAME), p.RDB$PARAMETER_TYPE, f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE,
       f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE, TRIM(p.RDB$PROCEDURE_NAME)
  FROM RDB$PROCEDURE_PARAMETERS p
  JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = p.RDB$FIELD_SOURCE
 WHERE p.RDB$PACKAGE_NAME IS NULL
 ORDER BY p.RDB$PROCEDURE_NAME, p.RDB$PARAMETER_TYPE, p.RDB$PARAMETER_NUMBER";

const FUNCTIONS: &str = "SELECT RDB$FUNCTION_SOURCE, TRIM(RDB$FUNCTION_NAME) FROM RDB$FUNCTIONS
  WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL";

const FUNCTION_ARGS: &str = "
SELECT TRIM(a.RDB$ARGUMENT_NAME), a.RDB$ARGUMENT_POSITION, f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE,
       f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE, TRIM(a.RDB$FUNCTION_NAME)
  FROM RDB$FUNCTION_ARGUMENTS a
  JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = a.RDB$FIELD_SOURCE
 WHERE a.RDB$PACKAGE_NAME IS NULL
 ORDER BY a.RDB$FUNCTION_NAME, a.RDB$ARGUMENT_POSITION";

const PACKAGES: &str = "SELECT RDB$PACKAGE_HEADER_SOURCE, RDB$PACKAGE_BODY_SOURCE, TRIM(RDB$PACKAGE_NAME) FROM RDB$PACKAGES
  WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0";

const TRIGGERS: &str = "SELECT RDB$TRIGGER_SOURCE, TRIM(RDB$RELATION_NAME), RDB$TRIGGER_TYPE, RDB$TRIGGER_SEQUENCE,
        RDB$TRIGGER_INACTIVE, TRIM(RDB$TRIGGER_NAME)
   FROM RDB$TRIGGERS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0";

const SEQUENCES: &str = "SELECT RDB$INITIAL_VALUE, RDB$GENERATOR_INCREMENT, TRIM(RDB$GENERATOR_NAME) FROM RDB$GENERATORS
  WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0";

const DOMAINS: &str = "
SELECT f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION,
       f.RDB$FIELD_SCALE, f.RDB$DEFAULT_SOURCE, f.RDB$NULL_FLAG, f.RDB$VALIDATION_SOURCE,
       TRIM(cs.RDB$CHARACTER_SET_NAME), TRIM(co.RDB$COLLATION_NAME), TRIM(cs.RDB$DEFAULT_COLLATE_NAME), TRIM(f.RDB$FIELD_NAME)
  FROM RDB$FIELDS f
  LEFT JOIN RDB$CHARACTER_SETS cs ON cs.RDB$CHARACTER_SET_ID = f.RDB$CHARACTER_SET_ID
  LEFT JOIN RDB$COLLATIONS co ON co.RDB$CHARACTER_SET_ID = f.RDB$CHARACTER_SET_ID AND co.RDB$COLLATION_ID = f.RDB$COLLATION_ID
 WHERE COALESCE(f.RDB$SYSTEM_FLAG, 0) = 0 AND f.RDB$FIELD_NAME NOT STARTING WITH 'RDB$'";

/// The rows of `sql` by their last column (the name), without it.
fn by_name(c: &mut Conn, sql: &str) -> Result<Vec<(String, Vec<Column>)>> {
    Ok(c
        .rows(sql, vec![])?
        .into_iter()
        .filter_map(|mut r| {
            let name = r.pop().as_ref().and_then(text)?;
            Some((name, r))
        })
        .collect())
}

/// A routine's [`crate::param`]s.
type Params = Vec<(String, i64, String)>;

/// Every routine's parameters, by routine.
fn params(c: &mut Conn, sql: &str) -> Result<HashMap<String, Params>> {
    let mut out: HashMap<String, Params> = HashMap::new();
    for (name, r) in by_name(c, sql)? {
        out.entry(name).or_default().push(param(&r));
    }
    Ok(out)
}

/// Every wanted kind's sources, by (kind, name).
fn sources(c: &mut Conn, wanted: &[String]) -> Result<HashMap<(String, String), String>> {
    let want = |k: &str| wanted.iter().any(|w| w == k);
    let mut out = HashMap::new();
    let mut put = |kind: &str, name: String, src: Option<String>| {
        if let Some(src) = src {
            out.insert((kind.to_string(), name), src);
        }
    };
    if want("view") {
        for (name, r) in by_name(c, VIEWS)? {
            let src = view_sql(&name, &r);
            put("view", name, src);
        }
    }
    if want("procedure") {
        let params = params(c, PROCEDURE_PARAMS)?;
        for (name, r) in by_name(c, PROCEDURES)? {
            let src = r.first().and_then(text).map(|s| procedure_sql(&name, &s, params.get(&name).map_or(&[][..], Vec::as_slice)));
            put("procedure", name, src);
        }
    }
    if want("function") {
        let args = params(c, FUNCTION_ARGS)?;
        // Legacy UDFs and external functions have no PSQL source.
        for (name, r) in by_name(c, FUNCTIONS)? {
            let src = r.first().and_then(text).map(|s| function_sql(&name, &s, args.get(&name).map_or(&[][..], Vec::as_slice)));
            put("function", name, src);
        }
    }
    if want(PACKAGE) {
        for (name, r) in by_name(c, PACKAGES)? {
            let src = Some(package_sql(&name, &r));
            put(PACKAGE, name, src);
        }
    }
    if want("trigger") {
        for (name, r) in by_name(c, TRIGGERS)? {
            let src = trigger_sql(&name, &r);
            put("trigger", name, src);
        }
    }
    if want("sequence") {
        for (name, r) in by_name(c, SEQUENCES)? {
            let src = Some(sequence_sql(&name, &r));
            put("sequence", name, src);
        }
    }
    if want("type") {
        for (name, r) in by_name(c, DOMAINS)? {
            let src = Some(domain_sql(&name, &r));
            put("type", name, src);
        }
    }
    Ok(out)
}

impl FirebirdSession {
    pub(crate) async fn search_code_impl(&mut self, q: &CodeSearch) -> Result<Option<CodeSearchReport>> {
        if q.text.is_empty() {
            return Ok(None);
        }
        let wanted: Vec<String> = crate::info()
            .object_kinds
            .iter()
            .filter(|k| k.has_definition && (q.kinds.is_empty() || q.kinds.iter().any(|w| w == k.id)))
            .map(|k| k.id.to_string())
            .collect();
        let objects: Vec<_> = Session::list_objects(self).await?.into_iter().filter(|o| wanted.contains(&o.kind)).collect();
        let present: Vec<String> = wanted.into_iter().filter(|k| objects.iter().any(|o| &o.kind == k)).collect();
        let sources = match self.run(move |c| sources(c, &present)).await {
            Ok(s) => s,
            Err(e) => {
                // The scan reads them one by one (and says which fail).
                tracing::debug!("firebird: bulk sources unavailable, the app scans: {e}");
                return Ok(None);
            }
        };
        let mut report = CodeSearchReport { scanned: objects.len(), ..Default::default() };
        for o in &objects {
            // Tables: Firebird has no DDL of its own (no source).
            let Some(source) = sources.get(&(o.kind.clone(), o.name.clone())) else { continue };
            report.hits.extend(hits_in(&o.kind, o.schema.as_deref(), &o.name, o.parent.as_deref(), source, q));
            if q.max_hits > 0 && report.hits.len() >= q.max_hits {
                report.hits.truncate(q.max_hits);
                report.truncated = true;
                break;
            }
        }
        Ok(Some(report))
    }
}

