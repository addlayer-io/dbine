//! The function catalog (issue #97) of the engines whose `pg_proc` is
//! PostgreSQL's: the query, how its rows become entries, and the static
//! catalog of PostgreSQL's documented functions for when the query fails.
//! Shared with the Aurora DSQL driver.

use dbine_driver::functions::parse_catalog;
use dbine_driver::{FunctionInfo, FunctionKind};

/// PostgreSQL's documented functions (postgresql.org/docs/current/functions.html),
/// marked with the version that added them.
pub const POSTGRES: &str = include_str!("functions/postgres.txt");

/// The support functions only the server calls, as the catalog column that
/// names them and the `server_version_num` that added the column: the
/// types' input and output functions, index access method support, the
/// aggregates' transition, final, moving-aggregate (9.4) and partial
/// aggregation (9.6) functions, and the range types' canonical and
/// difference functions (9.2).
const SUPPORT: &[(&str, &str, i32)] = &[
    ("pg_type", "typinput", 0),
    ("pg_type", "typoutput", 0),
    ("pg_type", "typreceive", 0),
    ("pg_type", "typsend", 0),
    ("pg_amproc", "amproc", 0),
    ("pg_aggregate", "aggtransfn", 0),
    ("pg_aggregate", "aggfinalfn", 0),
    ("pg_aggregate", "aggmtransfn", 90400),
    ("pg_aggregate", "aggminvtransfn", 90400),
    ("pg_aggregate", "aggmfinalfn", 90400),
    ("pg_aggregate", "aggcombinefn", 90600),
    ("pg_aggregate", "aggserialfn", 90600),
    ("pg_aggregate", "aggdeserialfn", 90600),
    ("pg_range", "rngcanonical", 90200),
    ("pg_range", "rngsubdiff", 90200),
];

/// Every function a user can call, one row per overload: schema, name,
/// kind (`a` aggregate, `w` window, `t` set-returning, `s` scalar), the
/// arguments and result as `\df` prints them, the comment, and whether it
/// implements an operator. Left out: procedures, trigger and event-trigger
/// functions, handlers, and what only the server calls (functions taking or
/// returning `internal` or `cstring`, and the [`SUPPORT`] functions whose
/// column `version` has; 0 for only the ones every version has).
///
/// `modern`: `prokind` (PostgreSQL 11+); before it, `proisagg` and
/// `proiswindow`.
pub fn pg_proc_query(modern: bool, version: i32) -> String {
    let (kind, routines) = if modern {
        (
            "CASE WHEN p.prokind = 'a' THEN 'a' WHEN p.prokind = 'w' THEN 'w' WHEN p.proretset THEN 't' ELSE 's' END",
            "p.prokind <> 'p'",
        )
    } else {
        (
            "CASE WHEN p.proisagg THEN 'a' WHEN p.proiswindow THEN 'w' WHEN p.proretset THEN 't' ELSE 's' END",
            "true",
        )
    };
    // A NULL in the list would make `NOT IN` NULL for every row, on a
    // variant that leaves the column empty instead of 0.
    let support = SUPPORT
        .iter()
        .filter(|(_, _, since)| *since <= version)
        .map(|(table, column, _)| format!("SELECT x.{column}::oid FROM {table} x"))
        .collect::<Vec<_>>()
        .join("\n                                   UNION ALL ");
    format!(
        "SELECT n.nspname::text, p.proname::text, {kind},
                pg_get_function_arguments(p.oid), pg_get_function_result(p.oid),
                obj_description(p.oid, 'pg_proc'),
                p.oid IN (SELECT o.oprcode::oid FROM pg_operator o)
         FROM pg_proc p
         JOIN pg_namespace n ON n.oid = p.pronamespace
         JOIN pg_type rt ON rt.oid = p.prorettype
         WHERE {routines}
           AND n.nspname <> 'information_schema'
           AND n.nspname NOT LIKE 'pg\\_toast%' AND n.nspname NOT LIKE 'pg\\_temp%'
           AND rt.typname NOT IN ('trigger', 'event_trigger', 'internal', 'cstring')
           AND rt.typname NOT LIKE '%\\_handler'
           AND NOT EXISTS (SELECT 1 FROM pg_type at WHERE at.oid = ANY (p.proargtypes)
                           AND at.typname IN ('internal', 'cstring', '_cstring'))
           AND p.oid NOT IN (SELECT s.f FROM ({support}) s(f) WHERE s.f IS NOT NULL)"
    )
}

