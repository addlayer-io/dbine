//! Which reads may run without asking (docs/mcp.md, "Reads"). A server
//! read-only transaction is rolled back, but some functions act outside it
//! (WAL replay, backups, statistics resets, advisory locks, signals to other
//! backends, dblink, untrusted languages) or run SQL handed to them as text
//! (`ts_stat`, `query_to_xml`, `crosstab`…). No denylist can name them all,
//! so the approval-free path takes an allowlist: every function the
//! statement calls has to be a side-effect-free built-in of the engine's
//! family. Anything else (a function not listed, a user-defined one, a
//! qualified or quoted name, a non-SQL language) goes to the user's approval.

use dbine_driver::sql::{
    expose_versioned, name_tokens, NameToken, ScriptDialect, ScriptStatement, StatementKind,
    TokenKind,
};
use dbine_driver::Language;

/// The engine families with their own built-ins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Postgres,
    Mysql,
    Sqlite,
    Tsql,
    Oracle,
    Other,
}

impl Family {
    fn of(hint: &str) -> Self {
        match hint {
            "postgres" => Family::Postgres,
            "mysql" => Family::Mysql,
            "sqlite" => Family::Sqlite,
            "mssql" | "sybase" => Family::Tsql,
            "oracle" => Family::Oracle,
            _ => Family::Other,
        }
    }

    /// Whether `name` (lowercase) is a listed built-in. An unqualified name
    /// that isn't a built-in resolves to a user-defined function in
    /// PostgreSQL, MySQL and Oracle, so their lists hold only their own
    /// built-ins. SQLite has no SQL-defined functions and SQL Server needs a
    /// schema for one: an unknown name there is an error, not a call.
    fn lists(self, name: &str) -> bool {
        let any = |lists: &[&[&str]]| lists.iter().any(|l| l.contains(&name));
        match self {
            Family::Postgres => any(&[STANDARD, POSTGRES]),
            Family::Mysql => any(&[STANDARD, MYSQL]),
            Family::Oracle => any(&[STANDARD, ORACLE]),
            Family::Sqlite | Family::Tsql => {
                any(&[STANDARD, POSTGRES, MYSQL, ORACLE, SQLITE, TSQL])
            }
            Family::Other => any(&[STANDARD]),
        }
    }

    /// The schema a built-in may be qualified with (`pg_catalog.lower(…)`).
    fn system_schema(self) -> Option<&'static str> {
        match self {
            Family::Postgres => Some("pg_catalog"),
            _ => None,
        }
    }
}

// Never in any list: anything that locks, signals, sleeps, touches
// sequences, settings, files or the network, or runs SQL handed to it as
// text (the test `mcp_allowlist_never_lists_what_acts_or_runs_sql_text`).

/// Built-ins of every SQL engine: the standard's aggregates, window
/// functions and the few scalar functions they all share.
const STANDARD: &[&str] = &[
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "coalesce",
    "nullif",
    "cast",
    "lower",
    "upper",
    "trim",
    "abs",
    "round",
    "row_number",
    "rank",
    "dense_rank",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    "percent_rank",
    "cume_dist",
];

/// PostgreSQL 12 and later (functions added after 12 are left out: on an
/// older server the name could be a user's function). Not `ts_stat`,
/// `ts_rewrite`, `query_to_xml`, `xpath`, `pg_read_file`, `pg_sleep`,
/// `nextval`, `set_config`, `txid_current`, advisory locks or any `pg_*`
/// that acts.
const POSTGRES: &[&str] = &[
    // Aggregates.
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "string_agg",
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "bool_and",
    "bool_or",
    "every",
    "bit_and",
    "bit_or",
    "percentile_cont",
    "percentile_disc",
    "mode",
    "corr",
    "covar_pop",
    "covar_samp",
    "regr_slope",
    "regr_intercept",
    "regr_count",
    "regr_r2",
    "regr_avgx",
    "regr_avgy",
    "regr_sxx",
    "regr_syy",
    "regr_sxy",
    "grouping",
    // Strings.
    "length",
    "char_length",
    "character_length",
    "octet_length",
    "bit_length",
    "substring",
    "substr",
    "ltrim",
    "rtrim",
    "btrim",
    "replace",
    "translate",
    "concat",
    "concat_ws",
    "left",
    "right",
    "position",
    "strpos",
    "split_part",
    "regexp_replace",
    "regexp_matches",
    "regexp_match",
    "regexp_split_to_array",
    "regexp_split_to_table",
    "lpad",
    "rpad",
    "initcap",
    "reverse",
    "repeat",
    "format",
    "md5",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "ascii",
    "chr",
    "starts_with",
    "overlay",
    "to_hex",
    "encode",
    "decode",
    "quote_ident",
    "quote_literal",
    "quote_nullable",
    "convert_from",
    "convert_to",
    "string_to_array",
    "array_to_string",
    // Math.
    "floor",
    "ceil",
    "ceiling",
    "trunc",
    "mod",
    "div",
    "power",
    "sqrt",
    "cbrt",
    "exp",
    "ln",
    "log",
    "log10",
    "greatest",
    "least",
    "sign",
    "pi",
    "degrees",
    "radians",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "cot",
    "random",
    "width_bucket",
    "factorial",
    // Dates and times.
    "now",
    "date_trunc",
    "date_part",
    "extract",
    "age",
    "to_char",
    "to_date",
    "to_timestamp",
    "to_number",
    "make_date",
    "make_time",
    "make_timestamp",
    "make_timestamptz",
    "make_interval",
    "clock_timestamp",
    "statement_timestamp",
    "transaction_timestamp",
    "timeofday",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "isfinite",
    // Type names with a modifier: keywords that can't name a function.
    "varchar",
    "char",
    "character",
    "numeric",
    "decimal",
    "float",
    "timestamp",
    "interval",
    "bit",
    // JSON.
    "json_extract_path",
    "json_extract_path_text",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "json_build_object",
    "json_build_array",
    "jsonb_build_object",
    "jsonb_build_array",
    "json_object",
    "json_array_length",
    "jsonb_array_length",
    "json_each",
    "json_each_text",
    "jsonb_each",
    "jsonb_each_text",
    "json_array_elements",
    "json_array_elements_text",
    "jsonb_array_elements",
    "jsonb_array_elements_text",
    "json_object_keys",
    "jsonb_object_keys",
    "json_typeof",
    "jsonb_typeof",
    "to_json",
    "to_jsonb",
    "row_to_json",
    "array_to_json",
    "jsonb_pretty",
    "json_strip_nulls",
    "jsonb_strip_nulls",
    "jsonb_set",
    "jsonb_insert",
    "jsonb_path_query",
    "jsonb_path_query_array",
    "jsonb_path_query_first",
    "jsonb_path_exists",
    "jsonb_path_match",
    "json_populate_record",
    "jsonb_populate_record",
    "json_to_record",
    "jsonb_to_record",
    "json_to_recordset",
    "jsonb_to_recordset",
    // Arrays and sets.
    "unnest",
    "array_length",
    "cardinality",
    "generate_series",
    "generate_subscripts",
    "array_position",
    "array_positions",
    "array_append",
    "array_prepend",
    "array_cat",
    "array_remove",
    "array_replace",
    "array_dims",
    "array_lower",
    "array_upper",
    "array_ndims",
    // Text search, without SQL text.
    "to_tsvector",
    "to_tsquery",
    "plainto_tsquery",
    "phraseto_tsquery",
    "websearch_to_tsquery",
    "ts_rank",
    "ts_rank_cd",
    "ts_headline",
    "setweight",
    "numnode",
    "querytree",
    // The catalog readers metadata queries use.
    "pg_typeof",
    "pg_get_viewdef",
    "pg_get_functiondef",
    "pg_get_function_arguments",
    "pg_get_function_result",
    "pg_get_function_identity_arguments",
    "pg_get_indexdef",
    "pg_get_constraintdef",
    "pg_get_triggerdef",
    "pg_get_ruledef",
    "pg_get_expr",
    "pg_get_userbyid",
    "pg_get_serial_sequence",
    "pg_get_keywords",
    "pg_table_size",
    "pg_relation_size",
    "pg_total_relation_size",
    "pg_indexes_size",
    "pg_database_size",
    "pg_column_size",
    "pg_size_pretty",
    "pg_size_bytes",
    "format_type",
    "obj_description",
    "col_description",
    "shobj_description",
    "current_schema",
    "current_schemas",
    "current_database",
    "current_setting",
    "version",
    "has_table_privilege",
    "has_column_privilege",
    "has_schema_privilege",
    "has_database_privilege",
    "has_function_privilege",
    "has_sequence_privilege",
    "has_any_column_privilege",
    "pg_has_role",
    "pg_table_is_visible",
    "pg_type_is_visible",
    "pg_function_is_visible",
    "to_regclass",
    "to_regtype",
    "to_regproc",
    "to_regprocedure",
    "to_regnamespace",
    "to_regrole",
    "pg_backend_pid",
    "pg_is_in_recovery",
    "pg_postmaster_start_time",
    "pg_conf_load_time",
    "pg_encoding_to_char",
    "inet_server_addr",
    "inet_server_port",
];

