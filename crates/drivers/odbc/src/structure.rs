//! What "Comparar esquemas" needs beyond what the ODBC catalog functions
//! give (columns, keys, SQLStatistics indexes): CHECK constraints, INCLUDE
//! columns, and the sequences, aliases / synonyms and user-defined types,
//! read from each engine's own catalog and written back as its SQL.
//!
//! What each engine has here:
//! - Db2 LUW: CHECKs, INCLUDE on unique indexes, sequences, aliases (as
//!   synonyms), distinct and structured types.
//! - Db2 for z/OS: CHECKs, sequences, aliases, distinct types.
//! - Db2 for i: CHECKs, sequences, aliases.
//! - Sybase ASE, Teradata: CHECKs.
//! - SQL Anywhere: CHECKs, sequences.
//! - Informix / GBase 8s: CHECKs, sequences, synonyms.
//! - Vertica: CHECKs, sequences.
//! - MonetDB: sequences.
//! - Netezza: sequences and synonyms (read by column name: its system
//!   views aren't documented column by column).
//! - Dameng: CHECKs, sequences, synonyms and object types (its
//!   Oracle-compatible dictionary).
//! - Altibase: CHECKs, sequences, synonyms.
//! - CUBRID: serials (as sequences) and synonyms (11.2+).
//! - Ingres: CHECKs, sequences, synonyms.
//! - Mimer SQL: CHECKs, sequences, synonyms and domains (as types).
//!
//! What they lack: Netezza, CUBRID and MonetDB have no CHECK constraints
//! (CUBRID and Netezza don't take them; MonetDB's aren't in its catalog
//! before 11.49); Altibase, CUBRID, Ingres and Netezza have no
//! user-defined SQL types; public synonyms (Dameng, Altibase, CUBRID)
//! aren't schema objects and are left out.
//!
//! Every catalog query returns text columns; a CHECK whose text is split
//! over several rows (ASE's syscomments, Informix's syschecks) comes as
//! consecutive rows with the same key, joined in order.

use crate::design::Eng;
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::{kinds, CheckDef, ObjectKindInfo, TableSchema};

type Rows = Vec<Vec<Option<String>>>;

fn col(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten().map(|s| s.trim_end().to_string())
}

fn text(r: &[Option<String>], i: usize) -> String {
    col(r, i).unwrap_or_default().trim().to_string()
}

/// The kinds each engine has beyond tables, views and routines.
pub fn object_kinds(e: Eng) -> Vec<ObjectKindInfo> {
    match e {
        Eng::Db2 | Eng::Db2zos => vec![ObjectKindInfo::sequences(), ObjectKindInfo::synonyms(), ObjectKindInfo::types()],
        Eng::Db2i => vec![ObjectKindInfo::sequences(), ObjectKindInfo::synonyms()],
        Eng::Informix => vec![ObjectKindInfo::sequences(), ObjectKindInfo::synonyms()],
        Eng::Sqla | Eng::Vertica | Eng::MonetDb => vec![ObjectKindInfo::sequences()],
        Eng::Netezza | Eng::Altibase | Eng::Cubrid | Eng::Ingres => vec![ObjectKindInfo::sequences(), ObjectKindInfo::synonyms()],
        Eng::Dameng | Eng::Mimer => vec![ObjectKindInfo::sequences(), ObjectKindInfo::synonyms(), ObjectKindInfo::types()],
        _ => Vec::new(),
    }
}

/// Rows: schema, table, constraint name, condition (or a piece of it).
pub fn checks_sql(e: Eng) -> &'static [&'static str] {
    match e {
        Eng::Db2 => &["SELECT TRIM(TABSCHEMA), TABNAME, CONSTNAME, TEXT FROM SYSCAT.CHECKS WHERE TYPE = 'C' ORDER BY 1, 2, 3"],
        Eng::Db2zos => &["SELECT TRIM(TBOWNER), TBNAME, CHECKNAME, CHECKCONDITION FROM SYSIBM.SYSCHECKS ORDER BY 1, 2, 3"],
        Eng::Db2i => &[
            "SELECT c.TABLE_SCHEMA, c.TABLE_NAME, k.CONSTRAINT_NAME, k.CHECK_CLAUSE
             FROM QSYS2.SYSCHKCST k JOIN QSYS2.SYSCST c ON c.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND c.CONSTRAINT_NAME = k.CONSTRAINT_NAME
             ORDER BY 1, 2, 3",
        ],
        // sysconstraints.status 128: a CHECK; its text (`CONSTRAINT n CHECK (…)`) in syscomments pieces.
        Eng::Ase => &[
            "SELECT user_name(o.uid), o.name, c.name, m.text
             FROM sysconstraints k JOIN sysobjects o ON o.id = k.tableid JOIN sysobjects c ON c.id = k.constrid
             JOIN syscomments m ON m.id = k.constrid
             WHERE k.status & 128 = 128 ORDER BY 1, 2, 3, m.colid2, m.colid",
        ],
        Eng::Sqla => &[
            "SELECT u.user_name, t.table_name, c.constraint_name, k.check_defn
             FROM SYS.SYSCONSTRAINT c JOIN SYS.SYSCHECK k ON k.check_id = c.constraint_id
             JOIN SYS.SYSTAB t ON t.object_id = c.table_object_id JOIN SYS.SYSUSER u ON u.user_id = t.creator
             WHERE c.constraint_type IN ('C', 'T') ORDER BY 1, 2, 3",
        ],
        Eng::Informix => &[
            "SELECT TRIM(t.owner), t.tabname, c.constrname, k.checktext
             FROM sysconstraints c JOIN systables t ON t.tabid = c.tabid JOIN syschecks k ON k.constrid = c.constrid
             WHERE c.constrtype = 'C' AND k.type = 'T' ORDER BY 1, 2, 3, k.seqno",
        ],
        // Table-level CHECKs (with their CONSTRAINT clause) and column ones.
        Eng::Teradata => &[
            "SELECT TRIM(DatabaseName), TRIM(TableName), TRIM(ConstraintName), TableCheck FROM DBC.TableConstraintsV
             WHERE ConstraintType = 'C' ORDER BY 1, 2, 3",
            "SELECT TRIM(DatabaseName), TRIM(TableName), '', ColumnConstraint FROM DBC.ColumnsV
             WHERE ColumnConstraint IS NOT NULL ORDER BY 1, 2, 4",
        ],
        Eng::Vertica => &[
            "SELECT t.table_schema, t.table_name, c.constraint_name, c.predicate
             FROM v_catalog.table_constraints c JOIN v_catalog.tables t ON t.table_id = c.table_id
             WHERE c.constraint_type = 'c' ORDER BY 1, 2, 3",
        ],
        // NOT NULL columns are CHECKs too there: dropped in attach_checks.
        Eng::Dameng => &[
            "SELECT OWNER, TABLE_NAME, CONSTRAINT_NAME, SEARCH_CONDITION FROM ALL_CONSTRAINTS
             WHERE CONSTRAINT_TYPE = 'C' AND OWNER NOT IN ('SYS', 'SYSAUDITOR', 'SYSSSO', 'CTISYS', 'SYSJOB') ORDER BY 1, 2, 3",
        ],
        // SYS_CONSTRAINTS_.CONSTRAINT_TYPE 7: CHECK (6.3.1+).
        Eng::Altibase => &[
            "SELECT u.USER_NAME, t.TABLE_NAME, c.CONSTRAINT_NAME, c.CHECK_CONDITION
             FROM SYSTEM_.SYS_CONSTRAINTS_ c JOIN SYSTEM_.SYS_TABLES_ t ON t.TABLE_ID = c.TABLE_ID
             JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = c.USER_ID
             WHERE c.CONSTRAINT_TYPE = 7 ORDER BY 1, 2, 3",
        ],
        Eng::Ingres => &[
            "SELECT TRIM(schema_name), TRIM(table_name), TRIM(constraint_name), text_segment FROM iiconstraints
             WHERE constraint_type = 'C' ORDER BY 1, 2, 3, text_sequence",
        ],
        Eng::Mimer => &[
            "SELECT t.TABLE_SCHEMA, t.TABLE_NAME, c.CONSTRAINT_NAME, c.CHECK_CLAUSE
             FROM INFORMATION_SCHEMA.CHECK_CONSTRAINTS c JOIN INFORMATION_SCHEMA.TABLE_CONSTRAINTS t
               ON t.CONSTRAINT_SCHEMA = c.CONSTRAINT_SCHEMA AND t.CONSTRAINT_NAME = c.CONSTRAINT_NAME
             WHERE t.CONSTRAINT_TYPE = 'CHECK' ORDER BY 1, 2, 3",
        ],
        _ => &[],
    }
}

