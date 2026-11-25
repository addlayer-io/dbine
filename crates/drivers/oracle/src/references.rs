//! What an object uses (`Session::references`), from Oracle's dictionary.
//!
//! - Views, materialized views, procedures, functions, packages, triggers,
//!   types and synonyms: `ALL_DEPENDENCIES`, what Oracle recorded when it
//!   compiled them (confirmed). A package is one object in the explorer
//!   (its spec; `definition` gives spec and body together), so what it uses
//!   is what either part records, less the body's own row on its spec. A
//!   type is the same with its body; a materialized view, less its own
//!   container table. A table that is a materialized view's container is
//!   reported as the materialized view, as the explorer lists it.
//! - Tables: the tables their foreign keys point at (`ALL_CONSTRAINTS`),
//!   the types their columns use (`ALL_DEPENDENCIES`) and, for an external
//!   table, its files (`ALL_EXTERNAL_LOCATIONS`, with the directory's path
//!   from `ALL_DIRECTORIES` when the user can read it).
//! - Synonyms: also `ALL_SYNONYMS`, the object they stand for.
//! - Database links go in `external`: a routine's remote objects are in
//!   `ALL_DEPENDENCIES` with `REFERENCED_LINK_NAME`, a synonym's in
//!   `ALL_SYNONYMS.DB_LINK`. Oracle records none for views and materialized
//!   views, so `name@link` in their text is read instead. The server and
//!   the service come from the link's `HOST` (`DBA_DB_LINKS`, else
//!   `ALL_DB_LINKS`, which shows only the user's own and the public links);
//!   without it, the link's name.
//!
//! What Oracle maintains (SYS's STANDARD and DBMS_* packages, the public
//! synonyms of them…) is left out: the explorer doesn't list it. An object
//! that isn't VALID may be recorded only in part, so what its source names
//! is added as the generic scan finds it (probable). If the dictionary
//! can't be read, the generic scan answers, with a note.

use crate::index_usage::{foreign_keys, FkRow, FOREIGN_KEYS_SQL};
use crate::{db_code, err, object_kind, OracleSession, PACKAGE};
use dbine_driver::dependencies::{
    find_mentions, foreign_key_parents, references_scan, used_names, used_objects, Confidence,
    DependencyReport, DependencyScan, DependencyTarget, Dependent, ExternalRef, Mention, Relation,
    UsedName, LINE_CHARS, LOOKUP_CAP, MAX_MENTIONS,
};
use dbine_driver::sql::{code_tokens, NameToken, ScriptDialect, TokenKind};
use dbine_driver::{kinds, DbObject, Error, ObjectQuery, ObjectRef, Result, Session};
use oracledb::{Connection, Row};

/// What the UI says when the dictionary couldn't be read.
pub(crate) const CATALOG_NOTE: &str = "No se pudo leer ALL_DEPENDENCIES: lo que usa se buscó por nombre en su código, sin confirmarlo con el diccionario.";

/// What a database link's remote side is, as `system`.
const LINKED: &str = "Oracle";

/// The `ALL_DEPENDENCIES.TYPE`s of an explorer kind; `None`: the generic
/// scan answers.
fn dependency_types(kind: &str) -> Option<&'static [&'static str]> {
    Some(match kind {
        kinds::TABLE => &["TABLE"],
        kinds::VIEW => &["VIEW"],
        kinds::MATERIALIZED_VIEW => &["MATERIALIZED VIEW"],
        kinds::PROCEDURE => &["PROCEDURE"],
        kinds::FUNCTION => &["FUNCTION"],
        PACKAGE => &["PACKAGE", "PACKAGE BODY"],
        kinds::TRIGGER => &["TRIGGER"],
        kinds::TYPE => &["TYPE", "TYPE BODY"],
        kinds::SYNONYM => &["SYNONYM"],
        _ => return None,
    })
}

/// `:1` the owner, `:2` the name. `{types}` the target's types,
/// `{maintained_ref}` and `{maintained_syn}` whether the referenced owner
/// and a public synonym's target are Oracle's (`maintained`). A row on the
/// object itself (a package body on its spec, a materialized view on its
/// container table) is left out; a trigger may share its table's name.
const DEPENDENCIES_SQL: &str = "SELECT d.referenced_owner, d.referenced_name, d.referenced_type, d.referenced_link_name,
       CASE WHEN d.referenced_link_name IS NULL AND d.referenced_type = 'TABLE' AND EXISTS (
         SELECT 1 FROM all_mviews m WHERE m.owner = d.referenced_owner AND m.mview_name = d.referenced_name)
       THEN 1 ELSE 0 END
  FROM all_dependencies d
 WHERE d.owner = :1 AND d.name = :2 AND d.type IN ({types})
   AND d.referenced_type <> 'NON-EXISTENT'
   AND NOT (d.referenced_link_name IS NULL AND d.type <> 'TRIGGER'
            AND d.referenced_owner = d.owner AND d.referenced_name = d.name)
   AND (d.referenced_link_name IS NOT NULL OR NOT ({maintained_ref}))
   AND (d.referenced_owner <> 'PUBLIC' OR d.referenced_link_name IS NOT NULL OR EXISTS (
         SELECT 1 FROM all_synonyms s
          WHERE s.owner = 'PUBLIC' AND s.synonym_name = d.referenced_name
            AND (s.db_link IS NOT NULL OR NOT ({maintained_syn}))))
 ORDER BY d.referenced_owner, d.referenced_name, d.referenced_type";

/// `{types}` as `DEPENDENCIES_SQL`'s: whether the target (or its body)
/// isn't VALID.
const STATUS_SQL: &str = "SELECT COUNT(*) FROM all_objects
 WHERE owner = :1 AND object_name = :2 AND object_type IN ({types}) AND status <> 'VALID'";