/// MySQL 8 and MariaDB. Not `sleep`, `benchmark`, `get_lock`,
/// `release_lock`, `load_file`, `last_insert_id` (it sets with an argument)
/// or the replication waits.
const MYSQL: &[&str] = &[
    // Aggregates.
    "std",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "group_concat",
    "json_arrayagg",
    "json_objectagg",
    "bit_and",
    "bit_or",
    "bit_xor",
    "any_value",
    // Strings.
    "lcase",
    "ucase",
    "length",
    "char_length",
    "character_length",
    "octet_length",
    "bit_length",
    "substring",
    "substr",
    "mid",
    "ltrim",
    "rtrim",
    "replace",
    "concat",
    "concat_ws",
    "left",
    "right",
    "position",
    "instr",
    "locate",
    "lpad",
    "rpad",
    "reverse",
    "repeat",
    "format",
    "md5",
    "sha1",
    "sha2",
    "ascii",
    "char",
    "hex",
    "unhex",
    "space",
    "strcmp",
    "soundex",
    "field",
    "elt",
    "find_in_set",
    "ord",
    "quote",
    "make_set",
    "regexp_like",
    "regexp_replace",
    "regexp_substr",
    "regexp_instr",
    // Math.
    "floor",
    "ceil",
    "ceiling",
    "mod",
    "power",
    "pow",
    "sqrt",
    "exp",
    "ln",
    "log",
    "log10",
    "log2",
    "greatest",
    "least",
    "sign",
    "pi",
    "degrees",
    "radians",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "cot",
    "rand",
    "crc32",
    "conv",
    "bin",
    "oct",
    "bit_count",
    // Dates and times.
    "now",
    "curdate",
    "curtime",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "sysdate",
    "utc_date",
    "utc_time",
    "utc_timestamp",
    "date_add",
    "date_sub",
    "adddate",
    "subdate",
    "datediff",
    "timestampdiff",
    "timestampadd",
    "date_format",
    "str_to_date",
    "from_unixtime",
    "unix_timestamp",
    "extract",
    "year",
    "month",
    "day",
    "hour",
    "minute",
    "second",
    "quarter",
    "week",
    "weekday",
    "dayofweek",
    "dayofmonth",
    "dayofyear",
    "weekofyear",
    "yearweek",
    "dayname",
    "monthname",
    "last_day",
    "makedate",
    "maketime",
    "microsecond",
    "period_add",
    "period_diff",
    "time_format",
    "time_to_sec",
    "sec_to_time",
    "to_days",
    "from_days",
    "date",
    "time",
    "timestamp",
    "convert_tz",
    // Conversion and conditionals.
    "convert",
    "ifnull",
    "isnull",
    "if",
    "interval",
    "decimal",
    "binary",
    // JSON.
    "json_extract",
    "json_unquote",
    "json_object",
    "json_array",
    "json_contains",
    "json_contains_path",
    "json_search",
    "json_keys",
    "json_length",
    "json_type",
    "json_valid",
    "json_depth",
    "json_quote",
    "json_set",
    "json_insert",
    "json_replace",
    "json_remove",
    "json_merge_patch",
    "json_merge_preserve",
    "json_array_append",
    "json_array_insert",
    "json_overlaps",
    "json_pretty",
    "json_storage_size",
    "json_table",
    "json_value",
    // About the session and the server.
    "database",
    "schema",
    "version",
    "user",
    "current_user",
    "session_user",
    "system_user",
    "connection_id",
    "found_rows",
    "row_count",
    "uuid",
    "inet_aton",
    "inet_ntoa",
    "inet6_aton",
    "inet6_ntoa",
    "is_ipv4",
    "is_ipv6",
    "charset",
    "collation",
    "coercibility",
    "weight_string",
];

/// Oracle. Packages (`dbms_*`, `utl_*`) are qualified names: they ask.
const ORACLE: &[&str] = &[
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "listagg",
    "median",
    "corr",
    "covar_pop",
    "covar_samp",
    "percentile_cont",
    "percentile_disc",
    "length",
    "lengthb",
    "substr",
    "ltrim",
    "rtrim",
    "replace",
    "translate",
    "concat",
    "instr",
    "lpad",
    "rpad",
    "initcap",
    "ascii",
    "chr",
    "regexp_replace",
    "regexp_substr",
    "regexp_instr",
    "regexp_like",
    "regexp_count",
    "soundex",
    "floor",
    "ceil",
    "trunc",
    "mod",
    "power",
    "sqrt",
    "exp",
    "ln",
    "log",
    "greatest",
    "least",
    "sign",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "bitand",
    "width_bucket",
    "nanvl",
    "remainder",
    "sysdate",
    "systimestamp",
    "add_months",
    "months_between",
    "last_day",
    "next_day",
    "to_char",
    "to_date",
    "to_number",
    "to_timestamp",
    "extract",
    "numtodsinterval",
    "numtoyminterval",
    "nvl",
    "nvl2",
    "decode",
    "lnnvl",
    "sys_context",
    "sys_guid",
    "varchar2",
    "nvarchar2",
    "number",
];

/// SQLite and libSQL. Not `load_extension`, `randomblob`, `zeroblob`,
/// `fts3_tokenizer`, `readfile`, `writefile` or `edit`.
const SQLITE: &[&str] = &[
    "typeof",
    "printf",
    "total",
    "iif",
    "likely",
    "unlikely",
    "likelihood",
    "changes",
    "total_changes",
    "last_insert_rowid",
    "sqlite_version",
    "sqlite_source_id",
    "glob",
    "like",
    "julianday",
    "strftime",
    "unixepoch",
    "timediff",
    "datetime",
    "unicode",
    "json",
    "jsonb",
    "json_array_length",
    "json_each",
    "json_tree",
    "json_group_array",
    "json_group_object",
    "json_patch",
    "pragma_table_info",
    "pragma_table_xinfo",
    "pragma_index_list",
    "pragma_index_info",
    "pragma_index_xinfo",
    "pragma_foreign_key_list",
    "pragma_table_list",
    "pragma_database_list",
];