/// `"COL" IS NOT NULL` / `COL IS NOT NULL`: a column's nullability kept as
/// a CHECK (Dameng, Mimer), not a constraint of its own.
pub fn is_not_null_check(cond: &str) -> bool {
    let c = cond.trim();
    let c = c.strip_prefix('(').and_then(|x| x.strip_suffix(')')).unwrap_or(c).trim();
    let Some(col) = c.strip_suffix("IS NOT NULL").or_else(|| c.strip_suffix("is not null")) else { return false };
    let col = col.trim();
    let bare = col.strip_prefix('"').and_then(|x| x.strip_suffix('"')).unwrap_or(col);
    !bare.is_empty() && !bare.contains(['"', ' ', '(', ')'])
}

/// Rows: schema, table, index, column: the INCLUDE columns, in order.
pub fn include_sql(e: Eng) -> Option<&'static str> {
    match e {
        Eng::Db2 => Some(
            "SELECT TRIM(i.TABSCHEMA), i.TABNAME, i.INDNAME, c.COLNAME
             FROM SYSCAT.INDEXES i JOIN SYSCAT.INDEXCOLUSE c ON c.INDSCHEMA = i.INDSCHEMA AND c.INDNAME = i.INDNAME
             WHERE c.COLORDER = 'I' ORDER BY 1, 2, 3, c.COLSEQ",
        ),
        _ => None,
    }
}

/// `CONSTRAINT n CHECK (a > 0)` / `check(a > 0)` / `(a > 0)` → `(a > 0)`.
pub fn check_condition(text: &str) -> String {
    let t = text.trim();
    let upper = t.to_ascii_uppercase();
    let t = match upper.find("CHECK") {
        Some(i) if upper[..i].trim().is_empty() || upper[..i].trim_start().starts_with("CONSTRAINT") => t[i + 5..].trim(),
        _ => t,
    };
    t.trim_end_matches(';').trim().to_string()
}

/// Puts the CHECKs read with [`checks_sql`] on their tables.
pub fn attach_checks(tables: &mut [TableSchema], rows: &Rows, has_schemas: bool) {
    let mut pieces: Vec<((String, String, String), String)> = Vec::new();
    for r in rows {
        let key = (text(r, 0), text(r, 1), text(r, 2));
        // Pieces as they are: a space at the end of one counts.
        let piece = r.get(3).cloned().flatten().unwrap_or_default();
        // An unnamed (column) CHECK is one row, never continued.
        match pieces.last_mut() {
            Some((k, t)) if !key.2.is_empty() && *k == key => t.push_str(&piece),
            _ => pieces.push((key, piece)),
        }
    }
    for ((schema, table, name), txt) in pieces {
        let cond = check_condition(&txt);
        if cond.is_empty() || is_not_null_check(&cond) {
            continue;
        }
        let found = tables.iter_mut().find(|t| {
            t.name.eq_ignore_ascii_case(&table) && (!has_schemas || t.schema.as_deref().unwrap_or("").eq_ignore_ascii_case(&schema))
        });
        if let Some(t) = found {
            t.checks.push(CheckDef { name: (!name.is_empty()).then_some(name), expression: cond });
        }
    }
}

/// Moves the INCLUDE columns read with [`include_sql`] out of the index keys.
pub fn attach_includes(tables: &mut [TableSchema], rows: &Rows) {
    for r in rows {
        let (schema, table, index, column) = (text(r, 0), text(r, 1), text(r, 2), text(r, 3));
        let Some(t) = tables.iter_mut().find(|t| t.name == table && t.schema.as_deref().unwrap_or("") == schema) else { continue };
        let Some(ix) = t.indexes.iter_mut().find(|i| i.name == index) else { continue };
        ix.columns.retain(|c| *c != column);
        if !ix.include.contains(&column) {
            ix.include.push(column);
        }
    }
}