/// The forms of [`pg_proc_query`] to try, in order, on a server reporting
/// `version` (`server_version_num`, 0 if unknown): with every support
/// column the version has, then with only the ones every version has, for
/// a variant that lacks a newer one. An unknown version tries `prokind`
/// first and then the older form.
pub fn pg_proc_queries(version: i32) -> Vec<String> {
    let forms = if version == 0 {
        [(true, 110000), (true, 0), (false, 0)].to_vec()
    } else {
        [(modern(version), version), (modern(version), 0)].to_vec()
    };
    let mut out: Vec<String> = Vec::new();
    for (modern, version) in forms {
        let sql = pg_proc_query(modern, version);
        if !out.contains(&sql) {
            out.push(sql);
        }
    }
    out
}

/// Schemas whose functions are the server's own machinery, on any
/// variant: CockroachDB's, Materialize's, Redshift's, openGauss's
/// performance views and debuggers, TimescaleDB's `_timescaledb_*`.
const INTERNAL_SCHEMAS: &[&str] = &[
    "crdb_internal",
    "mz_internal",
    "mz_unsafe",
    "pg_internal",
    "dbe_perf",
    "dbe_pldebugger",
    "dbe_pldeveloper",
    "pkg_service",
    "sqladvisor",
];

/// Schemas of built-ins reached by their bare name.
const BUILTIN_SCHEMAS: &[&str] = &["pg_catalog"];

/// Operator implementations the documentation lists as functions too.
const DOCUMENTED_OPERATORS: &[&str] = &["starts_with", "array_cat", "tsquery_phrase"];

/// `pg_catalog` functions the documentation leaves out: what the system
/// views read, server internals, and the leftovers of older releases.
const UNDOCUMENTED: &[&str] = &[
    "aclinsert",
    "aclremove",
    "amvalidate",
    "currtid2",
    "dexp",
    "dlog1",
    "dlog10",
    "dround",
    "dtrunc",
    "enum_send",
    "getpgusername",
    "int2int4_sum",
    "int4inc",
    "int8_sum",
    "int8dec",
    "int8dec_any",
    "like_escape",
    "nameconcatoid",
    "numeric_div_trunc",
    "numeric_exp",
    "numeric_inc",
    "numeric_ln",
    "numeric_log",
    "numeric_sqrt",
    "pg_available_extension_versions",
    "pg_available_extensions",
    "pg_char_to_encoding",
    "pg_column_is_updatable",
    "pg_config",
    "pg_cursor",
    "pg_encoding_max_length",
    "pg_encoding_to_char",
    "pg_get_aios",
    "pg_get_backend_memory_contexts",
    "pg_get_dsm_registry_allocations",
    "pg_get_function_arg_default",
    "pg_get_function_sqlbody",
    "pg_get_loaded_modules",
    "pg_get_multixact_members",
    "pg_get_partition_constraintdef",
    "pg_get_partkeydef",
    "pg_get_publication_tables",
    "pg_get_replica_identity_index",
    "pg_get_replication_slots",
    "pg_get_shmem_allocations",
    "pg_get_shmem_allocations_numa",
    "pg_get_statisticsobjdef_columns",
    "pg_get_statisticsobjdef_expressions",
    "pg_get_wait_events",
    "pg_hba_file_rules",
    "pg_ident_file_mappings",
    "pg_indexam_progress_phasename",
    "pg_isolation_test_session_is_blocked",
    "pg_lock_status",
    "pg_nextoid",
    "pg_prepared_statement",
    "pg_prepared_xact",
    "pg_read_file_old",
    "pg_relation_is_publishable",
    "pg_relation_is_updatable",
    "pg_rotate_logfile_old",
    "pg_sequence_last_value",
    "pg_sequence_parameters",
    "pg_show_all_file_settings",
    "pg_show_all_settings",
    "pg_show_replication_origin_status",
    "pg_stat_force_next_flush",
    "pg_stop_making_pinned_objects",
    "pg_timezone_abbrevs",
    "pg_timezone_abbrevs_abbrevs",
    "pg_timezone_abbrevs_zone",
    "pg_timezone_names",
    "satisfies_hash_partition",
    "similar_escape",
    "similar_to_escape",
    "textlen",
    "xmlvalidate",
];

/// Besides the list, by name: the statistics views' readers, the upgrade
/// and function-manager hooks, the languages' validators, and the support
/// functions of aggregates and range types, for a server whose catalog
/// [`pg_proc_query`] couldn't read them from (`_combine`, `_accum_inv`,
/// `_canonical`, `_subdiff`).
fn undocumented(name: &str) -> bool {
    UNDOCUMENTED.contains(&name)
        || (name.starts_with("pg_stat_get_")
            && !name.starts_with("pg_stat_get_backend_")
            && !matches!(
                name,
                "pg_stat_get_activity" | "pg_stat_get_snapshot_timestamp"
            ))
        || name.starts_with("binary_upgrade_")
        || name.starts_with("fmgr_")
        || [
            "_validator",
            "_subdiff",
            "_combine",
            "_accum_inv",
            "_canonical",
        ]
        .iter()
        .any(|s| name.ends_with(s))
}