/// SQL Server and Sybase. Not `openrowset`, `openquery`, `opendatasource`
/// or `openxml`.
const TSQL: &[&str] = &[
    "getdate",
    "getutcdate",
    "sysdatetime",
    "sysutcdatetime",
    "sysdatetimeoffset",
    "len",
    "datalength",
    "charindex",
    "patindex",
    "datepart",
    "datename",
    "dateadd",
    "datediff_big",
    "datefromparts",
    "eomonth",
    "choose",
    "try_cast",
    "try_convert",
    "try_parse",
    "quotename",
    "stuff",
    "replicate",
    "str",
    "string_split",
    "string_escape",
    "object_id",
    "object_name",
    "object_schema_name",
    "schema_name",
    "schema_id",
    "db_name",
    "db_id",
    "user_name",
    "suser_sname",
    "suser_name",
    "col_name",
    "col_length",
    "type_name",
    "newid",
    "openjson",
    "isjson",
    "json_modify",
    "json_query",
    "count_big",
    "checksum",
    "binary_checksum",
    "hashbytes",
    "serverproperty",
    "databasepropertyex",
    "objectproperty",
    "columnproperty",
    "nvarchar",
    "nchar",
    "varbinary",
    "datetime2",
    "datetimeoffset",
    "smalldatetime",
];

/// The vector (embedding) functions of the engine `driver` connects to, so
/// a similarity search (`ORDER BY cosine_distance(v, '[…]') LIMIT 5`) runs
/// without approval where the server enforces the read. By driver, not by
/// family: each name is one engine's, and on another server of the family
/// (MySQL next to TiDB, PostgreSQL next to Redshift) it would be a user's
/// function. The variants listed are those whose driver family stores
/// vectors (`Capabilities.vectors`), plus engines whose built-ins work on
/// arrays without a vector type (DuckDB files, Databricks). An extension's
/// names (pgvector's, [`vector_extension`]) skip the approval only after
/// the server says they're the extension's. Operators (pgvector's `<->`, `<#>`, `<=>`, `<+>`, `<~>`,
/// `<%>`) aren't calls: the check never stops at them. Never a function
/// that calls a model or another service (`vector_embedding`,
/// `ai_generate_embeddings`, `aiSimilarity`, `ai_similarity`, Databricks'
/// `vector_search`). Engines whose language isn't SQL (Cassandra's
/// `similarity_*`, Redis' `VSIM`, MongoDB's `$vectorSearch`) aren't checked
/// here: their reads are always approved. Neo4j's Cypher has its own list
/// (`reads::cypher`), with its vector functions.
fn vector_functions(driver: &str) -> &'static [&'static str] {
    match driver {
        d if vector_extension(d).is_some() => PGVECTOR,
        "cockroachdb" => COCKROACH_VECTOR,
        // RisingWave's own vector(n) (2.6; subvector 2.7), pgvector's names.
        "risingwave" => &[
            "l2_distance",
            "cosine_distance",
            "l1_distance",
            "inner_product",
            "vector_norm",
            "l2_normalize",
            "subvector",
        ],
        "cratedb" => &["knn_match", "vector_similarity"],
        // The MySQL driver connects to MariaDB too (its reads are enforced
        // there as well). Not Aurora MySQL or Cloud SQL for MySQL: the
        // family has no vectors there.
        "mysql" => &[
            "string_to_vector",
            "to_vector",
            "vector_to_string",
            "from_vector",
            "vector_dim",
            "vec_fromtext",
            "vec_totext",
            "vec_distance",
            "vec_distance_euclidean",
            "vec_distance_cosine",
        ],
        "mariadb" => MARIADB_VECTOR,
        "tidb" => &[
            "vec_from_text",
            "vec_as_text",
            "vec_dims",
            "vec_l1_distance",
            "vec_l2_distance",
            "vec_cosine_distance",
            "vec_negative_inner_product",
            "vec_l2_norm",
        ],
        "singlestore" => &[
            "dot_product",
            "euclidean_distance",
            "vector_num_elements",
            "vector_elements_sum",
            "vector_kth_element",
            "vector_subvector",
            "vector_sort",
            "vector_add",
            "vector_sub",
            "vector_mul",
            "scalar_vector_mul",
            "vector_sum",
            "json_array_pack",
            "json_array_unpack",
        ],
        // Databend's VECTOR(n).
        "databend" => &[
            "cosine_distance",
            "l1_distance",
            "l2_distance",
            "inner_product",
            "vector_dims",
            "vector_norm",
        ],
        "oceanbase" => &[
            "l2_distance",
            "cosine_distance",
            "inner_product",
            "negative_inner_product",
            "vector_distance",
            "vector_dims",
            "vector_norm",
        ],
        "doris" | "velodb" => &[
            "l1_distance",
            "l2_distance",
            "cosine_distance",
            "cosine_similarity",
            "inner_product",
            "l2_distance_approximate",
            "inner_product_approximate",
        ],
        "starrocks" => &[
            "approx_cosine_similarity",
            "approx_l2_distance",
            "cosine_similarity",
            "cosine_similarity_norm",
            "l2_distance",
        ],
        "greptimedb" => &[
            "parse_vec",
            "vec_to_string",
            "vec_dim",
            "vec_norm",
            "vec_add",
            "vec_sub",
            "vec_mul",
            "vec_div",
            "vec_scalar_add",
            "vec_scalar_mul",
            "vec_elem_sum",
            "vec_elem_product",
            "vec_elem_avg",
            "vec_kth_elem",
            "vec_subvector",
            "vec_cos_distance",
            "vec_l2sq_distance",
            "vec_dot_product",
            "vec_sum",
            "vec_product",
        ],
        "manticore" => &["knn_dist"],
        // Oracle 23ai. Not `vector_embedding` (it runs a model) or
        // `vector_chunks`.
        "oracle" | "oracle_adb" => &[
            "vector_distance",
            "cosine_distance",
            "inner_product",
            "l1_distance",
            "l2_distance",
            "hamming_distance",
            "jaccard_distance",
            "to_vector",
            "vector",
            "from_vector",
            "vector_serialize",
            "vector_dimension_count",
            "vector_dims",
            "vector_dimension_format",
            "vector_norm",
        ],
        // SQL Server 2025 and Azure SQL. Not `ai_generate_embeddings`.
        "sqlserver" | "azuresql" => &[
            "vector_distance",
            "vector_norm",
            "vector_normalize",
            "vectorproperty",
            "vector_search",
        ],
        // libSQL's own (not `libsql_vector_idx`: it only builds an index).
        "libsql" => &[
            "vector",
            "vector32",
            "vector64",
            "vector16",
            "vectorb16",
            "vector8",
            "vector1bit",
            "vector_extract",
            "vector_distance_cos",
            "vector_distance_l2",
            "vector_top_k",
        ],
        // Lower case: the check compares names that way.
        "clickhouse" | "timeplus" => &[
            "cosinedistance",
            "l1distance",
            "l2distance",
            "l2squareddistance",
            "linfdistance",
            "lpdistance",
            "dotproduct",
            "l1norm",
            "l2norm",
            "l2squarednorm",
            "linfnorm",
            "lpnorm",
            "l1normalize",
            "l2normalize",
            "linfnormalize",
            "lpnormalize",
            "distancecosine",
            "distancel1",
            "distancel2",
            "distancel2squared",
            "distancelinf",
            "distancelp",
        ],
        "duckdb" | "duckdb_files" => &[
            "array_value",
            "array_cosine_similarity",
            "array_cosine_distance",
            "array_distance",
            "array_inner_product",
            "array_negative_inner_product",
            "array_dot_product",
            "array_negative_dot_product",
            "list_cosine_similarity",
            "list_cosine_distance",
            "list_distance",
            "list_inner_product",
            "list_negative_inner_product",
            "list_dot_product",
            "list_negative_dot_product",
        ],
        // `ML.DISTANCE` is a qualified name: it asks.
        "bigquery" => &["cosine_distance", "euclidean_distance", "vector_search"],
        "spanner" => &[
            "cosine_distance",
            "euclidean_distance",
            "dot_product",
            "approx_cosine_distance",
            "approx_euclidean_distance",
            "approx_dot_product",
        ],
        "snowflake" => &[
            "vector_cosine_similarity",
            "vector_inner_product",
            "vector_l1_distance",
            "vector_l2_distance",
            "vector_normalize",
            "vector_truncate",
            "vector_avg",
            "vector_sum",
            "vector_min",
            "vector_max",
        ],
        // Not `vector_search`: it queries a separate service.
        "databricks" | "azure_databricks" => &[
            "vector_cosine_similarity",
            "vector_inner_product",
            "vector_l2_distance",
            "vector_norm",
            "vector_normalize",
            "vector_avg",
            "vector_sum",
        ],
        "couchbase" => &[
            "vector_distance",
            "approx_vector_distance",
            "isvector",
            "decode_vector",
            "encode_vector",
            "normalize_vector",
        ],
        "cosmosdb" => &["vectordistance"],
        // Not `vector_embedding` (it runs a model).
        "hana" => &[
            "cosine_similarity",
            "l2distance",
            "to_real_vector",
            "cardinality",
        ],
        _ => &[],
    }
}