/// Rows: kind, schema, name.
pub fn objects_sql(e: Eng) -> &'static [&'static str] {
    match e {
        Eng::Db2 => &[
            "SELECT 'sequence', TRIM(SEQSCHEMA), SEQNAME FROM SYSCAT.SEQUENCES WHERE SEQTYPE = 'S' AND SEQSCHEMA NOT LIKE 'SYS%'",
            "SELECT 'synonym', TRIM(TABSCHEMA), TABNAME FROM SYSCAT.TABLES WHERE TYPE = 'A' AND TABSCHEMA NOT LIKE 'SYS%'",
            "SELECT 'type', TRIM(TYPESCHEMA), TYPENAME FROM SYSCAT.DATATYPES WHERE METATYPE IN ('T', 'R') AND TYPESCHEMA NOT LIKE 'SYS%'",
        ],
        Eng::Db2zos => &[
            "SELECT 'sequence', TRIM(SCHEMA), NAME FROM SYSIBM.SYSSEQUENCES WHERE SEQTYPE = 'S' AND SCHEMA NOT LIKE 'SYS%'",
            "SELECT 'synonym', TRIM(CREATOR), NAME FROM SYSIBM.SYSTABLES WHERE TYPE = 'A' AND CREATOR NOT LIKE 'SYS%'",
            "SELECT 'type', TRIM(SCHEMA), NAME FROM SYSIBM.SYSDATATYPES WHERE METATYPE = 'T' AND SCHEMA NOT LIKE 'SYS%'",
        ],
        Eng::Db2i => &[
            "SELECT 'sequence', SEQUENCE_SCHEMA, SEQUENCE_NAME FROM QSYS2.SYSSEQUENCES WHERE SEQUENCE_SCHEMA NOT LIKE 'Q%'",
            "SELECT 'synonym', TABLE_SCHEMA, TABLE_NAME FROM QSYS2.SYSTABLES WHERE TABLE_TYPE = 'A' AND TABLE_SCHEMA NOT LIKE 'Q%'",
        ],
        Eng::Sqla => &["SELECT 'sequence', u.user_name, s.sequence_name FROM SYS.SYSSEQUENCE s JOIN SYS.SYSUSER u ON u.user_id = s.owner"],
        Eng::Informix => &[
            "SELECT 'sequence', TRIM(owner), tabname FROM systables WHERE tabtype = 'Q' AND tabid >= 100",
            "SELECT 'synonym', TRIM(owner), tabname FROM systables WHERE tabtype IN ('S', 'P') AND tabid >= 100",
        ],
        // Identity columns' sequences belong to their tables.
        Eng::Vertica => &[
            "SELECT 'sequence', sequence_schema, sequence_name FROM v_catalog.sequences
             WHERE identity_table_name IS NULL OR identity_table_name = ''",
        ],
        // SERIAL columns' sequences (`seq_<id>`) belong to their tables.
        Eng::MonetDb => &[
            "SELECT 'sequence', s.name, q.name FROM sys.sequences q JOIN sys.schemas s ON s.id = q.schema_id
             WHERE NOT s.system AND q.name NOT LIKE 'seq\\_%' ESCAPE '\\'",
        ],
        Eng::Netezza => &[
            "SELECT 'sequence', SCHEMA, SEQNAME FROM _V_SEQUENCE",
            "SELECT 'synonym', SCHEMA, SYNONYM_NAME FROM _V_SYNONYM",
        ],
        // Public synonyms (owner PUBLIC) aren't a schema's.
        Eng::Dameng => &[
            "SELECT 'sequence', SEQUENCE_OWNER, SEQUENCE_NAME FROM ALL_SEQUENCES
             WHERE SEQUENCE_OWNER NOT IN ('SYS', 'SYSAUDITOR', 'SYSSSO', 'CTISYS', 'SYSJOB')",
            "SELECT 'synonym', OWNER, SYNONYM_NAME FROM ALL_SYNONYMS
             WHERE OWNER NOT IN ('PUBLIC', 'SYS', 'SYSAUDITOR', 'SYSSSO', 'CTISYS', 'SYSJOB')",
            "SELECT DISTINCT 'type', OWNER, NAME FROM ALL_SOURCE
             WHERE TYPE = 'TYPE' AND OWNER NOT IN ('SYS', 'SYSAUDITOR', 'SYSSSO', 'CTISYS', 'SYSJOB')",
        ],
        Eng::Altibase => &[
            "SELECT 'sequence', u.USER_NAME, t.TABLE_NAME FROM SYSTEM_.SYS_TABLES_ t JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = t.USER_ID
             WHERE t.TABLE_TYPE = 'S' AND u.USER_NAME <> 'SYSTEM_'",
            "SELECT 'synonym', u.USER_NAME, s.SYNONYM_NAME FROM SYSTEM_.SYS_SYNONYMS_ s JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = s.SYNONYM_OWNER_ID
             WHERE u.USER_NAME <> 'SYSTEM_'",
        ],
        // AUTO_INCREMENT columns' serials belong to their tables.
        Eng::Cubrid => &[
            "SELECT 'sequence', '', name FROM db_serial WHERE class_name IS NULL",
            "SELECT 'synonym', '', synonym_name FROM db_synonym WHERE is_public_synonym = 'NO'",
        ],
        // Identity columns' sequences belong to their tables.
        Eng::Ingres => &[
            "SELECT 'sequence', TRIM(seq_owner), TRIM(seq_name) FROM iisequences WHERE ident_flag <> 'Y' AND seq_owner <> '$ingres'",
            "SELECT 'synonym', TRIM(synonym_owner), TRIM(synonym_name) FROM iisynonyms WHERE synonym_owner <> '$ingres'",
        ],
        Eng::Mimer => &[
            "SELECT 'sequence', SEQUENCE_SCHEMA, SEQUENCE_NAME FROM INFORMATION_SCHEMA.EXT_SEQUENCES",
            "SELECT 'synonym', SYNONYM_SCHEMA, SYNONYM_NAME FROM INFORMATION_SCHEMA.EXT_SYNONYMS",
            "SELECT 'type', DOMAIN_SCHEMA, DOMAIN_NAME FROM INFORMATION_SCHEMA.DOMAINS",
        ],
        _ => &[],
    }
}

/// Whether the [`definition_sql`] queries take the schema before the
/// name (CUBRID's serials and synonyms go by name only).
pub fn definition_takes_schema(e: Eng) -> bool {
    e != Eng::Cubrid
}

/// Netezza: a `SELECT *` (schema and name as parameters) whose columns
/// [`build_named`] picks by name.
pub fn named_definition_sql(e: Eng, kind: &str) -> Option<&'static str> {
    match (e, kind) {
        (Eng::Netezza, kinds::SEQUENCE) => Some("SELECT * FROM _V_SEQUENCE WHERE SCHEMA = ? AND SEQNAME = ?"),
        (Eng::Netezza, kinds::SYNONYM) => Some("SELECT * FROM _V_SYNONYM WHERE SCHEMA = ? AND SYNONYM_NAME = ?"),
        _ => None,
    }
}

/// The CREATE from a [`named_definition_sql`] row: its columns are taken
/// by name (any of several spellings), into [`build`]'s layout. `None`
/// when the row doesn't have what the statement needs.
pub fn build_named(e: Eng, kind: &str, q: Quote, schema: Option<&str>, name: &str, cols: &[String], row: &[Option<String>]) -> Option<String> {
    let pick = |names: &[&str]| -> String {
        names
            .iter()
            .find_map(|n| cols.iter().position(|c| c.eq_ignore_ascii_case(n)))
            .and_then(|i| col(row, i))
            .map(|v| v.trim().to_string())
            .unwrap_or_default()
    };
    let layout: Vec<String> = match kind {
        kinds::SEQUENCE => {
            let inc = pick(&["INCREMENT", "INCREMENT_BY", "INCREMENTBY", "SEQINCREMENT"]);
            if inc.is_empty() {
                return None;
            }
            vec![
                pick(&["DATATYPE", "DATA_TYPE", "SEQTYPE"]),
                String::new(),
                pick(&["STARTVALUE", "START_VALUE", "START", "SEQSTART"]),
                inc,
                pick(&["MINVALUE", "MIN_VALUE", "SEQMIN"]),
                pick(&["MAXVALUE", "MAX_VALUE", "SEQMAX"]),
                pick(&["CYCLE", "IS_CYCLE", "CYCLE_FLAG", "SEQCYCLE"]),
                String::new(),
                String::new(),
            ]
        }
        kinds::SYNONYM => {
            let target = pick(&["REFOBJNAME", "REF_OBJNAME", "OBJNAME", "TABLE_NAME"]);
            if target.is_empty() {
                return None;
            }
            vec![pick(&["REFSCHEMA", "REF_SCHEMA", "OBJSCHEMA"]), target]
        }
        _ => return None,
    };
    let row: Vec<Option<String>> = layout.into_iter().map(Some).collect();
    build(e, kind, q, schema, name, &[vec![row]])
}