/// A row of [`pg_proc_query`] as an entry; `None` for what isn't for
/// users. Built-ins (`pg_catalog`) go without a schema and without their
/// undocumented internals; everything else (extensions, the user's own)
/// carries its schema.
pub fn pg_entry(
    schema: &str,
    name: &str,
    kind: &str,
    args: &str,
    returns: &str,
    description: &str,
    implements_operator: bool,
) -> Option<FunctionInfo> {
    if name.is_empty() || INTERNAL_SCHEMAS.contains(&schema) || schema.starts_with('_') {
        return None;
    }
    let builtin = BUILTIN_SCHEMAS.contains(&schema);
    if (builtin || schema == "gp_toolkit") && name.starts_with('_') {
        return None;
    }
    if builtin && undocumented(name) {
        return None;
    }
    // The function behind an operator (`int4pl` for `+`) carries the
    // comment PostgreSQL gives those, or none.
    let description = description.trim();
    if implements_operator
        && (description.is_empty() || description.starts_with("implementation of"))
        && !(builtin && DOCUMENTED_OPERATORS.contains(&name))
    {
        return None;
    }
    let kind = match kind {
        "a" => FunctionKind::Aggregate,
        "w" => FunctionKind::Window,
        "t" => FunctionKind::Table,
        _ => FunctionKind::Scalar,
    };
    let args = if args.is_empty() && kind == FunctionKind::Aggregate {
        "(*)".to_string()
    } else {
        format!("({args})")
    };
    let f = FunctionInfo::new(name, kind)
        .args(args)
        .returns(returns)
        .description(description);
    Some(if builtin { f } else { f.schema(schema) })
}

/// `server_version_num` (`160002`, `90426`) as a dotted version (`16.2`,
/// `9.4.26`); `None` when unknown.
pub fn version_text(num: i32) -> Option<String> {
    if num <= 0 {
        return None;
    }
    let major = num / 10000;
    Some(if major >= 10 {
        format!("{major}.{}", num % 10000)
    } else {
        format!("{major}.{}.{}", (num / 100) % 100, num % 100)
    })
}

/// The static catalog for a server reporting `version` (`server_version_num`).
pub fn fallback(version: i32) -> Vec<FunctionInfo> {
    let v = version_text(version);
    parse_catalog(POSTGRES, v.as_deref())
}