/// A synonym's object and link, and the object's type here (`MIN`: of a
/// materialized view's two rows, MATERIALIZED VIEW before TABLE; no other
/// listed type shares a name).
const SYNONYM_SQL: &str = "SELECT s.table_owner, s.table_name, s.db_link,
       (SELECT MIN(o.object_type) FROM all_objects o
         WHERE s.db_link IS NULL AND o.owner = s.table_owner AND o.object_name = s.table_name
           AND o.object_type IN ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE',
                                 'SEQUENCE','SYNONYM','TYPE'))
  FROM all_synonyms s
 WHERE s.owner = :1 AND s.synonym_name = :2";

/// An external table's access driver and files, each with its directory
/// and that directory's path.
const EXTERNAL_SQL: &str =
    "SELECT x.type_name, l.location, NVL(l.directory_name, x.default_directory_name),
       (SELECT d.directory_path FROM all_directories d
         WHERE d.directory_name = NVL(l.directory_name, x.default_directory_name))
  FROM all_external_tables x
  LEFT JOIN all_external_locations l ON l.owner = x.owner AND l.table_name = x.table_name
 WHERE x.owner = :1 AND x.table_name = :2
 ORDER BY l.location";

/// Every database link, with a DBA's view; the user's and the public ones
/// otherwise (`ALL_DB_LINKS`).
const LINKS_SQL: &str = "SELECT owner, db_link, username, host FROM {view}";

/// SQL that is true when the user `col` names is one Oracle maintains.
pub(crate) fn maintained(col: &str, legacy: bool) -> String {
    if legacy {
        format!("{col} IN ({})", oracle_schemas_11g!())
    } else {
        format!(
            "EXISTS (SELECT 1 FROM all_users u WHERE u.username = {col} AND u.oracle_maintained = 'Y')"
        )
    }
}

fn type_list(types: &[&str]) -> String {
    types
        .iter()
        .map(|t| format!("'{t}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// `DEPENDENCIES_SQL` for `types`; `legacy`: before 12c.
pub(crate) fn dependencies_sql(types: &[&str], legacy: bool) -> String {
    DEPENDENCIES_SQL
        .replace("{types}", &type_list(types))
        .replace(
            "{maintained_ref}",
            &maintained("d.referenced_owner", legacy),
        )
        .replace("{maintained_syn}", &maintained("s.table_owner", legacy))
}

fn status_sql(types: &[&str]) -> String {
    STATUS_SQL.replace("{types}", &type_list(types))
}

/// An `ALL_DEPENDENCIES` row.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DepRow {
    pub owner: String,
    pub name: String,
    pub ty: String,
    pub link: Option<String>,
    /// A TABLE that is a materialized view's container.
    pub mview: bool,
}

/// An `ALL_SYNONYMS` row and its object's type here.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct SynonymRow {
    pub owner: Option<String>,
    pub name: String,
    pub link: Option<String>,
    pub ty: Option<String>,
}

/// An external table's file (`EXTERNAL_SQL`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct FileRow {
    pub driver: Option<String>,
    pub location: Option<String>,
    pub directory: Option<String>,
    pub path: Option<String>,
}

/// A database link (`LINKS_SQL`).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct LinkRow {
    pub owner: String,
    pub name: String,
    pub user: Option<String>,
    pub host: Option<String>,
}

/// What the dictionary says about the target.
#[derive(Debug, Default)]
pub(crate) struct Catalog {
    pub deps: Vec<DepRow>,
    /// The target (or its body) isn't VALID: the rows may be partial.
    pub invalid: bool,
    pub fks: Vec<FkRow>,
    pub synonym: Option<SynonymRow>,
    pub files: Vec<FileRow>,
    pub links: Vec<LinkRow>,
}

fn rows(
    c: &Connection,
    sql: &str,
    owner: &str,
    name: &str,
) -> std::result::Result<Vec<Row>, oracledb::Error> {
    let params: [&dyn oracledb::ToDbValue; 2] = [&owner, &name];
    c.query(sql, &params)?.collect()
}

fn text(r: &Row, i: usize) -> Result<Option<String>> {
    r.get::<Option<String>>(i).map_err(err)
}

fn dep_row(r: &Row) -> Result<DepRow> {
    Ok(DepRow {
        owner: text(r, 0)?.unwrap_or_default(),
        name: text(r, 1)?.unwrap_or_default(),
        ty: text(r, 2)?.unwrap_or_default(),
        link: text(r, 3)?,
        mview: r.get::<Option<i64>>(4).map_err(err)? == Some(1),
    })
}

/// Every link this user may see: DBA_DB_LINKS with the privilege, else
/// ALL_DB_LINKS. Empty when neither can be read (the link's name stands
/// for its server then).
pub(crate) fn read_links(c: &Connection) -> Vec<LinkRow> {
    let read = |view: &str| -> Result<Vec<LinkRow>> {
        let mut out = Vec::new();
        for r in c
            .query(&LINKS_SQL.replace("{view}", view), &[])
            .map_err(err)?
        {
            let r = r.map_err(err)?;
            out.push(LinkRow {
                owner: text(&r, 0)?.unwrap_or_default(),
                name: text(&r, 1)?.unwrap_or_default(),
                user: text(&r, 2)?,
                host: text(&r, 3)?,
            });
        }
        Ok(out)
    };
    read("dba_db_links")
        .or_else(|_| read("all_db_links"))
        .unwrap_or_else(|e| {
            tracing::debug!("oracle: database links not read: {e}");
            Vec::new()
        })
}