/// The queries (each with the schema and the name as parameters) whose
/// rows [`build`] turns into the object's CREATE.
pub fn definition_sql(e: Eng, kind: &str) -> &'static [&'static str] {
    match (e, kind) {
        (Eng::Db2, kinds::SEQUENCE) => &[
            "SELECT d.TYPENAME, s.PRECISION, s.START, s.INCREMENT, s.MINVALUE, s.MAXVALUE, s.CYCLE, s.CACHE, s.ORDER
             FROM SYSCAT.SEQUENCES s JOIN SYSCAT.DATATYPES d ON d.TYPEID = s.DATATYPEID AND d.TYPESCHEMA = 'SYSIBM'
             WHERE TRIM(s.SEQSCHEMA) = ? AND s.SEQNAME = ?",
        ],
        (Eng::Db2, kinds::SYNONYM) => &["SELECT TRIM(BASE_TABSCHEMA), BASE_TABNAME FROM SYSCAT.TABLES WHERE TRIM(TABSCHEMA) = ? AND TABNAME = ? AND TYPE = 'A'"],
        (Eng::Db2, kinds::TYPE) => &[
            "SELECT METATYPE, TRIM(SOURCESCHEMA), SOURCENAME, LENGTH, SCALE, INSTANTIABLE, FINAL
             FROM SYSCAT.DATATYPES WHERE TRIM(TYPESCHEMA) = ? AND TYPENAME = ?",
            "SELECT ATTR_NAME, TRIM(ATTR_TYPESCHEMA), ATTR_TYPENAME, LENGTH, SCALE FROM SYSCAT.ATTRIBUTES
             WHERE TRIM(TYPESCHEMA) = ? AND TYPENAME = ? AND SOURCE_TYPENAME = TYPENAME AND SOURCE_TYPESCHEMA = TYPESCHEMA
             ORDER BY ORDINAL",
            "SELECT TRIM(SUPER_SCHEMA), SUPER_NAME FROM SYSCAT.HIERARCHIES WHERE METATYPE = 'U' AND TRIM(SUB_SCHEMA) = ? AND SUB_NAME = ?",
        ],
        (Eng::Db2zos, kinds::SEQUENCE) => &[
            "SELECT DATATYPEID, PRECISION, START, INCREMENT, MINVALUE, MAXVALUE, CYCLE, CACHE, ORDER
             FROM SYSIBM.SYSSEQUENCES WHERE TRIM(SCHEMA) = ? AND NAME = ?",
        ],
        (Eng::Db2zos, kinds::SYNONYM) => &["SELECT TRIM(TBCREATOR), TBNAME FROM SYSIBM.SYSTABLES WHERE TRIM(CREATOR) = ? AND NAME = ? AND TYPE = 'A'"],
        (Eng::Db2zos, kinds::TYPE) => &["SELECT 'T', TRIM(SOURCESCHEMA), SOURCETYPE, LENGTH, SCALE, 'Y', 'Y' FROM SYSIBM.SYSDATATYPES WHERE TRIM(SCHEMA) = ? AND NAME = ?"],
        (Eng::Db2i, kinds::SEQUENCE) => &[
            "SELECT DATA_TYPE, NUMERIC_PRECISION, START, INCREMENT, MINIMUM_VALUE, MAXIMUM_VALUE, CYCLE_OPTION, CACHE, ORDER
             FROM QSYS2.SYSSEQUENCES WHERE SEQUENCE_SCHEMA = ? AND SEQUENCE_NAME = ?",
        ],
        (Eng::Db2i, kinds::SYNONYM) => &["SELECT BASE_TABLE_SCHEMA, BASE_TABLE_NAME FROM QSYS2.SYSTABLES WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ?"],
        (Eng::Sqla, kinds::SEQUENCE) => &[
            "SELECT '', '', s.start_with, s.increment_by, s.min_value, s.max_value, s.cycle, s.cache, ''
             FROM SYS.SYSSEQUENCE s JOIN SYS.SYSUSER u ON u.user_id = s.owner WHERE u.user_name = ? AND s.sequence_name = ?",
        ],
        (Eng::Informix, kinds::SEQUENCE) => &[
            "SELECT '', '', q.start_val, q.inc_val, q.min_val, q.max_val, q.cycle, q.cache, q.order
             FROM syssequences q JOIN systables t ON t.tabid = q.tabid WHERE TRIM(t.owner) = ? AND t.tabname = ?",
        ],
        (Eng::Informix, kinds::SYNONYM) => &[
            "SELECT t.tabtype, TRIM(s.servername), TRIM(s.dbname), TRIM(NVL(b.owner, s.owner)), NVL(b.tabname, s.tabname)
             FROM systables t JOIN syssyntable s ON s.tabid = t.tabid LEFT JOIN systables b ON b.tabid = s.btabid
             WHERE TRIM(t.owner) = ? AND t.tabname = ?",
        ],
        (Eng::Vertica, kinds::SEQUENCE) => &[
            "SELECT '', '', '', increment_by, minimum, maximum, allow_cycle, session_cache_count, ''
             FROM v_catalog.sequences WHERE sequence_schema = ? AND sequence_name = ?",
        ],
        (Eng::Dameng, kinds::SEQUENCE) => &[
            "SELECT '', '', LAST_NUMBER, INCREMENT_BY, MIN_VALUE, MAX_VALUE, CYCLE_FLAG, CACHE_SIZE, ORDER_FLAG
             FROM ALL_SEQUENCES WHERE SEQUENCE_OWNER = ? AND SEQUENCE_NAME = ?",
        ],
        (Eng::Dameng, kinds::SYNONYM) => &["SELECT TABLE_OWNER, TABLE_NAME FROM ALL_SYNONYMS WHERE OWNER = ? AND SYNONYM_NAME = ?"],
        (Eng::Dameng, kinds::TYPE) => &["SELECT TEXT FROM ALL_SOURCE WHERE OWNER = ? AND NAME = ? AND TYPE = 'TYPE' ORDER BY LINE"],
        (Eng::Altibase, kinds::SEQUENCE) => &[
            "SELECT '', '', s.START_SEQ, s.INCREMENT_SEQ, s.MIN_SEQ, s.MAX_SEQ, s.IS_CYCLE, s.CACHE_SIZE, ''
             FROM V$SEQ s JOIN SYSTEM_.SYS_TABLES_ t ON t.TABLE_OID = s.SEQ_OID JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = t.USER_ID
             WHERE u.USER_NAME = ? AND t.TABLE_NAME = ?",
        ],
        (Eng::Altibase, kinds::SYNONYM) => &[
            "SELECT s.OBJECT_OWNER_NAME, s.OBJECT_NAME FROM SYSTEM_.SYS_SYNONYMS_ s JOIN SYSTEM_.SYS_USERS_ u ON u.USER_ID = s.SYNONYM_OWNER_ID
             WHERE u.USER_NAME = ? AND s.SYNONYM_NAME = ?",
        ],
        // A serial that hasn't given a value yet starts at current_val.
        (Eng::Cubrid, kinds::SEQUENCE) => &[
            "SELECT '', '', CASE WHEN started = 0 THEN current_val END, increment_val, min_val, max_val, cyclic, cached_num, ''
             FROM db_serial WHERE name = ?",
        ],
        (Eng::Cubrid, kinds::SYNONYM) => &["SELECT target_owner_name, target_name FROM db_synonym WHERE synonym_name = ?"],
        (Eng::Ingres, kinds::SEQUENCE) => &[
            "SELECT data_type, seq_precision, start_value, increment_value, min_value, max_value, cycle_flag,
                    CASE WHEN cache_flag = 'Y' OR cache_size > 0 THEN cache_size ELSE 0 END, order_flag
             FROM iisequences WHERE TRIM(seq_owner) = ? AND TRIM(seq_name) = ?",
        ],
        (Eng::Ingres, kinds::SYNONYM) => &["SELECT TRIM(table_owner), TRIM(table_name) FROM iisynonyms WHERE TRIM(synonym_owner) = ? AND TRIM(synonym_name) = ?"],
        (Eng::Mimer, kinds::SEQUENCE) => &[
            "SELECT IS_UNIQUE, '', INITIAL_VALUE, INCREMENT, '', MAXIMUM_VALUE, '', '', ''
             FROM INFORMATION_SCHEMA.EXT_SEQUENCES WHERE SEQUENCE_SCHEMA = ? AND SEQUENCE_NAME = ?",
        ],
        (Eng::Mimer, kinds::SYNONYM) => &["SELECT TABLE_SCHEMA, TABLE_NAME FROM INFORMATION_SCHEMA.EXT_SYNONYMS WHERE SYNONYM_SCHEMA = ? AND SYNONYM_NAME = ?"],
        (Eng::Mimer, kinds::TYPE) => &[
            "SELECT DATA_TYPE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, DOMAIN_DEFAULT
             FROM INFORMATION_SCHEMA.DOMAINS WHERE DOMAIN_SCHEMA = ? AND DOMAIN_NAME = ?",
            "SELECT k.CONSTRAINT_NAME, c.CHECK_CLAUSE FROM INFORMATION_SCHEMA.DOMAIN_CONSTRAINTS k
             JOIN INFORMATION_SCHEMA.CHECK_CONSTRAINTS c ON c.CONSTRAINT_SCHEMA = k.CONSTRAINT_SCHEMA AND c.CONSTRAINT_NAME = k.CONSTRAINT_NAME
             WHERE k.DOMAIN_SCHEMA = ? AND k.DOMAIN_NAME = ? ORDER BY 1",
        ],
        (Eng::MonetDb, kinds::SEQUENCE) => &[
            "SELECT '', '', q.start, q.increment, q.minvalue, q.maxvalue, q.cycle, q.cacheinc, ''
             FROM sys.sequences q JOIN sys.schemas s ON s.id = q.schema_id WHERE s.name = ? AND q.name = ?",
        ],
        _ => &[],
    }
}