/// The extension `driver`'s vector functions belong to, where they aren't
/// the engine's own built-ins: pgvector (`vector`) on the PostgreSQL
/// variants the family gives it to. A user's function of the same name
/// (in any schema, through an overload or a schema earlier in the path)
/// would run instead, so a query that calls one skips the approval only
/// after the server says every function of that name is the extension's
/// or a built-in ([`extension_check_sql`]).
pub(crate) fn vector_extension(driver: &str) -> Option<&'static str> {
    matches!(
        driver,
        "postgres"
            | "timescaledb"
            | "alloydb"
            | "cloudsql_postgres"
            | "aurora_postgres"
            | "yugabytedb"
            | "greenplum"
            | "cloudberry"
            | "greengage"
            | "edb"
            | "fujitsu"
            | "kingbase"
            | "opengauss"
    )
    .then_some("vector")
}

/// The extension functions (lower case) `query` calls on `driver`
/// ([`vector_extension`]); none where its vector functions are built-ins.
pub(crate) fn extension_calls(
    driver: &str,
    dialect: &ScriptDialect,
    query: &str,
) -> Vec<&'static str> {
    if vector_extension(driver).is_none() {
        return Vec::new();
    }
    let text = expose_versioned(query, dialect);
    let toks = name_tokens(&text, dialect);
    let mut out: Vec<&'static str> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        let call = t.kind == TokenKind::Name
            && toks
                .get(i + 1)
                .is_some_and(|n| n.kind == TokenKind::Punct && n.text == "(");
        if !call {
            continue;
        }
        let lower = t.text.to_ascii_lowercase();
        if let Some(f) = PGVECTOR.iter().find(|f| **f == lower) {
            if !out.contains(f) {
                out.push(f);
            }
        }
    }
    out
}

/// A read that counts the functions named `names` (lower case, from
/// [`PGVECTOR`]) that are neither `extension`'s nor built-ins (in
/// `pg_catalog`), in any schema: 0 means each call is the extension's.
/// PostgreSQL 9.4 catalogs (Greenplum 6) answer it too.
pub(crate) fn extension_check_sql(extension: &str, names: &[&str]) -> String {
    let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let list: Vec<String> = names.iter().map(|n| lit(n)).collect();
    format!(
        "SELECT count(*) FROM pg_catalog.pg_proc p \
         WHERE p.proname IN ({}) \
         AND p.pronamespace <> (SELECT n.oid FROM pg_catalog.pg_namespace n WHERE n.nspname = 'pg_catalog') \
         AND NOT EXISTS (SELECT 1 FROM pg_catalog.pg_depend d \
         JOIN pg_catalog.pg_extension e ON e.oid = d.refobjid \
         WHERE d.classid = 'pg_catalog.pg_proc'::pg_catalog.regclass \
         AND d.refclassid = 'pg_catalog.pg_extension'::pg_catalog.regclass \
         AND d.objid = p.oid AND d.deptype = 'e' AND e.extname = {})",
        list.join(", "),
        lit(extension)
    )
}

/// pgvector 0.8 (an extension, not a built-in: listed for the variants that
/// ship it). Not the index builders' support functions.
const PGVECTOR: &[&str] = &[
    "l2_distance",
    "cosine_distance",
    "inner_product",
    "l1_distance",
    "vector_dims",
    "vector_norm",
    "l2_norm",
    "l2_normalize",
    "binary_quantize",
    "subvector",
    "hamming_distance",
    "jaccard_distance",
];

/// CockroachDB's native vectors (25.2), pgvector's names.
const COCKROACH_VECTOR: &[&str] = &[
    "l2_distance",
    "cosine_distance",
    "inner_product",
    "l1_distance",
    "vector_dims",
    "vector_norm",
];

/// MariaDB 11.7 and later.
const MARIADB_VECTOR: &[&str] = &[
    "vec_fromtext",
    "vec_totext",
    "vec_distance",
    "vec_distance_euclidean",
    "vec_distance_cosine",
];

/// Words a `(` follows without being a call (`IN (`, `EXISTS (`, `OVER (`,
/// `AS (`…). Only unquoted: `"values"(…)` is a call in PostgreSQL.
const NOT_CALLS: &[&str] = &[
    "in",
    "exists",
    "not",
    "and",
    "or",
    "any",
    "all",
    "some",
    "as",
    "from",
    "join",
    "on",
    "using",
    "where",
    "having",
    "select",
    "values",
    "over",
    "filter",
    "group",
    "by",
    "when",
    "then",
    "else",
    "case",
    "distinct",
    "union",
    "intersect",
    "except",
    "minus",
    "between",
    "is",
    "like",
    "ilike",
    "similar",
    "with",
    "recursive",
    "partition",
    "window",
    "rows",
    "range",
    "groups",
    "lateral",
    "array",
    "row",
    "limit",
    "offset",
    "top",
    "fetch",
    "table",
    "pivot",
    "unpivot",
    "for",
    "keep",
    "rollup",
    "cube",
    "sets",
    "apply",
    "only",
    "escape",
    "materialized",
    "qualify",
    "repeatable",
    "nulls",
    "explain",
    "index",
    "key",
    "option",
    "returns",
    "return",
    "of",
    "to",
    "into",
];

/// SQLite pragmas that read and take an argument (`PRAGMA table_info(t)`).
const READ_PRAGMAS: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

/// Whether `query` may run on the approval-free path: `Ok` when every
/// function it calls is a listed built-in of the engine's family; `Err`
/// names the first one that isn't (or why the statement can't be checked).
/// DAX: `Ok` when the query is one unit, as the driver's own splitter cut
/// it ([`dbine_driver::Driver::split_script`]: the same rules its
/// `Session::run_read_only` checks again), that is a DAX query
/// ([`dbine_driver::read_only::dax_is_query`]) with a single `EVALUATE`
/// (the splitter marks a `DEFINE` followed by several as
/// [`StatementKind::Block`]). DAX has no function with a side effect, and
/// the query APIs run nothing but DAX queries: the service enforces the
/// read. Anything else goes to the user's approval.
pub(crate) fn dax_approval_free(units: &[ScriptStatement]) -> Result<(), String> {
    let [unit] = units else {
        return Err(format!(
            "{} DAX units: only a single query runs without approval",
            units.len()
        ));
    };
    if let Some(e) = &unit.error {
        return Err(e.clone());
    }
    if unit.kind != StatementKind::Sql {
        return Err("more than one EVALUATE".into());
    }
    if !dbine_driver::read_only::dax_is_query(&unit.text) {
        return Err("not a DAX query (DEFINE / EVALUATE)".into());
    }
    Ok(())
}