/// The dictionary's rows for `owner`.`name` of `kind`. Fails when
/// ALL_DEPENDENCIES (or the table's foreign keys, the synonym) can't be
/// read; the external files and the links are left empty then.
fn read_catalog(
    c: &Connection,
    kind: &str,
    types: &[&str],
    owner: &str,
    name: &str,
) -> Result<Catalog> {
    let deps = match rows(c, &dependencies_sql(types, false), owner, name) {
        // ORA-00904: no ORACLE_MAINTAINED before 12c.
        Err(e) if db_code(&e) == Some(904) => rows(c, &dependencies_sql(types, true), owner, name),
        r => r,
    }
    .map_err(err)?;
    let mut cat = Catalog {
        deps: deps.iter().map(dep_row).collect::<Result<_>>()?,
        ..Default::default()
    };
    cat.invalid = rows(c, &status_sql(types), owner, name)
        .map_err(err)?
        .first()
        .map(|r| r.get::<Option<i64>>(0).map_err(err))
        .transpose()?
        .flatten()
        .unwrap_or(0)
        > 0;
    match kind {
        kinds::TABLE => {
            for r in rows(c, FOREIGN_KEYS_SQL, owner, name).map_err(err)? {
                cat.fks.push((
                    text(&r, 0)?.unwrap_or_default(),
                    text(&r, 1)?.unwrap_or_default(),
                    text(&r, 2)?,
                    text(&r, 3)?.unwrap_or_default(),
                    text(&r, 4)?.unwrap_or_default(),
                    text(&r, 5)?,
                ));
            }
            match rows(c, EXTERNAL_SQL, owner, name) {
                Ok(found) => {
                    for r in found {
                        cat.files.push(FileRow {
                            driver: text(&r, 0)?,
                            location: text(&r, 1)?,
                            directory: text(&r, 2)?,
                            path: text(&r, 3)?,
                        });
                    }
                }
                Err(e) => tracing::debug!("oracle: external table locations not read: {e}"),
            }
        }
        kinds::SYNONYM => {
            if let Some(r) = rows(c, SYNONYM_SQL, owner, name).map_err(err)?.first() {
                cat.synonym = Some(SynonymRow {
                    owner: text(r, 0)?,
                    name: text(r, 1)?.unwrap_or_default(),
                    link: text(r, 2)?,
                    ty: text(r, 3)?,
                });
            }
        }
        _ => {}
    }
    // Only where a link may show: the rows', or the source's `name@link`.
    if cat.deps.iter().any(|d| d.link.is_some())
        || cat.synonym.as_ref().is_some_and(|s| s.link.is_some())
        || cat.invalid
        || matches!(kind, kinds::VIEW | kinds::MATERIALIZED_VIEW)
    {
        cat.links = read_links(c);
    }
    Ok(cat)
}

/// The explorer's kind of a referenced `ALL_DEPENDENCIES.REFERENCED_TYPE`
/// (or `ALL_OBJECTS.OBJECT_TYPE`); `None` for what it doesn't list (Java
/// classes, libraries, operators…).
pub(crate) fn ref_kind(ty: &str, mview: bool) -> Option<&'static str> {
    match ty {
        "TABLE" if mview => Some(kinds::MATERIALIZED_VIEW),
        "PACKAGE BODY" => Some(PACKAGE),
        "TYPE BODY" => Some(kinds::TYPE),
        _ => object_kind(ty),
    }
}

/// An owner as the explorer shows it: none for the session's schema.
pub(crate) fn schema_of(owner: &str, session: &str) -> Option<String> {
    (!owner.is_empty() && owner != session).then(|| owner.to_string())
}

/// Two names of one database link: equal, or one is the other plus its
/// domain (`LNK` and `LNK.EXAMPLE.COM`).
fn same_link(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_uppercase(), b.to_ascii_uppercase());
    a == b || a.starts_with(&format!("{b}.")) || b.starts_with(&format!("{a}."))
}

/// The link `name` as the object's owner reaches it: its own private link,
/// else a public one.
fn find_link<'a>(links: &'a [LinkRow], name: &str, owner: &str) -> Option<&'a LinkRow> {
    let named = || links.iter().filter(|l| same_link(&l.name, name));
    named()
        .find(|l| l.owner == owner)
        .or_else(|| named().find(|l| l.owner == "PUBLIC"))
}

/// One `(KEY=value)` of a connect descriptor.
fn descriptor_value(descriptor: &str, key: &str) -> Option<String> {
    descriptor.split('(').find_map(|part| {
        let (k, v) = part.split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case(key)
            .then(|| v.split(')').next().unwrap_or_default().trim().to_string())
            .filter(|v| !v.is_empty())
    })
}

/// A link's `HOST` as (server, database): a connect descriptor's host,
/// port and service (or SID); an Easy Connect string's
/// `host[:port]` and service; a TNS alias as it is.
pub(crate) fn remote_place(host: &str) -> (String, Option<String>) {
    let h = host.trim();
    if h.starts_with('(') {
        let db = descriptor_value(h, "SERVICE_NAME").or_else(|| descriptor_value(h, "SID"));
        return match descriptor_value(h, "HOST") {
            Some(server) => match descriptor_value(h, "PORT") {
                Some(port) => (format!("{server}:{port}"), db),
                None => (server, db),
            },
            None => (h.to_string(), db),
        };
    }
    if !h.contains('/') && !h.contains(':') {
        return (h.to_string(), None);
    }
    // [[protocol:]//]host[:port][/service[:server][/instance]][?parameters]
    let h = h.split_once("://").map_or(h, |(_, rest)| rest);
    let h = h.trim_start_matches("//");
    let h = h.split('?').next().unwrap_or_default();
    let (server, rest) = h.split_once('/').unwrap_or((h, ""));
    let db = rest
        .split(['/', ':'])
        .next()
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    (server.to_string(), db)
}