fn yes(v: &str) -> bool {
    matches!(v.trim().to_ascii_uppercase().as_str(), "Y" | "YES" | "1" | "T" | "TRUE")
}

/// A Db2 built-in type with its length / precision and scale.
fn db2_type(name: &str, length: &str, scale: &str) -> String {
    let n = name.trim().to_ascii_uppercase();
    let (len, sc) = (length.trim(), scale.trim());
    match n.as_str() {
        "CHARACTER" | "CHAR" | "VARCHAR" | "GRAPHIC" | "VARGRAPHIC" | "BINARY" | "VARBINARY" | "BLOB" | "CLOB" | "DBCLOB" if !len.is_empty() && len != "0" => {
            format!("{n}({len})")
        }
        "DECIMAL" | "DECFLOAT" | "NUMERIC" if !len.is_empty() && len != "0" => {
            if n == "DECFLOAT" { format!("{n}({len})") } else { format!("{n}({len},{})", if sc.is_empty() { "0" } else { sc }) }
        }
        "TIMESTAMP" if !sc.is_empty() && sc != "6" => format!("TIMESTAMP({sc})"),
        _ => n,
    }
}

/// `(…)` around a condition that has none.
fn wrap(c: &str) -> String {
    if c.starts_with('(') { c.to_string() } else { format!("({c})") }
}

/// An INFORMATION_SCHEMA type (Mimer's domains) with its length or
/// precision and scale.
fn sql_type(name: &str, length: &str, precision: &str, scale: &str) -> String {
    let n = name.trim().to_ascii_uppercase();
    let has = |v: &str| !v.trim().is_empty() && v.trim() != "0";
    if (n.contains("CHAR") || n.contains("BINARY")) && has(length) {
        format!("{n}({})", length.trim())
    } else if matches!(n.as_str(), "DECIMAL" | "NUMERIC") && has(precision) {
        format!("{n}({},{})", precision.trim(), if scale.trim().is_empty() { "0" } else { scale.trim() })
    } else {
        n
    }
}

/// Db2 for z/OS `DATATYPEID` of the types a sequence can have.
fn zos_type(id: &str, precision: &str) -> String {
    match id.trim() {
        "500" => "SMALLINT".into(),
        "496" => "INTEGER".into(),
        "492" => "BIGINT".into(),
        "484" => format!("DECIMAL({},0)", if precision.trim().is_empty() { "31" } else { precision.trim() }),
        other => other.to_string(),
    }
}