/// `driver` is the driver's id (its vector functions, [`vector_functions`]);
/// `hint` its dialect hint (the family's built-ins).
pub(crate) fn approval_free(
    language: Language,
    driver: &str,
    hint: &str,
    dialect: &ScriptDialect,
    query: &str,
) -> Result<(), String> {
    if language == Language::Cypher {
        return super::cypher::approval_free(driver, query);
    }
    if language != Language::Sql {
        return Err(format!("{language:?} statements aren't checked"));
    }
    let family = Family::of(hint);
    let vectors = vector_functions(driver);
    // MySQL's `/*! … */` runs on the server: read it as code.
    let text = expose_versioned(query, dialect);
    let toks = name_tokens(&text, dialect);
    let bytes = text.as_bytes();
    let quoted = |t: &NameToken<'_>| matches!(bytes[t.start], b'"' | b'`' | b'[');
    let bare = |i: usize| {
        toks.get(i)
            .is_some_and(|t| t.kind == TokenKind::Name && !quoted(t))
    };
    let word = |i: usize, w: &str| bare(i) && toks[i].text.eq_ignore_ascii_case(w);
    let punct = |i: usize, p: &str| {
        toks.get(i)
            .is_some_and(|t| t.kind == TokenKind::Punct && t.text == p)
    };
    let listed = |name: &str| {
        let name = name.to_ascii_lowercase();
        family.lists(&name) || vectors.contains(&name.as_str())
    };
    let pragma_name = if word(0, "pragma") {
        if punct(2, ".") {
            Some(3)
        } else {
            Some(1)
        }
    } else {
        None
    };
    for i in 0..toks.len() {
        if toks[i].kind != TokenKind::Name || !punct(i + 1, "(") {
            continue;
        }
        let name = toks[i].text;
        if i >= 1 && punct(i - 1, ".") {
            // `schema.f(…)`: only a built-in under the system schema (an
            // extension's vector functions aren't there).
            let system = family.system_schema().is_some_and(|s| word(i - 2, s))
                && !(i >= 3 && punct(i - 3, "."));
            if system && bare(i) && family.lists(&name.to_ascii_lowercase()) {
                continue;
            }
            return Err(qualified(&toks, i));
        }
        if !bare(i) {
            return Err(format!("\"{name}\""));
        }
        let lower = name.to_ascii_lowercase();
        if NOT_CALLS.contains(&lower.as_str()) {
            continue;
        }
        if pragma_name == Some(i) && READ_PRAGMAS.contains(&lower.as_str()) {
            continue;
        }
        // A type with its modifier or an alias with its column list:
        // `CAST(x AS varchar(10))`, `AS g(n)`, PostgreSQL's `x::numeric(10,2)`,
        // `generate_series(1, 3) g(n)`, `WITH t(a, b) AS (SELECT …)`.
        if i >= 1 && word(i - 1, "as") {
            continue;
        }
        if family == Family::Postgres
            && i >= 2
            && punct(i - 1, ":")
            && punct(i - 2, ":")
            && toks[i - 2].end == toks[i - 1].start
        {
            continue;
        }
        if i >= 1 && punct(i - 1, ")") && column_list(&toks, i + 1, &bare).is_some() {
            continue;
        }
        if cte_columns(&toks, i, &bare, &word, &punct) {
            continue;
        }
        // Glued to what precedes it (`x$lower(`, `#lower(`, `'a'lower(`),
        // the server may read another name.
        if i >= 1
            && toks[i - 1].end == toks[i].start
            && !matches!(
                toks[i - 1].text,
                "(" | ","
                    | "="
                    | "<"
                    | ">"
                    | "+"
                    | "-"
                    | "*"
                    | "/"
                    | "%"
                    | "|"
                    | "!"
                    | ";"
                    | "["
                    | "{"
                    | "^"
                    | "~"
                    | "&"
            )
        {
            return Err(name.to_string());
        }
        if listed(name) {
            continue;
        }
        return Err(name.to_string());
    }
    Ok(())
}