/// An object read through a database link.
pub(crate) fn linked(
    links: &[LinkRow],
    link: &str,
    referencing_owner: &str,
    schema: Option<String>,
    name: &str,
) -> ExternalRef {
    let found = find_link(links, link, referencing_owner);
    let (server, database) = match found.and_then(|l| l.host.as_deref()) {
        Some(host) => {
            let (s, d) = remote_place(host);
            (Some(s), d)
        }
        None => (Some(link.to_string()), None),
    };
    ExternalRef {
        system: Some(LINKED.into()),
        server,
        database,
        // An object named without its owner is the link user's.
        schema: schema.or_else(|| found.and_then(|l| l.user.clone())),
        name: Some(name.to_string()),
        detail: Some(format!("@{link}")),
        mentions: Vec::new(),
    }
}

fn confirmed(kind: &str, schema: Option<String>, name: &str) -> Dependent {
    Dependent {
        kind: kind.to_string(),
        schema,
        name: name.to_string(),
        parent: None,
        relation: Relation::Code,
        confidence: Confidence::Confirmed,
        detail: None,
        mentions: Vec::new(),
    }
}

fn same_item(a: &Dependent, b: &Dependent) -> bool {
    a.kind == b.kind && a.schema == b.schema && a.name.eq_ignore_ascii_case(&b.name)
}

fn push_item(items: &mut Vec<Dependent>, d: Dependent) {
    if !items.iter().any(|i| same_item(i, &d)) {
        items.push(d);
    }
}

fn push_external(out: &mut Vec<ExternalRef>, x: ExternalRef) {
    if !out.contains(&x) {
        out.push(x);
    }
}

impl Catalog {
    /// The objects the target uses (confirmed) and its sources outside the
    /// database, from the dictionary's rows. `owner`: the target's;
    /// `session`: the session's schema (its objects go without a schema, as
    /// the explorer lists them).
    pub(crate) fn assemble(
        &self,
        target: &ObjectRef,
        owner: &str,
        session: &str,
    ) -> (Vec<Dependent>, Vec<ExternalRef>) {
        let mut items = Vec::new();
        let mut external = Vec::new();
        if !self.fks.is_empty() {
            for mut d in foreign_key_parents(&foreign_keys(owner, &self.fks), target) {
                d.schema = d.schema.filter(|s| s != session);
                push_item(&mut items, d);
            }
        }
        if let Some(s) = &self.synonym {
            match (&s.link, &s.ty) {
                (Some(link), _) => push_external(
                    &mut external,
                    linked(&self.links, link, owner, s.owner.clone(), &s.name),
                ),
                (None, Some(ty)) => {
                    if let Some(kind) = ref_kind(ty, false) {
                        let schema = s.owner.as_deref().and_then(|o| schema_of(o, session));
                        push_item(&mut items, confirmed(kind, schema, &s.name));
                    }
                }
                // Its object doesn't exist or this user can't see it.
                (None, None) => {}
            }
        }
        for r in &self.deps {
            match &r.link {
                Some(link) => push_external(
                    &mut external,
                    linked(
                        &self.links,
                        link,
                        owner,
                        Some(r.owner.clone()).filter(|o| !o.is_empty()),
                        &r.name,
                    ),
                ),
                None => {
                    if let Some(kind) = ref_kind(&r.ty, r.mview) {
                        push_item(
                            &mut items,
                            confirmed(kind, schema_of(&r.owner, session), &r.name),
                        );
                    }
                }
            }
        }
        for f in &self.files {
            let detail = match (&f.directory, &f.path) {
                (Some(d), Some(p)) => Some(format!("{d} ({p})")),
                (d, _) => d.clone(),
            };
            push_external(
                &mut external,
                ExternalRef {
                    system: f.driver.clone(),
                    name: f.location.clone(),
                    detail,
                    ..Default::default()
                },
            );
        }
        (items, external)
    }
}

/// `name@link` in a body: the name's parts (`[owner.]name`) and the link's,
/// as Oracle folds them (unquoted in upper case).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteName {
    pub parts: Vec<String>,
    pub link: String,
    /// Where the name starts.
    pub at: usize,
    /// Where its last part starts (the generic scan's `UsedName::at`).
    pub name_at: usize,
}

/// Every `[owner.]name@link[.domain]` the code of `body` names (strings and
/// comments don't count).
pub(crate) fn remote_names(body: &str, dialect: &ScriptDialect) -> Vec<RemoteName> {
    let toks = code_tokens(body, dialect);
    let punct = |k: usize, p: &str| {
        toks.get(k)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == p)
    };
    let name = |k: usize| toks.get(k).is_some_and(|t| t.kind == TokenKind::Name);
    let fold = |t: &NameToken<'_>| {
        if body.as_bytes().get(t.start) == Some(&b'"') {
            t.text.to_string()
        } else {
            t.text.to_uppercase()
        }
    };
    let mut out = Vec::new();
    for i in 1..toks.len() {
        if !punct(i, "@") || !name(i - 1) || !name(i + 1) {
            continue;
        }
        let mut first = i - 1;
        while first >= 2 && punct(first - 1, ".") && name(first - 2) {
            first -= 2;
        }
        let mut link = vec![fold(&toks[i + 1])];
        let mut j = i + 1;
        while punct(j + 1, ".") && name(j + 2) {
            link.push(fold(&toks[j + 2]));
            j += 2;
        }
        out.push(RemoteName {
            parts: (first..i).step_by(2).map(|k| fold(&toks[k])).collect(),
            link: link.join("."),
            at: toks[first].start,
            name_at: toks[i - 1].start,
        });
    }
    out
}