/// The object's CREATE from the rows of each [`definition_sql`] query.
pub fn build(e: Eng, kind: &str, q: Quote, schema: Option<&str>, name: &str, sets: &[Rows]) -> Option<String> {
    let first = sets.first()?.first()?;
    let full = qualified_name(q, schema.filter(|s| !s.is_empty()), name);
    let v = |i: usize| text(first, i);
    match (e, kind) {
        // Dameng keeps an object type's source (`TYPE n AS OBJECT …`).
        (Eng::Dameng, kinds::TYPE) => {
            let src: String = sets[0].iter().filter_map(|r| r.first().cloned().flatten()).collect();
            let src = src.trim();
            return (!src.is_empty()).then(|| {
                let body = if src.len() >= 6 && src[..6].eq_ignore_ascii_case("CREATE") { src.to_string() } else { format!("CREATE OR REPLACE {src}") };
                if body.ends_with(';') { body } else { format!("{body};") }
            });
        }
        (Eng::Mimer, kinds::TYPE) => {
            let ty = sql_type(&v(0), &v(1), &v(2), &v(3));
            let mut s = format!("CREATE DOMAIN {full} AS {ty}");
            if !v(4).is_empty() {
                s.push_str(&format!(" DEFAULT {}", v(4)));
            }
            for r in sets.get(1).into_iter().flatten() {
                s.push_str(&format!(" CONSTRAINT {} CHECK {}", quote_ident(q, &text(r, 0)), wrap(&check_condition(&text(r, 1)))));
            }
            s.push(';');
            return Some(s);
        }
        (Eng::Mimer, kinds::SEQUENCE) => {
            let mut s = format!("CREATE {}SEQUENCE {full}", if yes(&v(0)) { "UNIQUE " } else { "" });
            for (kw, i) in [("START WITH", 2), ("INCREMENT BY", 3), ("MAXVALUE", 5)] {
                if !v(i).is_empty() {
                    s.push_str(&format!(" {kw} {}", v(i)));
                }
            }
            s.push(';');
            return Some(s);
        }
        (Eng::Cubrid, kinds::SEQUENCE) => return Some(sequence(e, &full, &v).replacen("CREATE SEQUENCE ", "CREATE SERIAL ", 1)),
        _ => {}
    }
    match kind {
        kinds::SEQUENCE => Some(sequence(e, &full, &v)),
        kinds::SYNONYM => {
            let target = |s: String, n: String| qualified_name(q, Some(s.as_str()).filter(|s| !s.is_empty()), &n);
            Some(match e {
                Eng::Informix => {
                    let (kind, server, db) = (v(0), v(1), v(2));
                    let mut t = target(v(3), v(4));
                    if !db.is_empty() {
                        t = if server.is_empty() { format!("{db}:{t}") } else { format!("{db}@{server}:{t}") };
                    }
                    let scope = if kind.eq_ignore_ascii_case("P") { "PRIVATE " } else { "PUBLIC " };
                    format!("CREATE {scope}SYNONYM {full} FOR {t};")
                }
                Eng::Db2 | Eng::Db2i | Eng::Db2zos => format!("CREATE ALIAS {full} FOR {};", target(v(0), v(1))),
                _ => format!("CREATE SYNONYM {full} FOR {};", target(v(0), v(1))),
            })
        }
        kinds::TYPE => {
            let meta = v(0);
            if meta.eq_ignore_ascii_case("R") {
                let attrs: Vec<String> = sets
                    .get(1)
                    .map(|rows| {
                        rows.iter()
                            .map(|r| {
                                let ty = if text(r, 1).eq_ignore_ascii_case("SYSIBM") {
                                    db2_type(&text(r, 2), &text(r, 3), &text(r, 4))
                                } else {
                                    qualified_name(q, Some(text(r, 1).as_str()), &text(r, 2))
                                };
                                format!("    {} {ty}", quote_ident(q, &text(r, 0)))
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let under = sets.get(2).and_then(|r| r.first()).map(|r| format!(" UNDER {}", qualified_name(q, Some(text(r, 0).as_str()), &text(r, 1))));
                let mut s = format!("CREATE TYPE {full}{}", under.unwrap_or_default());
                if !attrs.is_empty() {
                    s.push_str(&format!(" AS (\n{}\n)", attrs.join(",\n")));
                }
                s.push_str(if yes(&v(5)) { " INSTANTIABLE" } else { " NOT INSTANTIABLE" });
                s.push_str(if yes(&v(6)) { " FINAL" } else { " NOT FINAL" });
                s.push_str(" MODE DB2SQL;");
                Some(s)
            } else {
                let source = if v(1).eq_ignore_ascii_case("SYSIBM") || v(1).is_empty() {
                    db2_type(&v(2), &v(3), &v(4))
                } else {
                    qualified_name(q, Some(v(1).as_str()), &v(2))
                };
                Some(format!("CREATE DISTINCT TYPE {full} AS {source} WITH COMPARISONS;"))
            }
        }
        _ => None,
    }
}

/// Columns: type, precision, start, increment, min, max, cycle, cache, order.
fn sequence(e: Eng, full: &str, v: &dyn Fn(usize) -> String) -> String {
    let mut s = format!("CREATE SEQUENCE {full}");
    let ty = match e {
        Eng::Db2 | Eng::Db2i | Eng::Ingres | Eng::Netezza => {
            let t = v(0).to_ascii_uppercase();
            (!t.is_empty()).then(|| if t == "DECIMAL" || t == "NUMERIC" { format!("DECIMAL({},0)", v(1)) } else { t })
        }
        Eng::Db2zos => Some(zos_type(&v(0), &v(1))),
        _ => None,
    };
    if let Some(t) = ty {
        s.push_str(&format!(" AS {t}"));
    }
    if !v(2).is_empty() {
        s.push_str(&format!(" START WITH {}", v(2)));
    }
    for (kw, i) in [("INCREMENT BY", 3), ("MINVALUE", 4), ("MAXVALUE", 5)] {
        if !v(i).is_empty() {
            s.push_str(&format!(" {kw} {}", v(i)));
        }
    }
    // NOCYCLE / NOCACHE / NOORDER in one word.
    let informix = matches!(e, Eng::Informix | Eng::Dameng | Eng::Altibase | Eng::Cubrid);
    let cycle = yes(&v(6));
    s.push_str(match (cycle, informix) {
        (true, _) => " CYCLE",
        (false, true) => " NOCYCLE",
        (false, false) => " NO CYCLE",
    });
    let cache = v(7);
    let no_cache = cache.is_empty() || cache == "0" || (e == Eng::Vertica && cache == "1");
    if no_cache {
        s.push_str(if informix { " NOCACHE" } else { " NO CACHE" });
    } else {
        s.push_str(&format!(" CACHE {cache}"));
    }
    if matches!(e, Eng::Db2 | Eng::Db2zos | Eng::Db2i | Eng::Informix | Eng::Dameng | Eng::Ingres) && !v(8).is_empty() {
        s.push_str(match (yes(&v(8)), informix) {
            (true, _) => " ORDER",
            (false, true) => " NOORDER",
            (false, false) => " NO ORDER",
        });
    }
    s.push(';');
    s
}

/// Informix names a constraint after it: `CHECK (…) CONSTRAINT n`. The
/// CREATE TABLE lines that [`dbine_driver::ddl`] writes as
/// `CONSTRAINT n CHECK (…)` are turned around.
pub fn informix_checks(ddl: &str) -> String {
    ddl.lines()
        .map(|l| {
            let indent = &l[..l.len() - l.trim_start().len()];
            let body = l.trim_start();
            let (body, comma) = match body.strip_suffix(',') {
                Some(b) => (b, ","),
                None => (body, ""),
            };
            match body.strip_prefix("CONSTRAINT ").and_then(|r| r.split_once(" CHECK ")) {
                Some((n, cond)) => format!("{indent}CHECK {cond} CONSTRAINT {n}{comma}"),
                None => l.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::IndexDef;

    fn rows(v: &[&[&str]]) -> Rows {
        v.iter().map(|r| r.iter().map(|c| Some(c.to_string())).collect()).collect()
    }

    #[test]
    fn checks_from_catalog_rows() {
        let mut t = vec![TableSchema { schema: Some("APP".into()), name: "DOCS".into(), ..Default::default() }];
        attach_checks(
            &mut t,
            &rows(&[
                &["APP", "DOCS", "CK_A", "CONSTRAINT CK_A CHECK (a > "],
                &["APP", "DOCS", "CK_A", "0)"],
                &["APP", "DOCS", "CK_B", "check(b < 5)"],
                &["APP", "DOCS", "", "CHECK (c IN (1, 2))"],
                &["APP", "OTRA", "CK_X", "(x > 0)"],
            ]),
            true,
        );
        let c = &t[0].checks;
        assert_eq!(c.len(), 3);
        assert_eq!((c[0].name.as_deref(), c[0].expression.as_str()), (Some("CK_A"), "(a > 0)"));
        assert_eq!(c[1].expression, "(b < 5)");
        assert_eq!((c[2].name.as_deref(), c[2].expression.as_str()), (None, "(c IN (1, 2))"));
        assert_eq!(check_condition("(fecha >= '2000-01-01')"), "(fecha >= '2000-01-01')");
    }

    #[test]
    fn include_columns_leave_the_key() {
        let ix = IndexDef { name: "UX".into(), unique: true, columns: vec!["A".into(), "B".into()], ..Default::default() };
        let mut t = vec![TableSchema { schema: Some("APP".into()), name: "T".into(), indexes: vec![ix], ..Default::default() }];
        attach_includes(&mut t, &rows(&[&["APP", "T", "UX", "B"]]));
        assert_eq!((t[0].indexes[0].columns.as_slice(), t[0].indexes[0].include.as_slice()), (&["A".to_string()][..], &["B".to_string()][..]));
    }

    #[test]
    fn db2_objects() {
        let q = Quote::Double;
        let seq = rows(&[&["BIGINT", "19", "100", "5", "1", "9223372036854775807", "N", "20", "N"]]);
        assert_eq!(
            build(Eng::Db2, kinds::SEQUENCE, q, Some("APP"), "SEQ", &[seq]).unwrap(),
            "CREATE SEQUENCE \"APP\".\"SEQ\" AS BIGINT START WITH 100 INCREMENT BY 5 MINVALUE 1 MAXVALUE 9223372036854775807 NO CYCLE CACHE 20 NO ORDER;"
        );
        let alias = rows(&[&["APP", "DOCS"]]);
        assert_eq!(build(Eng::Db2, kinds::SYNONYM, q, Some("APP"), "D", &[alias]).unwrap(), "CREATE ALIAS \"APP\".\"D\" FOR \"APP\".\"DOCS\";");
        let distinct = rows(&[&["T", "SYSIBM", "DECIMAL", "12", "2", "Y", "Y"]]);
        assert_eq!(
            build(Eng::Db2, kinds::TYPE, q, Some("APP"), "PRECIO", &[distinct]).unwrap(),
            "CREATE DISTINCT TYPE \"APP\".\"PRECIO\" AS DECIMAL(12,2) WITH COMPARISONS;"
        );
        let structured = vec![rows(&[&["R", "", "", "0", "0", "Y", "N"]]), rows(&[&["CALLE", "SYSIBM", "VARCHAR", "40", "0"], &["NRO", "SYSIBM", "INTEGER", "4", "0"]]), rows(&[&["APP", "BASE"]])];
        assert_eq!(
            build(Eng::Db2, kinds::TYPE, q, Some("APP"), "DIR", &structured).unwrap(),
            "CREATE TYPE \"APP\".\"DIR\" UNDER \"APP\".\"BASE\" AS (\n    \"CALLE\" VARCHAR(40),\n    \"NRO\" INTEGER\n) INSTANTIABLE NOT FINAL MODE DB2SQL;"
        );
        let zos = rows(&[&["484", "10", "1", "1", "1", "9999999999", "Y", "0", "Y"]]);
        assert_eq!(
            build(Eng::Db2zos, kinds::SEQUENCE, q, Some("A"), "S", &[zos]).unwrap(),
            "CREATE SEQUENCE \"A\".\"S\" AS DECIMAL(10,0) START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9999999999 CYCLE NO CACHE ORDER;"
        );
    }

    #[test]
    fn informix_objects_and_checks() {
        let q = Quote::Double;
        let seq = rows(&[&["", "", "1", "1", "1", "9223372036854775807", "0", "20", "0"]]);
        assert_eq!(
            build(Eng::Informix, kinds::SEQUENCE, q, Some("informix"), "s", &[seq]).unwrap(),
            "CREATE SEQUENCE \"informix\".\"s\" START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 NOCYCLE CACHE 20 NOORDER;"
        );
        let syn = rows(&[&["P", "", "", "informix", "docs"]]);
        assert_eq!(build(Eng::Informix, kinds::SYNONYM, q, Some("u"), "d", &[syn]).unwrap(), "CREATE PRIVATE SYNONYM \"u\".\"d\" FOR \"informix\".\"docs\";");
        let remote = rows(&[&["S", "srv", "otra", "informix", "docs"]]);
        assert_eq!(build(Eng::Informix, kinds::SYNONYM, q, None, "d", &[remote]).unwrap(), "CREATE PUBLIC SYNONYM \"d\" FOR otra@srv:\"informix\".\"docs\";");
        assert_eq!(
            informix_checks("CREATE TABLE \"t\" (\n    \"a\" INTEGER,\n    CONSTRAINT \"ck_a\" CHECK (a > 0),\n    CHECK (a < 9)\n);"),
            "CREATE TABLE \"t\" (\n    \"a\" INTEGER,\n    CHECK (a > 0) CONSTRAINT \"ck_a\",\n    CHECK (a < 9)\n);"
        );
    }

    #[test]
    fn dameng_altibase_cubrid_ingres() {
        let q = Quote::Double;
        let seq = rows(&[&["", "", "21", "1", "1", "9223372036854775807", "N", "20", "N"]]);
        assert_eq!(
            build(Eng::Dameng, kinds::SEQUENCE, q, Some("APP"), "S", &[seq]).unwrap(),
            "CREATE SEQUENCE \"APP\".\"S\" START WITH 21 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 NOCYCLE CACHE 20 NOORDER;"
        );
        assert_eq!(build(Eng::Dameng, kinds::SYNONYM, q, Some("APP"), "D", &[rows(&[&["APP", "DOCS"]])]).unwrap(), "CREATE SYNONYM \"APP\".\"D\" FOR \"APP\".\"DOCS\";");
        let src = rows(&[&["TYPE DIR AS OBJECT (\n"], &["  CALLE VARCHAR(40)\n"], &[")"]]);
        assert_eq!(build(Eng::Dameng, kinds::TYPE, q, Some("APP"), "DIR", &[src]).unwrap(), "CREATE OR REPLACE TYPE DIR AS OBJECT (\n  CALLE VARCHAR(40)\n);");
        let seq = rows(&[&["", "", "1", "2", "1", "100", "YES", "0", ""]]);
        assert_eq!(
            build(Eng::Altibase, kinds::SEQUENCE, q, Some("SYS"), "S", &[seq]).unwrap(),
            "CREATE SEQUENCE \"SYS\".\"S\" START WITH 1 INCREMENT BY 2 MINVALUE 1 MAXVALUE 100 CYCLE NOCACHE;"
        );
        let serial = rows(&[&["", "", "", "1", "1", "10000", "0", "10", ""]]);
        assert_eq!(build(Eng::Cubrid, kinds::SEQUENCE, q, None, "s", &[serial]).unwrap(), "CREATE SERIAL \"s\" INCREMENT BY 1 MINVALUE 1 MAXVALUE 10000 NOCYCLE CACHE 10;");
        assert_eq!(build(Eng::Cubrid, kinds::SYNONYM, q, None, "d", &[rows(&[&["DBA", "docs"]])]).unwrap(), "CREATE SYNONYM \"d\" FOR \"DBA\".\"docs\";");
        let seq = rows(&[&["decimal", "12", "1", "1", "1", "999999999999", "N", "20", "N"]]);
        assert_eq!(
            build(Eng::Ingres, kinds::SEQUENCE, q, Some("u"), "s", &[seq]).unwrap(),
            "CREATE SEQUENCE \"u\".\"s\" AS DECIMAL(12,0) START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 999999999999 NO CYCLE CACHE 20 NO ORDER;"
        );
        assert!(object_kinds(Eng::Ingres).iter().all(|k| k.id != kinds::TYPE));
    }

    #[test]
    fn mimer_objects_and_not_null_checks() {
        let q = Quote::Double;
        let seq = rows(&[&["YES", "", "1", "5", "", "1000", "", "", ""]]);
        assert_eq!(build(Eng::Mimer, kinds::SEQUENCE, q, Some("APP"), "S", &[seq]).unwrap(), "CREATE UNIQUE SEQUENCE \"APP\".\"S\" START WITH 1 INCREMENT BY 5 MAXVALUE 1000;");
        let dom = vec![rows(&[&["DECIMAL", "", "12", "2", "0"]]), rows(&[&["PRECIO_POS", "VALUE >= 0"]])];
        assert_eq!(
            build(Eng::Mimer, kinds::TYPE, q, Some("APP"), "PRECIO", &dom).unwrap(),
            "CREATE DOMAIN \"APP\".\"PRECIO\" AS DECIMAL(12,2) DEFAULT 0 CONSTRAINT \"PRECIO_POS\" CHECK (VALUE >= 0);"
        );
        let dom = vec![rows(&[&["CHARACTER VARYING", "40", "", "", ""]]), Vec::new()];
        assert_eq!(build(Eng::Mimer, kinds::TYPE, q, Some("APP"), "NOMBRE", &dom).unwrap(), "CREATE DOMAIN \"APP\".\"NOMBRE\" AS CHARACTER VARYING(40);");
        assert!(is_not_null_check("\"NOMBRE\" IS NOT NULL") && is_not_null_check("(ID IS NOT NULL)"));
        assert!(!is_not_null_check("a + b IS NOT NULL") && !is_not_null_check("a > 0"));
        let mut t = vec![TableSchema { schema: Some("APP".into()), name: "T".into(), ..Default::default() }];
        attach_checks(&mut t, &rows(&[&["APP", "T", "SYS_C1", "\"ID\" IS NOT NULL"], &["APP", "T", "CK_P", "P > 0"]]), true);
        assert_eq!(t[0].checks.len(), 1);
    }

    #[test]
    fn netezza_by_column_name() {
        let q = Quote::Double;
        let cols: Vec<String> = ["SEQNAME", "SCHEMA", "DATATYPE", "STARTVALUE", "INCREMENT", "MINVALUE", "MAXVALUE", "CYCLE"].map(String::from).to_vec();
        let row: Vec<Option<String>> = ["S", "ADMIN", "BIGINT", "1", "1", "1", "9223372036854775807", "f"].map(|v| Some(v.to_string())).to_vec();
        assert_eq!(
            build_named(Eng::Netezza, kinds::SEQUENCE, q, Some("ADMIN"), "S", &cols, &row).unwrap(),
            "CREATE SEQUENCE \"ADMIN\".\"S\" AS BIGINT START WITH 1 INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 NO CYCLE NO CACHE;"
        );
        assert!(build_named(Eng::Netezza, kinds::SEQUENCE, q, None, "S", &cols[..2], &row[..2]).is_none());
        let cols: Vec<String> = ["SYNONYM_NAME", "REFSCHEMA", "REFOBJNAME"].map(String::from).to_vec();
        let row = vec![Some("D".to_string()), Some("ADMIN".into()), Some("DOCS".into())];
        assert_eq!(build_named(Eng::Netezza, kinds::SYNONYM, q, Some("ADMIN"), "D", &cols, &row).unwrap(), "CREATE SYNONYM \"ADMIN\".\"D\" FOR \"ADMIN\".\"DOCS\";");
    }

    #[test]
    fn vertica_and_monetdb_sequences() {
        let q = Quote::Double;
        let v = rows(&[&["", "", "", "1", "1", "9223372036854775807", "f", "250000", ""]]);
        assert_eq!(
            build(Eng::Vertica, kinds::SEQUENCE, q, Some("public"), "s", &[v]).unwrap(),
            "CREATE SEQUENCE \"public\".\"s\" INCREMENT BY 1 MINVALUE 1 MAXVALUE 9223372036854775807 NO CYCLE CACHE 250000;"
        );
        let m = rows(&[&["", "", "10", "2", "0", "100", "true", "1", ""]]);
        assert_eq!(
            build(Eng::MonetDb, kinds::SEQUENCE, q, Some("sys"), "s", &[m]).unwrap(),
            "CREATE SEQUENCE \"sys\".\"s\" START WITH 10 INCREMENT BY 2 MINVALUE 0 MAXVALUE 100 CYCLE CACHE 1;"
        );
    }
}