/// `a.b.f` as written, for the reason.
fn qualified(toks: &[NameToken<'_>], i: usize) -> String {
    let mut from = i;
    while from >= 2 && toks[from - 1].text == "." && toks[from - 2].kind == TokenKind::Name {
        from -= 2;
    }
    toks[from..=i].iter().map(|t| t.text).collect()
}

/// At `open` a `(` holding only bare names and commas: where its `)` is.
fn column_list(toks: &[NameToken<'_>], open: usize, bare: &dyn Fn(usize) -> bool) -> Option<usize> {
    let mut j = open + 1;
    let mut want_name = true;
    while let Some(t) = toks.get(j) {
        match (want_name, t.kind, t.text) {
            (true, TokenKind::Name, _) if bare(j) => want_name = false,
            (false, TokenKind::Punct, ",") => want_name = true,
            (false, TokenKind::Punct, ")") => return Some(j),
            _ => return None,
        }
        j += 1;
    }
    None
}

/// `WITH t(a, b) AS [NOT] [MATERIALIZED] (SELECT|WITH …)`: a CTE's column
/// list, after `WITH`, `RECURSIVE` or a comma.
fn cte_columns(
    toks: &[NameToken<'_>],
    i: usize,
    bare: &dyn Fn(usize) -> bool,
    word: &dyn Fn(usize, &str) -> bool,
    punct: &dyn Fn(usize, &str) -> bool,
) -> bool {
    if i == 0 || !(word(i - 1, "with") || word(i - 1, "recursive") || punct(i - 1, ",")) {
        return false;
    }
    let Some(close) = column_list(toks, i + 1, bare) else {
        return false;
    };
    let mut j = close + 1;
    if !word(j, "as") {
        return false;
    }
    j += 1;
    if word(j, "not") {
        j += 1;
    }
    if word(j, "materialized") {
        j += 1;
    }
    punct(j, "(") && (word(j + 1, "select") || word(j + 1, "with"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pg(q: &str) -> Result<(), String> {
        approval_free(
            Language::Sql,
            "postgres",
            "postgres",
            &ScriptDialect::postgres(),
            q,
        )
    }
    fn my(q: &str) -> Result<(), String> {
        approval_free(Language::Sql, "mysql", "mysql", &ScriptDialect::mysql(), q)
    }
    fn lite(q: &str) -> Result<(), String> {
        approval_free(
            Language::Sql,
            "sqlite",
            "sqlite",
            &ScriptDialect::generic(),
            q,
        )
    }

    #[test]
    fn mcp_allowlist_ordinary_reads_need_no_approval() {
        for q in [
            "SELECT 1",
            "SELECT * FROM orders WHERE id IN (1, 2) AND EXISTS (SELECT 1 FROM t)",
            "SELECT customer, count(*), sum(total), avg(total), max(at) FROM orders GROUP BY customer HAVING count(*) > 1",
            "SELECT row_number() OVER (PARTITION BY a ORDER BY b), lag(x) OVER w FROM t WINDOW w AS (ORDER BY b)",
            "SELECT lower(name), upper(trim(code)), substring(s FROM 1 FOR 3), coalesce(a, b), date_trunc('day', at) FROM t",
            "SELECT CAST(x AS varchar(10)), x::numeric(10,2), extract(year FROM at), to_char(now(), 'YYYY') FROM t",
            "SELECT n FROM generate_series(1, 3) AS g(n)",
            "SELECT n FROM generate_series(1, 3) g(n)",
            "WITH t(a, b) AS (SELECT 1, 2) SELECT * FROM t",
            "WITH RECURSIVE r(n) AS (SELECT 1 UNION ALL SELECT n + 1 FROM r WHERE n < 3) SELECT * FROM r",
            "SELECT pg_catalog.pg_get_viewdef('v'::regclass), pg_size_pretty(pg_total_relation_size('t'))",
            "SELECT percentile_cont(0.5) WITHIN GROUP (ORDER BY x) FROM t",
            "SELECT jsonb_build_object('a', 1), j->>'k', jsonb_array_length(j) FROM t",
            "SELECT 'pg_sleep(10)' AS s",
        ] {
            assert_eq!(pg(q), Ok(()), "{q}");
        }
        assert_eq!(my("SELECT database(), version(), json_extract(j, '$.a'), ifnull(a, 0), date_format(d, '%Y') FROM t"), Ok(()));
        assert_eq!(
            lite(
                "SELECT typeof(x), ifnull(a, 0), strftime('%Y', d), json_extract(j, '$.a') FROM t"
            ),
            Ok(())
        );
        assert_eq!(lite("PRAGMA table_info(t)"), Ok(()));
    }

    #[test]
    fn mcp_allowlist_side_effects_and_unknown_functions_ask() {
        for (q, why) in [
            ("SELECT pg_wal_replay_pause()", "pg_wal_replay_pause"),
            ("SELECT pg_wal_replay_resume()", "pg_wal_replay_resume"),
            ("SELECT pg_backup_start('x')", "pg_backup_start"),
            ("SELECT pg_stat_reset_slru('x')", "pg_stat_reset_slru"),
            ("SELECT pg_advisory_lock(1)", "pg_advisory_lock"),
            ("SELECT pg_terminate_backend(123)", "pg_terminate_backend"),
            ("SELECT pg_sleep(10)", "pg_sleep"),
            ("SELECT nextval('s')", "nextval"),
            ("SELECT pg_read_file('/etc/passwd')", "pg_read_file"),
            // SQL hidden in a string, run by the function.
            (
                "SELECT * FROM ts_stat('SELECT pg_wal_replay_pause()::text::tsvector')",
                "ts_stat",
            ),
            ("SELECT ts_rewrite('a'::tsquery, 'SELECT 1')", "ts_rewrite"),
            (
                "SELECT query_to_xml('SELECT pg_promote()', true, true, '')",
                "query_to_xml",
            ),
            ("SELECT * FROM crosstab('SELECT 1')", "crosstab"),
            ("SELECT * FROM dblink('c', 'SELECT 1') AS (a int)", "dblink"),
            // A user-defined function (its body may use plpython3u, dblink…).
            ("SELECT my_report(1)", "my_report"),
            // Qualified and quoted names.
            (
                "SELECT pg_catalog.pg_wal_replay_pause()",
                "pg_catalog.pg_wal_replay_pause",
            ),
            ("SELECT public.lower('x')", "public.lower"),
            ("SELECT \"lower\"('x')", "\"lower\""),
            ("SELECT \"values\"(1)", "\"values\""),
            ("SELECT U&\"low\\0065r\"('x')", "\"low\\0065r\""),
            // Nested, in a comment-split call, after a cast.
            (
                "SELECT * FROM (SELECT a FROM t WHERE b = (SELECT pg_promote())) s",
                "pg_promote",
            ),
            ("SELECT pg_sleep/* x */(1)", "pg_sleep"),
            (
                "SELECT 1 FROM t WHERE x IN (SELECT pg_cancel_backend(pid) FROM pg_stat_activity)",
                "pg_cancel_backend",
            ),
            ("SELECT arr[1:pg_sleep(1)] FROM t", "pg_sleep"),
        ] {
            assert_eq!(pg(q), Err(why.to_string()), "{q}");
        }
        assert_eq!(my("SELECT sleep(10)"), Err("sleep".into()));
        assert_eq!(
            my("SELECT benchmark(1000000, md5('x'))"),
            Err("benchmark".into())
        );
        assert_eq!(
            my("SELECT /*!50000 get_lock('x', 10) */"),
            Err("get_lock".into())
        );
        assert_eq!(my("SELECT `lower`('x')"), Err("\"lower\"".into()));
        assert_eq!(my("SELECT x$lower('x')"), Err("x$lower".into()));
        assert_eq!(
            lite("SELECT load_extension('x')"),
            Err("load_extension".into())
        );
        assert_eq!(
            lite("PRAGMA wal_checkpoint(TRUNCATE)"),
            Err("wal_checkpoint".into())
        );
        // A built-in elsewhere but not a system schema outside PostgreSQL.
        assert_eq!(
            approval_free(
                Language::Sql,
                "sqlserver",
                "mssql",
                &ScriptDialect::tsql(),
                "SELECT dbo.f(1)"
            ),
            Err("dbo.f".into())
        );
        // Another engine's built-in is a user's function in PostgreSQL and
        // MySQL; SQLite and SQL Server have no unqualified user functions.
        assert_eq!(pg("SELECT ifnull(a, 0) FROM t"), Err("ifnull".into()));
        assert_eq!(my("SELECT strpos(a, 'x') FROM t"), Err("strpos".into()));
        assert_eq!(lite("SELECT strpos(a, 'x') FROM t"), Ok(()));
        // Cypher has its own list (`reads::cypher`): Neo4j's only.
        let cypher = |driver: &str, q: &str| {
            approval_free(
                Language::Cypher,
                driver,
                driver,
                &ScriptDialect::generic(),
                q,
            )
        };
        assert_eq!(cypher("neo4j", "RETURN toUpper('a')"), Ok(()));
        assert_eq!(
            cypher("neo4j", "RETURN apoc.text.join(['a'], ',')"),
            Err("apoc.text.join".into())
        );
        assert!(cypher("memgraph", "RETURN 1").is_err());
        // DAX goes through the driver's own units, never this lexer.
        assert!(approval_free(
            Language::Dax,
            "powerbi",
            "powerbi",
            &ScriptDialect::generic(),
            "EVALUATE {1}"
        )
        .is_err());
    }

    #[test]
    fn a_single_dax_query_runs_without_approval() {
        let unit = |text: &str, kind: StatementKind| ScriptStatement {
            text: text.into(),
            start: 0,
            end: text.len(),
            line: 1,
            kind,
            repeat: 1,
            error: None,
        };
        let q = |t: &str| unit(t, StatementKind::Sql);
        assert_eq!(dax_approval_free(&[q("EVALUATE 'Ventas'")]), Ok(()));
        assert_eq!(
            dax_approval_free(&[q("DEFINE MEASURE 'T'[m] = 1\nEVALUATE {[m]}")]),
            Ok(())
        );
        // Two units, several EVALUATEs in one, an empty script.
        assert!(dax_approval_free(&[q("EVALUATE {1}"), q("EVALUATE {2}")]).is_err());
        assert!(dax_approval_free(&[unit(
            "DEFINE VAR x = 1 EVALUATE {x} EVALUATE {x}",
            StatementKind::Block
        )])
        .is_err());
        assert!(dax_approval_free(&[]).is_err());
        // The driver's own commands, or anything that isn't a query.
        assert!(dax_approval_free(&[q("GRANT Read TO \"ana@contoso.com\"")]).is_err());
        assert!(dax_approval_free(&[q("{1}")]).is_err());
        // A unit the splitter flagged.
        let mut bad = q("DEFINE VAR x = 1");
        bad.error = Some("Falta EVALUATE después de DEFINE".into());
        assert!(dax_approval_free(&[bad]).is_err());
    }

    /// The check above trusts the units the Power BI driver cuts (the
    /// `Block` mark above all): the same inputs through its real splitter,
    /// as `mcp/tools.rs::approval_free` passes them.
    #[test]
    #[ignore = "needs part B's DAX splitter (crates/drivers/powerbi/src/dax/split.rs): the phase 1 fallback never marks a unit Block and doesn't cut at EVALUATE"]
    fn dax_approval_free_through_the_power_bi_splitter() {
        let d = dbine_drivers::find("powerbi").expect("the Power BI driver");
        let check = |q: &str| dax_approval_free(&d.split_script(q));
        assert_eq!(check("// n\nEVALUATE 'T'"), Ok(()));
        assert_eq!(
            check("DEFINE MEASURE 'T'[m] = 1\nEVALUATE {[m]}\nORDER BY [m]"),
            Ok(())
        );
        // Two EVALUATEs, without `;`: two units, or one unit with several.
        assert!(check("EVALUATE {1}\nEVALUATE {2}").is_err());
        assert!(check("EVALUATE {1} EVALUATE {2}").is_err());
        assert!(check("DEFINE VAR x = 1 EVALUATE {x} EVALUATE {x}").is_err());
        assert!(check("DEFINE VAR x = 1\nEVALUATE {x}\n// b\nEVALUATE {x}").is_err());
        // One of the driver's commands; a DEFINE alone.
        assert!(check("GRANT Read TO \"ana@contoso.com\"").is_err());
        assert!(check("DEFINE VAR x = 1").is_err());
        // EVALUATE inside a string, a name, a ref or a comment doesn't count.
        assert_eq!(check("EVALUATE { \"EVALUATE\" } // EVALUATE {2}"), Ok(()));
        assert_eq!(check("EVALUATE 'EVALUATE'"), Ok(()));
        assert_eq!(check("EVALUATE {[EVALUATE]}"), Ok(()));
        assert_eq!(check("EVALUATE {1} /* EVALUATE {2} */"), Ok(()));
    }

    #[test]
    fn mcp_allowlist_never_lists_what_acts_or_runs_sql_text() {
        let never = [
            "ts_stat",
            "ts_rewrite",
            "query_to_xml",
            "query_to_xmlschema",
            "query_to_xml_and_xmlschema",
            "cursor_to_xml",
            "table_to_xml",
            "database_to_xml",
            "schema_to_xml",
            "crosstab",
            "crosstab2",
            "connectby",
            "xpath",
            "xpath_table",
            "dblink",
            "dblink_exec",
            "set_config",
            "nextval",
            "setval",
            "currval",
            "lastval",
            "setseed",
            "pg_sleep",
            "pg_read_file",
            "pg_ls_dir",
            "pg_stat_file",
            "pg_wal_replay_pause",
            "pg_backup_start",
            "pg_start_backup",
            "pg_stat_reset",
            "pg_reload_conf",
            "pg_rotate_logfile",
            "pg_promote",
            "pg_terminate_backend",
            "pg_cancel_backend",
            "pg_advisory_lock",
            "txid_current",
            "lo_import",
            "lo_export",
            "load_extension",
            "sleep",
            "benchmark",
            "get_lock",
            "release_lock",
            "load_file",
            "last_insert_id",
            "randomblob",
            "zeroblob",
            "openrowset",
            "openquery",
            "opendatasource",
            "pg_notify",
        ];
        for f in never {
            for family in [
                Family::Postgres,
                Family::Mysql,
                Family::Sqlite,
                Family::Tsql,
                Family::Oracle,
                Family::Other,
            ] {
                assert!(!family.lists(f), "{f} is listed");
            }
        }
        for list in [STANDARD, POSTGRES, MYSQL, SQLITE, TSQL, ORACLE, NOT_CALLS] {
            assert!(list.iter().all(|f| f
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')));
        }
        // Every driver's vector functions: never one that acts, runs SQL
        // text, calls a model or another service; lower case.
        let models = [
            "vector_embedding",
            "ai_generate_embeddings",
            "aisimilarity",
            "ai_similarity",
            "libsql_vector_idx",
            "distance",
        ];
        for d in dbine_drivers::all() {
            let id = d.info().id;
            let list = vector_functions(id);
            for f in never.iter().chain(&models) {
                assert!(!list.contains(f), "{f} is listed for {id}");
            }
            assert!(
                list.iter().all(|f| f
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')),
                "{id}"
            );
        }
        // A service of its own, not a read of the warehouse.
        assert!(!vector_functions("databricks").contains(&"vector_search"));
    }

    fn on(driver: &str, q: &str) -> Result<(), String> {
        let d = dbine_drivers::find(driver).expect(driver);
        let info = d.info();
        approval_free(info.language, info.id, info.dialect, &d.script_dialect(), q)
    }

    /// Similarity searches run without approval where the server enforces
    /// the read: pgvector's operators aren't calls and its functions are
    /// listed for the PostgreSQL variants that ship it; MySQL 9, MariaDB
    /// and libSQL have their own.
    #[test]
    fn mcp_allowlist_vector_searches_need_no_approval() {
        for q in [
            "SELECT id, embedding <-> '[1,2,3]' AS d FROM items ORDER BY embedding <-> '[1,2,3]' LIMIT 5",
            "SELECT id FROM items ORDER BY embedding <=> '[1,2,3]'::vector(3) LIMIT 5",
            "SELECT id FROM items ORDER BY embedding <#> $1 LIMIT 5",
            "SELECT id FROM items ORDER BY embedding <+> '[1,2,3]' LIMIT 5",
            "SELECT id FROM items ORDER BY b <~> '101'::bit(3), b <%> '101'::bit(3) LIMIT 5",
            "SELECT id, cosine_distance(embedding, '[1,2,3]') AS d, l2_distance(embedding, '[1,2,3]'), inner_product(embedding, '[1,2,3]'), l1_distance(embedding, '[1,2,3]') FROM items ORDER BY d LIMIT 5",
            "SELECT vector_dims(embedding), vector_norm(embedding), l2_normalize(embedding), subvector(embedding, 1, 2), binary_quantize(embedding) FROM items",
            "SELECT hamming_distance(b, '101'), jaccard_distance(b, '101'), avg(embedding), sum(embedding) FROM items",
            "SELECT id FROM items ORDER BY embedding <=> l2_normalize('[1,2,3]') LIMIT 5",
        ] {
            assert_eq!(on("postgres", q), Ok(()), "{q}");
        }
        assert_eq!(
            on(
                "timescaledb",
                "SELECT id FROM t ORDER BY cosine_distance(v, '[1,2]') LIMIT 3"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "cockroachdb",
                "SELECT id FROM t ORDER BY v <-> '[1,2]', l2_distance(v, '[1,2]') LIMIT 3"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "mysql",
                "SELECT id, VECTOR_TO_STRING(v), VECTOR_DIM(v) FROM t WHERE v = STRING_TO_VECTOR('[1,2]')"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "mariadb",
                "SELECT id FROM t ORDER BY VEC_DISTANCE_COSINE(v, VEC_FromText('[1,2]')) LIMIT 5"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "libsql",
                "SELECT id, vector_extract(v), vector_distance_cos(v, vector32('[1,2]')) FROM vector_top_k('idx', vector32('[1,2]'), 5) JOIN t ON t.rowid = id"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "sqlserver",
                "SELECT TOP 5 id, VECTOR_DISTANCE('cosine', v, CAST('[1,2]' AS VECTOR(2))) AS d FROM t ORDER BY d"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "oracle",
                "SELECT id FROM t ORDER BY VECTOR_DISTANCE(v, TO_VECTOR('[1,2]'), COSINE) FETCH FIRST 5 ROWS ONLY"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "clickhouse",
                "SELECT id, cosineDistance(v, [1., 2.]) AS d FROM t ORDER BY L2Distance(v, [1., 2.]) LIMIT 5"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "duckdb",
                "SELECT id, array_cosine_similarity(v, [1, 2]::FLOAT[2]) AS s FROM t ORDER BY s DESC LIMIT 5"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "snowflake",
                "SELECT id, VECTOR_COSINE_SIMILARITY(v, CAST([1, 2] AS VECTOR(FLOAT, 2))) FROM t"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "spanner",
                "SELECT id FROM t ORDER BY APPROX_COSINE_DISTANCE(v, [1.0, 2.0], options => JSON '{}') LIMIT 5"
            ),
            Ok(())
        );
        assert_eq!(
            on(
                "couchbase",
                "SELECT b.name FROM b ORDER BY APPROX_VECTOR_DISTANCE(b.v, [1, 2], 'cosine') LIMIT 5"
            ),
            Ok(())
        );
        assert_eq!(
            on("cosmosdb", "SELECT TOP 5 c.id, VectorDistance(c.v, [1, 2]) AS s FROM c ORDER BY VectorDistance(c.v, [1, 2])"),
            Ok(())
        );
    }

    /// A vector name is one engine's: on another server of the family (or
    /// where an extension's name isn't one) it's a user's function, and a
    /// function that calls a model always asks.
    #[test]
    fn mcp_allowlist_other_engines_vector_names_ask() {
        // TiDB's and SingleStore's names on MySQL; MySQL 9's on MariaDB.
        assert_eq!(
            on("mysql", "SELECT VEC_COSINE_DISTANCE(v, '[1,2]') FROM t"),
            Err("VEC_COSINE_DISTANCE".into())
        );
        assert_eq!(
            on("mysql", "SELECT dot_product(v, v) FROM t"),
            Err("dot_product".into())
        );
        assert_eq!(
            on("mariadb", "SELECT STRING_TO_VECTOR('[1,2]')"),
            Err("STRING_TO_VECTOR".into())
        );
        // HeatWave's DISTANCE() is a common name for a user's function.
        assert_eq!(
            on("mysql", "SELECT DISTANCE(v, v, 'COSINE') FROM t"),
            Err("DISTANCE".into())
        );
        // pgvector's names where pgvector isn't (Redshift, Aurora DSQL).
        assert_eq!(
            on("redshift", "SELECT cosine_distance(v, v) FROM t"),
            Err("cosine_distance".into())
        );
        assert_eq!(
            on("dsql", "SELECT l2_distance(v, v) FROM t"),
            Err("l2_distance".into())
        );
        // Qualified with the system schema: not pgvector's.
        assert_eq!(
            on("postgres", "SELECT pg_catalog.cosine_distance(v, v) FROM t"),
            Err("pg_catalog.cosine_distance".into())
        );
        // A model or another service.
        assert_eq!(
            on(
                "oracle",
                "SELECT VECTOR_EMBEDDING(doc_model USING 'hello' AS data) FROM dual"
            ),
            Err("VECTOR_EMBEDDING".into())
        );
        assert_eq!(
            on(
                "sqlserver",
                "SELECT AI_GENERATE_EMBEDDINGS(N'hola' USE MODEL m)"
            ),
            Err("AI_GENERATE_EMBEDDINGS".into())
        );
        assert_eq!(
            on(
                "databricks",
                "SELECT * FROM vector_search(index => 'i', query_text => 'x')"
            ),
            Err("vector_search".into())
        );
        // libSQL's names aren't SQLite's (DBine loads no extension there).
        assert_eq!(
            on("sqlite", "SELECT vector_extract(v) FROM t"),
            Err("vector_extract".into())
        );
        // Non-SQL engines aren't checked: their reads are approved. Neo4j's
        // Cypher is (`reads::cypher`): its own vector functions are listed,
        // not a plugin's.
        assert!(on("cassandra", "SELECT similarity_cosine(v, [1, 2]) FROM t").is_err());
        if dbine_drivers::find("neo4j").is_some() {
            assert_eq!(
                on("neo4j", "RETURN vector.similarity.cosine([1, 2], [1, 2])"),
                Ok(())
            );
            assert_eq!(
                on("neo4j", "RETURN gds.similarity.cosine([1, 2], [1, 2])"),
                Err("gds.similarity.cosine".into())
            );
        }
        if dbine_drivers::find("memgraph").is_some() {
            assert!(on("memgraph", "RETURN 1").is_err());
        }
    }

    /// The list follows each family's vector variants: pgvector's names on
    /// KingbaseES, openGauss and Greengage too, RisingWave's and Databend's
    /// own; not on Aurora MySQL or Cloud SQL for MySQL, which have none.
    #[test]
    fn mcp_allowlist_follows_each_variants_vectors() {
        for id in ["kingbase", "opengauss", "greengage"] {
            if dbine_drivers::find(id).is_some() {
                assert_eq!(
                    on(
                        id,
                        "SELECT id FROM t ORDER BY cosine_distance(v, '[1,2]') LIMIT 3"
                    ),
                    Ok(()),
                    "{id}"
                );
            }
            assert_eq!(vector_extension(id), Some("vector"), "{id}");
        }
        if dbine_drivers::find("risingwave").is_some() {
            assert_eq!(
                on(
                    "risingwave",
                    "SELECT l2_distance(v, '[1,2]'), vector_norm(v), subvector(v, 1, 1) FROM t"
                ),
                Ok(())
            );
            assert_eq!(
                on("risingwave", "SELECT binary_quantize(v) FROM t"),
                Err("binary_quantize".into())
            );
        }
        // RisingWave's and CockroachDB's are built-ins, not an extension's.
        assert_eq!(vector_extension("risingwave"), None);
        assert_eq!(vector_extension("cockroachdb"), None);
        assert_eq!(vector_extension("redshift"), None);
        if dbine_drivers::find("databend").is_some() {
            assert_eq!(
                on(
                    "databend",
                    "SELECT cosine_distance(v, [1,2]), vector_dims(v), vector_norm(v) FROM t"
                ),
                Ok(())
            );
        }
        for id in ["aurora-mysql", "cloudsql-mysql"] {
            assert!(vector_functions(id).is_empty(), "{id}");
            if dbine_drivers::find(id).is_some() {
                assert_eq!(
                    on(id, "SELECT STRING_TO_VECTOR('[1,2]')"),
                    Err("STRING_TO_VECTOR".into()),
                    "{id}"
                );
            }
        }
    }

    /// pgvector's names are an extension's: the read skips the approval
    /// only after the server says each one called is the extension's. The
    /// calls are found as the allowlist finds them; the check reads the
    /// catalogs only.
    #[test]
    fn mcp_allowlist_extension_calls_are_checked_on_the_server() {
        let pg = dbine_driver::ScriptDialect::postgres();
        assert_eq!(
            extension_calls(
                "postgres",
                &pg,
                "SELECT id, COSINE_DISTANCE(v, '[1,2]'), l2_distance(v, '[1,2]'), \
                 cosine_distance(w, '[1,2]') FROM t ORDER BY v <-> '[1,2]' LIMIT 5"
            ),
            ["cosine_distance", "l2_distance"]
        );
        // Operators and built-ins aren't calls of the extension's.
        assert!(extension_calls(
            "postgres",
            &pg,
            "SELECT lower(name), avg(v) FROM t ORDER BY v <=> '[1,2]'"
        )
        .is_empty());
        // Built-ins elsewhere: nothing to check.
        assert!(extension_calls("cockroachdb", &pg, "SELECT l2_distance(v, v) FROM t").is_empty());
        assert!(extension_calls("risingwave", &pg, "SELECT l2_distance(v, v) FROM t").is_empty());
        let sql = extension_check_sql("vector", &["cosine_distance", "l2_distance"]);
        assert!(
            sql.starts_with("SELECT count(*) FROM pg_catalog.pg_proc p"),
            "{sql}"
        );
        assert!(
            sql.contains("p.proname IN ('cosine_distance', 'l2_distance')"),
            "{sql}"
        );
        assert!(sql.contains("e.extname = 'vector'"), "{sql}");
        assert!(
            dbine_driver::read_only::first_write(&sql).is_none(),
            "a read: {sql}"
        );
        // The check itself calls no function the allowlist doesn't know.
        assert_eq!(on("postgres", &sql), Ok(()));
    }
}