/// Whether `r` is the remote object `x` stands for: same link and name,
/// and its owner when written (`owner.name`, or `package.routine`, whose
/// object is the package).
fn names_external(r: &RemoteName, x: &ExternalRef) -> bool {
    let (Some(name), Some(link)) = (x.name.as_deref(), x.detail.as_deref()) else {
        return false;
    };
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    let link = link.strip_prefix('@').unwrap_or(link);
    same_link(&r.link, link)
        && r.parts.iter().any(|p| eq(p, name))
        && (r.parts.len() == 1
            || eq(&r.parts[0], name)
            || x.schema.as_deref().is_some_and(|s| eq(&r.parts[0], s)))
}

/// The line of `body` at `at` as a mention.
fn mention_at(body: &str, at: usize) -> Mention {
    let line = body[..at].bytes().filter(|&b| b == b'\n').count() as u32 + 1;
    let from = body[..at].rfind('\n').map_or(0, |p| p + 1);
    let to = body[at..].find('\n').map_or(body.len(), |p| at + p);
    Mention {
        line,
        text: body[from..to].trim().chars().take(LINE_CHARS).collect(),
        dynamic: false,
    }
}

fn add_mention(mentions: &mut Vec<Mention>, m: Mention) {
    if mentions.len() < MAX_MENTIONS && !mentions.iter().any(|x| x.line == m.line) {
        mentions.push(m);
    }
}

impl Catalog {
    /// What the target's own source adds to `report` (the dictionary's
    /// rows): the lines that name each object and remote source and, with
    /// `text_links` (a view or materialized view, whose links Oracle doesn't
    /// record, or an object that isn't VALID), the `name@link` sources the
    /// rows lack. `owner`: the target's. Returns the remote names.
    pub(crate) fn add_source(
        &self,
        body: &str,
        dialect: &ScriptDialect,
        text_links: bool,
        owner: &str,
        report: &mut DependencyReport,
    ) -> Vec<RemoteName> {
        for d in report
            .items
            .iter_mut()
            .filter(|d| d.relation == Relation::Code)
        {
            let target = DependencyTarget {
                object: ObjectRef {
                    kind: d.kind.clone(),
                    schema: d.schema.clone(),
                    name: d.name.clone(),
                },
                column: None,
            };
            if let Some((_, mentions)) = find_mentions(body, dialect, &target) {
                d.mentions = mentions;
            }
        }
        let remote = remote_names(body, dialect);
        for r in &remote {
            let m = mention_at(body, r.at);
            if let Some(x) = report.external.iter_mut().find(|x| names_external(r, x)) {
                add_mention(&mut x.mentions, m);
            } else if text_links {
                let (schema, name) = match r.parts.as_slice() {
                    [name] => (None, name),
                    [.., schema, name] => (Some(schema.clone()), name),
                    [] => continue,
                };
                let mut x = linked(&self.links, &r.link, owner, schema, name);
                x.mentions.push(m);
                report.external.push(x);
            }
        }
        remote
    }
}

/// Confirmed first, then by relation and name (as the generic scan sorts).
pub(crate) fn sort(items: &mut [Dependent]) {
    items.sort_by(|a, b| {
        (
            a.confidence,
            a.relation,
            a.schema.as_deref().unwrap_or(""),
            a.name.to_lowercase(),
        )
            .cmp(&(
                b.confidence,
                b.relation,
                b.schema.as_deref().unwrap_or(""),
                b.name.to_lowercase(),
            ))
    });
}

impl OracleSession {
    pub(crate) async fn references_impl(
        &mut self,
        target: &DependencyTarget,
        scan: &DependencyScan,
    ) -> Result<DependencyReport> {
        let t = &target.object;
        let Some(types) = dependency_types(&t.kind) else {
            return references_scan(self, target, scan).await;
        };
        let owner = self.owner(t);
        let read = {
            let (kind, owner, name) = (t.kind.clone(), owner.clone(), t.name.clone());
            self.run(move |c| read_catalog(c, &kind, types, &owner, &name))
                .await
        };
        let cat = match read {
            Ok(cat) => cat,
            Err(e) => {
                tracing::debug!("oracle: dependencies not read from the dictionary: {e}");
                let mut report = references_scan(self, target, scan).await?;
                report.note = Some(CATALOG_NOTE.into());
                return Ok(report);
            }
        };
        let mut report = DependencyReport::default();
        (report.items, report.external) = cat.assemble(t, &owner, &self.schema);
        if scan.written_kinds.contains(&t.kind) {
            match self.definition(t).await {
                Ok(Some(body)) => {
                    report.scanned = 1;
                    let text_links = cat.invalid
                        || matches!(t.kind.as_str(), kinds::VIEW | kinds::MATERIALIZED_VIEW);
                    let remote =
                        cat.add_source(&body, &scan.dialect, text_links, &owner, &mut report);
                    if cat.invalid {
                        // Recorded in part, maybe: what the source names too,
                        // as the generic scan finds it (a remote name isn't
                        // one of this database's).
                        let used: Vec<UsedName> = used_names(&body, &scan.dialect)
                            .into_iter()
                            .filter(|u| !remote.iter().any(|r| r.name_at == u.at))
                            .collect();
                        let objects = self.objects_named(&used).await?;
                        for d in used_objects(&body, &used, &objects, t) {
                            push_item(&mut report.items, d);
                        }
                    }
                }
                Ok(None) => {}
                Err(_) => report.unreadable.push(match t.schema() {
                    Some(s) => format!("{s}.{}", t.name),
                    None => t.name.clone(),
                }),
            }
        }
        sort(&mut report.items);
        Ok(report)
    }