/// Whether `version` (`server_version_num`, 0 if unknown) has `prokind`.
pub fn modern(version: i32) -> bool {
    version == 0 || version >= 110000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_postgres_catalog_parses_line_by_line() {
        let mut entries = 0;
        for line in POSTGRES.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
            entries += 1;
        }
        assert!(entries > 400, "{entries}");
    }

    #[test]
    fn rows_map_to_entries() {
        let f = pg_entry(
            "pg_catalog",
            "string_agg",
            "a",
            "value text, delimiter text",
            "text",
            "concatenate aggregate input into a string",
            false,
        )
        .unwrap();
        assert_eq!(f.kind, FunctionKind::Aggregate);
        assert_eq!(f.args, "(value text, delimiter text)");
        assert_eq!(f.returns, "text");
        assert_eq!(f.description, "concatenate aggregate input into a string");
        assert_eq!(f.schema, "");

        let count = pg_entry("pg_catalog", "count", "a", "", "bigint", "", false).unwrap();
        assert_eq!(count.args, "(*)");
        let now = pg_entry("pg_catalog", "now", "s", "", "timestamptz", "", false).unwrap();
        assert_eq!(now.args, "()");
        let series = pg_entry(
            "pg_catalog",
            "generate_series",
            "t",
            "integer, integer",
            "SETOF integer",
            "",
            false,
        )
        .unwrap();
        assert_eq!(series.kind, FunctionKind::Table);
        assert_eq!(
            pg_entry("pg_catalog", "rank", "w", "", "bigint", "", false)
                .unwrap()
                .kind,
            FunctionKind::Window
        );

        // The user's and the extensions' carry their schema.
        let mine = pg_entry("sales", "net", "s", "x numeric", "numeric", "", false).unwrap();
        assert_eq!(mine.schema, "sales");
        assert_eq!(
            pg_entry("public", "time_bucket", "s", "", "", "", false)
                .unwrap()
                .schema,
            "public"
        );

        // Operator implementations, internals, undocumented ones.
        assert!(pg_entry(
            "pg_catalog",
            "int4pl",
            "s",
            "integer, integer",
            "integer",
            "implementation of + operator",
            true
        )
        .is_none());
        assert!(pg_entry("public", "geometry_lt", "s", "", "boolean", "", true).is_none());
        assert!(pg_entry(
            "pg_catalog",
            "starts_with",
            "s",
            "text, text",
            "boolean",
            "implementation of ^@ operator",
            true
        )
        .is_some());
        assert!(pg_entry(
            "pg_catalog",
            "array_append",
            "s",
            "",
            "",
            "append element",
            true
        )
        .is_some());
        for (schema, name) in [
            ("pg_catalog", "pg_stat_get_numscans"),
            ("pg_catalog", "pg_show_all_settings"),
            ("pg_catalog", "plpgsql_validator"),
            ("pg_catalog", "int4_avg_combine"),
            ("pg_catalog", "int4_avg_accum_inv"),
            ("pg_catalog", "int4range_canonical"),
            ("pg_catalog", "numeric_sqrt"),
            ("pg_catalog", "similar_to_escape"),
            ("pg_catalog", "_st_contains"),
            ("crdb_internal", "cluster_id"),
            ("_timescaledb_functions", "chunk_status"),
            ("gp_toolkit", "__gp_aovisimap"),
            ("dbe_perf", "get_instr_wait_event"),
        ] {
            assert!(
                pg_entry(schema, name, "s", "", "", "", false).is_none(),
                "{name}"
            );
        }
        assert!(pg_entry(
            "pg_catalog",
            "pg_stat_get_backend_pid",
            "s",
            "",
            "",
            "",
            false
        )
        .is_some());
        assert!(pg_entry("pg_catalog", "pg_size_pretty", "s", "", "", "", false).is_some());
        // A user's function whose name starts with `_` stays, and so do
        // the names that only look like support functions.
        assert!(pg_entry("public", "_helper", "s", "", "", "", false).is_some());
        assert!(pg_entry("sales", "order_combine", "s", "", "", "", false).is_some());
    }

    #[test]
    fn the_query_leaves_out_the_support_columns_the_version_lacks() {
        let pg9_4 = pg_proc_query(false, 90426);
        assert!(pg9_4.contains("proisagg") && !pg9_4.contains("prokind"));
        assert!(pg9_4.contains("aggminvtransfn") && pg9_4.contains("rngcanonical"));
        assert!(!pg9_4.contains("aggcombinefn"));
        let pg9_2 = pg_proc_query(false, 90204);
        assert!(pg9_2.contains("rngcanonical") && !pg9_2.contains("aggmtransfn"));
        let base = pg_proc_query(true, 0);
        assert!(base.contains("aggfinalfn") && !base.contains("pg_range"));
        let pg16 = pg_proc_query(true, 160000);
        for column in ["aggcombinefn", "aggdeserialfn", "aggmfinalfn", "rngsubdiff"] {
            assert!(pg16.contains(column), "{column}");
        }
        assert!(pg16.contains("IS NOT NULL"));

        // Every support column first, then the ones every version has.
        let forms = pg_proc_queries(160000);
        assert_eq!(forms, [pg16, base.clone()]);
        assert_eq!(pg_proc_queries(0).len(), 3);
        assert_eq!(pg_proc_queries(0)[1], base);
        assert_eq!(pg_proc_queries(80000).len(), 1);
    }

    #[test]
    fn versions_read_from_server_version_num() {
        assert_eq!(version_text(160002).as_deref(), Some("16.2"));
        assert_eq!(version_text(90426).as_deref(), Some("9.4.26"));
        assert_eq!(version_text(0), None);
        assert!(modern(0) && modern(110000) && !modern(90426));
    }

    #[test]
    fn the_fallback_follows_the_version() {
        let names = |v| fallback(v).into_iter().map(|f| f.name).collect::<Vec<_>>();
        let pg9 = names(90426);
        let pg17 = names(170002);
        assert!(pg9.iter().any(|n| n == "string_agg"));
        assert!(!pg9.iter().any(|n| n == "gen_random_uuid"));
        assert!(!pg9.iter().any(|n| n == "jsonb_path_query"));
        assert!(pg17.iter().any(|n| n == "json_value"));
        assert!(pg17.iter().any(|n| n == "any_value"));
        assert!(!names(150000).iter().any(|n| n == "any_value"));
        assert!(names(0).iter().any(|n| n == "json_value"));
    }
}