    /// The objects named like `used`, as the generic scan looks them up:
    /// one page lookup per name, or the whole list once for many names.
    async fn objects_named(&mut self, used: &[UsedName]) -> Result<Vec<DbObject>> {
        let mut names: Vec<&str> = Vec::new();
        for u in used {
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&u.name)) {
                names.push(&u.name);
            }
        }
        if names.is_empty() {
            return Ok(Vec::new());
        }
        if names.len() > LOOKUP_CAP {
            return self.list_objects().await;
        }
        let mut found = Vec::new();
        for name in names {
            let q = ObjectQuery {
                name: Some(name.to_string()),
                limit: 50,
                ..Default::default()
            };
            match self.list_objects_page(&q).await {
                Ok(page) => found.extend(page.objects),
                Err(Error::Unsupported(_)) => return self.list_objects().await,
                Err(e) => return Err(e),
            }
        }
        Ok(found)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oracle() -> ScriptDialect {
        ScriptDialect::oracle()
    }

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind: kind.into(),
            schema: schema.map(str::to_string),
            name: name.into(),
        }
    }

    fn dep(owner: &str, name: &str, ty: &str, link: Option<&str>, mview: bool) -> DepRow {
        DepRow {
            owner: owner.into(),
            name: name.into(),
            ty: ty.into(),
            link: link.map(str::to_string),
            mview,
        }
    }

    fn link(owner: &str, name: &str, user: Option<&str>, host: Option<&str>) -> LinkRow {
        LinkRow {
            owner: owner.into(),
            name: name.into(),
            user: user.map(str::to_string),
            host: host.map(str::to_string),
        }
    }

    #[test]
    fn the_dictionary_query_binds_the_target_and_leaves_out_oracle_and_itself() {
        let sql = dependencies_sql(dependency_types(PACKAGE).unwrap(), false);
        assert!(sql.contains("FROM all_dependencies d"));
        assert!(sql.contains(
            "WHERE d.owner = :1 AND d.name = :2 AND d.type IN ('PACKAGE', 'PACKAGE BODY')"
        ));
        assert!(sql.contains("d.referenced_type <> 'NON-EXISTENT'"));
        // The body's row on its spec, a materialized view's on its table.
        assert!(sql.contains("AND d.referenced_owner = d.owner AND d.referenced_name = d.name"));
        assert!(sql.contains("d.type <> 'TRIGGER'"));
        assert!(sql.contains(
            "NOT (EXISTS (SELECT 1 FROM all_users u WHERE u.username = d.referenced_owner AND u.oracle_maintained = 'Y'))"
        ));
        assert!(sql.contains(
            "NOT (EXISTS (SELECT 1 FROM all_users u WHERE u.username = s.table_owner AND u.oracle_maintained = 'Y'))"
        ));
        assert!(sql.contains("all_mviews m"));
        assert!(!sql.contains('{'), "{sql}");
        let legacy = dependencies_sql(&["VIEW"], true);
        assert!(legacy.contains("d.type IN ('VIEW')"));
        assert!(!legacy.contains("oracle_maintained"));
        assert!(legacy.contains("NOT (d.referenced_owner IN ('SYS','SYSTEM'"));
        assert!(legacy.contains("NOT (s.table_owner IN ('SYS','SYSTEM'"));
        assert_eq!(
            status_sql(&["TYPE", "TYPE BODY"]),
            "SELECT COUNT(*) FROM all_objects\n WHERE owner = :1 AND object_name = :2 AND object_type IN ('TYPE', 'TYPE BODY') AND status <> 'VALID'"
        );
        assert!(SYNONYM_SQL
            .contains("FROM all_synonyms s\n WHERE s.owner = :1 AND s.synonym_name = :2"));
        assert!(EXTERNAL_SQL.contains("WHERE x.owner = :1 AND x.table_name = :2"));
        assert!(FOREIGN_KEYS_SQL.contains("c.constraint_type = 'R'"));
        assert_eq!(
            LINKS_SQL.replace("{view}", "all_db_links"),
            "SELECT owner, db_link, username, host FROM all_db_links"
        );
    }

    #[test]
    fn kinds_map_to_the_explorers() {
        assert_eq!(dependency_types(kinds::SEQUENCE), None);
        assert_eq!(dependency_types("index"), None);
        assert_eq!(
            dependency_types(kinds::TYPE),
            Some(&["TYPE", "TYPE BODY"][..])
        );
        assert_eq!(ref_kind("TABLE", false), Some(kinds::TABLE));
        assert_eq!(ref_kind("TABLE", true), Some(kinds::MATERIALIZED_VIEW));
        assert_eq!(
            ref_kind("MATERIALIZED VIEW", false),
            Some(kinds::MATERIALIZED_VIEW)
        );
        assert_eq!(ref_kind("PACKAGE BODY", false), Some(PACKAGE));
        assert_eq!(ref_kind("TYPE BODY", false), Some(kinds::TYPE));
        assert_eq!(ref_kind("SEQUENCE", false), Some(kinds::SEQUENCE));
        assert_eq!(ref_kind("SYNONYM", false), Some(kinds::SYNONYM));
        assert_eq!(ref_kind("JAVA CLASS", false), None);
    }

    #[test]
    fn link_hosts_become_server_and_service() {
        assert_eq!(
            remote_place("localhost:1521/FREEPDB1"),
            ("localhost:1521".into(), Some("FREEPDB1".into()))
        );
        assert_eq!(
            remote_place("tcps://db.example.com:2484/ventas.example.com?ssl_server_dn_match=yes"),
            (
                "db.example.com:2484".into(),
                Some("ventas.example.com".into())
            )
        );
        assert_eq!(
            remote_place("//db:1521/orcl:dedicated/inst1"),
            ("db:1521".into(), Some("orcl".into()))
        );
        assert_eq!(
            remote_place("(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST = db.example.com)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=ventas)))"),
            ("db.example.com:1521".into(), Some("ventas".into()))
        );
        assert_eq!(
            remote_place(
                "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db))(CONNECT_DATA=(SID=ORCL)))"
            ),
            ("db".into(), Some("ORCL".into()))
        );
        assert_eq!(remote_place(" VENTAS_TNS "), ("VENTAS_TNS".into(), None));
    }

    #[test]
    fn rows_become_confirmed_objects_and_linked_sources() {
        let cat = Catalog {
            deps: vec![
                dep("HR", "DEPT", "TABLE", None, false),
                dep("HR", "EMP_SEQ", "SEQUENCE", None, false),
                dep("HR", "MV_DEPT", "TABLE", None, true),
                dep("HR", "PKG", "PACKAGE", None, false),
                dep("HR", "PKG", "PACKAGE BODY", None, false),
                dep("PUBLIC", "HR_DEPT", "SYNONYM", None, false),
                dep("SCOTT", "BONUS", "VIEW", None, false),
                dep("SCOTT", "EMP", "TABLE", Some("LNK"), false),
                dep("SCOTT", "EMP", "TABLE", Some("LNK"), false),
                dep("HR", "LOADER", "JAVA CLASS", None, false),
            ],
            links: vec![
                link("PUBLIC", "LNK", None, Some("otro:1521/OTRA")),
                link("HR", "LNK", Some("SCOTT"), Some("localhost:1521/FREEPDB1")),
            ],
            ..Default::default()
        };
        let target = obj(kinds::PROCEDURE, None, "P");
        let (items, external) = cat.assemble(&target, "HR", "HR");
        let got: Vec<_> = items
            .iter()
            .map(|d| (d.kind.as_str(), d.schema.as_deref(), d.name.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (kinds::TABLE, None, "DEPT"),
                (kinds::SEQUENCE, None, "EMP_SEQ"),
                (kinds::MATERIALIZED_VIEW, None, "MV_DEPT"),
                (PACKAGE, None, "PKG"),
                (kinds::SYNONYM, Some("PUBLIC"), "HR_DEPT"),
                (kinds::VIEW, Some("SCOTT"), "BONUS"),
            ]
        );
        assert!(items
            .iter()
            .all(|d| d.relation == Relation::Code && d.confidence == Confidence::Confirmed));
        // The owner's private link before the public one of the same name.
        assert_eq!(
            external,
            vec![ExternalRef {
                system: Some("Oracle".into()),
                server: Some("localhost:1521".into()),
                database: Some("FREEPDB1".into()),
                schema: Some("SCOTT".into()),
                name: Some("EMP".into()),
                detail: Some("@LNK".into()),
                mentions: Vec::new(),
            }]
        );
        // Another schema's objects keep it; the session's go without.
        let (items, _) = cat.assemble(&obj(kinds::PROCEDURE, Some("HR"), "P"), "HR", "SCOTT");
        assert_eq!(items[0].schema.as_deref(), Some("HR"));
        assert_eq!(items[5].schema, None);
        // A link this user can't see: its name stands for the server.
        let cat = Catalog {
            deps: vec![dep("SCOTT", "EMP", "TABLE", Some("LNK.EXAMPLE.COM"), false)],
            ..Default::default()
        };
        let (_, external) = cat.assemble(&target, "HR", "HR");
        assert_eq!(external[0].server.as_deref(), Some("LNK.EXAMPLE.COM"));
        assert_eq!(external[0].database, None);
        assert_eq!(external[0].schema.as_deref(), Some("SCOTT"));
    }

    #[test]
    fn a_tables_foreign_keys_and_files() {
        let cat = Catalog {
            fks: vec![
                (
                    "FK_EMP_DEPT".into(),
                    "DEPT_ID".into(),
                    Some("HR".into()),
                    "DEPT".into(),
                    "ID".into(),
                    Some("NO ACTION".into()),
                ),
                (
                    "FK_EMP_PAIS".into(),
                    "PAIS".into(),
                    Some("REF".into()),
                    "PAISES".into(),
                    "CODIGO".into(),
                    None,
                ),
            ],
            deps: vec![dep("HR", "T_ADDR", "TYPE", None, false)],
            files: vec![
                FileRow {
                    driver: Some("ORACLE_LOADER".into()),
                    location: Some("data.csv".into()),
                    directory: Some("DATA_DIR".into()),
                    path: Some("/u01/data".into()),
                },
                FileRow {
                    driver: Some("ORACLE_LOADER".into()),
                    location: Some("more.csv".into()),
                    directory: Some("OTHER_DIR".into()),
                    path: None,
                },
            ],
            ..Default::default()
        };
        let (items, external) = cat.assemble(&obj(kinds::TABLE, None, "EMP"), "HR", "HR");
        let fk = &items[0];
        assert_eq!(
            (
                fk.kind.as_str(),
                fk.schema.as_deref(),
                fk.name.as_str(),
                fk.relation,
                fk.confidence
            ),
            (
                kinds::TABLE,
                None,
                "DEPT",
                Relation::ForeignKey,
                Confidence::Confirmed
            )
        );
        assert_eq!(
            fk.detail.as_deref(),
            Some("FK_EMP_DEPT (DEPT_ID) → DEPT (ID)")
        );
        assert_eq!(
            (items[1].schema.as_deref(), items[1].name.as_str()),
            (Some("REF"), "PAISES")
        );
        assert_eq!(
            (items[2].kind.as_str(), items[2].relation),
            (kinds::TYPE, Relation::Code)
        );
        assert_eq!(
            external,
            vec![
                ExternalRef {
                    system: Some("ORACLE_LOADER".into()),
                    name: Some("data.csv".into()),
                    detail: Some("DATA_DIR (/u01/data)".into()),
                    ..Default::default()
                },
                ExternalRef {
                    system: Some("ORACLE_LOADER".into()),
                    name: Some("more.csv".into()),
                    detail: Some("OTHER_DIR".into()),
                    ..Default::default()
                },
            ]
        );
    }

    #[test]
    fn a_synonyms_object_here_or_through_a_link() {
        let local = Catalog {
            synonym: Some(SynonymRow {
                owner: Some("SCOTT".into()),
                name: "MV".into(),
                link: None,
                ty: Some("MATERIALIZED VIEW".into()),
            }),
            // ALL_DEPENDENCIES says the same: once.
            deps: vec![dep("SCOTT", "MV", "TABLE", None, true)],
            ..Default::default()
        };
        let (items, external) = local.assemble(&obj(kinds::SYNONYM, None, "S"), "HR", "HR");
        assert_eq!(items.len(), 1);
        assert_eq!(
            (
                items[0].kind.as_str(),
                items[0].schema.as_deref(),
                items[0].name.as_str()
            ),
            (kinds::MATERIALIZED_VIEW, Some("SCOTT"), "MV")
        );
        assert!(external.is_empty());
        let remote = Catalog {
            synonym: Some(SynonymRow {
                owner: None,
                name: "DEPT".into(),
                link: Some("LNK".into()),
                ty: None,
            }),
            links: vec![link("HR", "LNK", Some("LNPROBE"), Some("db:1521/PDB"))],
            ..Default::default()
        };
        let (items, external) = remote.assemble(&obj(kinds::SYNONYM, None, "S"), "HR", "HR");
        assert!(items.is_empty());
        assert_eq!(
            (
                external[0].server.as_deref(),
                external[0].database.as_deref(),
                external[0].schema.as_deref(),
                external[0].name.as_deref(),
                external[0].detail.as_deref()
            ),
            (
                Some("db:1521"),
                Some("PDB"),
                Some("LNPROBE"),
                Some("DEPT"),
                Some("@LNK")
            )
        );
        // A synonym of nothing visible: nothing.
        let broken = Catalog {
            synonym: Some(SynonymRow {
                owner: Some("X".into()),
                name: "GONE".into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            broken.assemble(&obj(kinds::SYNONYM, None, "S"), "HR", "HR"),
            (vec![], vec![])
        );
    }

    #[test]
    fn remote_names_in_code() {
        let d = oracle();
        let body = "CREATE OR REPLACE FORCE EDITIONABLE VIEW \"HR\".\"V\" (\"ID\") AS\n  SELECT d.id FROM dept@LNK d\n  JOIN scott.\"Emp\"@lnk.example.com e ON e.id = d.id -- x@nope\n  WHERE d.n = 'a@b' AND EXISTS (SELECT 1 FROM \"DEPT\"@\"LNK\" \"DEPT\")";
        let r = remote_names(body, &d);
        let got: Vec<_> = r
            .iter()
            .map(|r| (r.parts.clone(), r.link.as_str()))
            .collect();
        assert_eq!(
            got,
            vec![
                (vec!["DEPT".to_string()], "LNK"),
                (
                    vec!["SCOTT".to_string(), "Emp".to_string()],
                    "LNK.EXAMPLE.COM"
                ),
                (vec!["DEPT".to_string()], "LNK"),
            ]
        );
        assert_eq!(&body[r[1].at..r[1].at + 5], "scott");
        assert_eq!(&body[r[1].name_at..r[1].name_at + 5], "\"Emp\"");
    }

    #[test]
    fn source_adds_mentions_and_a_views_links() {
        let d = oracle();
        let body = "CREATE OR REPLACE VIEW \"V\" AS\nSELECT d.id FROM dept d\nJOIN emp@lnk e ON e.dept_id = d.id\nUNION ALL SELECT id FROM scott.bonus@lnk";
        let cat = Catalog {
            links: vec![link("HR", "LNK", Some("LNPROBE"), Some("db:1521/PDB"))],
            ..Default::default()
        };
        let mut report = DependencyReport {
            items: vec![confirmed(kinds::TABLE, None, "DEPT")],
            ..Default::default()
        };
        let remote = cat.add_source(body, &d, true, "HR", &mut report);
        assert_eq!(remote.len(), 2);
        assert_eq!(report.items[0].mentions.len(), 1);
        assert_eq!(report.items[0].mentions[0].line, 2);
        let got: Vec<_> = report
            .external
            .iter()
            .map(|x| {
                (
                    x.schema.as_deref(),
                    x.name.as_deref(),
                    x.server.as_deref(),
                    x.mentions.iter().map(|m| m.line).collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                (Some("LNPROBE"), Some("EMP"), Some("db:1521"), vec![3]),
                (Some("SCOTT"), Some("BONUS"), Some("db:1521"), vec![4]),
            ]
        );
        // A routine's links are the dictionary's: its source only points at
        // them (`pkg.proc@lnk` names the package).
        let body = "PROCEDURE p IS\nBEGIN\n  remote_pkg.run@lnk(1);\n  INSERT INTO log@lnk VALUES (1);\nEND;";
        let mut report = DependencyReport {
            external: vec![linked(
                &cat.links,
                "LNK",
                "HR",
                Some("LNPROBE".into()),
                "REMOTE_PKG",
            )],
            ..Default::default()
        };
        cat.add_source(body, &d, false, "HR", &mut report);
        assert_eq!(report.external.len(), 1);
        assert_eq!(
            report.external[0]
                .mentions
                .iter()
                .map(|m| m.line)
                .collect::<Vec<_>>(),
            vec![3]
        );
    }

    #[test]
    fn sorted_as_the_generic_scan() {
        let mut items = vec![
            Dependent {
                confidence: Confidence::Probable,
                ..confirmed(kinds::TABLE, None, "A")
            },
            confirmed(kinds::VIEW, None, "z"),
            Dependent {
                relation: Relation::ForeignKey,
                ..confirmed(kinds::TABLE, None, "M")
            },
        ];
        sort(&mut items);
        let names: Vec<_> = items.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["M", "z", "A"]);
    }
}
