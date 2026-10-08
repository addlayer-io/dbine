use super::*;

fn pg() -> Profile {
    Profile::Sql(ScriptDialect::postgres(), Flavor::Postgres)
}
fn ms() -> Profile {
    Profile::Sql(ScriptDialect::tsql(), Flavor::Tsql)
}
fn my() -> Profile {
    Profile::Sql(ScriptDialect::mysql(), Flavor::Mysql)
}
fn ora() -> Profile {
    Profile::Sql(ScriptDialect::oracle(), Flavor::Oracle)
}
fn ansi() -> Profile {
    Profile::Sql(ScriptDialect::generic(), Flavor::Generic)
}
fn influx() -> Profile {
    Profile::Sql(ScriptDialect::generic(), Flavor::Influxql)
}
fn cql() -> Profile {
    Profile::Cql(ScriptDialect { dollar_quotes: true, backtick_idents: false, compound_blocks: false, ..ScriptDialect::generic() })
}

/// The rules `script` breaks.
fn rules(p: Profile, script: &str) -> Vec<&'static str> {
    lint(script, p).into_iter().map(|f| f.rule).collect()
}

/// `script` breaks `rule`, at the text `at` (its first occurrence).
#[track_caller]
fn hit(p: Profile, script: &str, rule: &str, at: &str) {
    let found = lint(script, p);
    let f = found.iter().find(|f| f.rule == rule).unwrap_or_else(|| panic!("{rule} not found in {script:?}: {found:?}"));
    assert_eq!(&script[f.start..f.end], at, "{rule} in {script:?}");
}

#[track_caller]
fn miss(p: Profile, script: &str, rule: &str) {
    let found = rules(p, script);
    assert!(!found.contains(&rule), "{rule} found in {script:?}: {found:?}");
}

#[test]
fn every_rule_has_a_unique_id() {
    let mut ids: Vec<_> = RULES.iter().map(|r| r.id).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), RULES.len());
}

#[test]
fn select_star() {
    hit(ansi(), "select * from t", "select-star", "*");
    hit(ms(), "SELECT TOP (10) * FROM t", "select-star", "*");
    hit(pg(), "select distinct * from t", "select-star", "*");
    miss(ansi(), "select count(*) from t", "select-star");
    miss(ansi(), "select a from t where exists (select * from u)", "select-star");
    miss(ansi(), "select a, b from t -- select * from t", "select-star");
    miss(ansi(), "select 'select * from t' from t", "select-star");
    miss(pg(), "select $$ select * from t $$", "select-star");
}

#[test]
fn dml_without_where() {
    hit(ansi(), "update t set a = 1", "dml-without-where", "update");
    hit(ansi(), "select 1;\ndelete from t", "dml-without-where", "delete");
    miss(ansi(), "delete from t where id = 1", "dml-without-where");
    miss(ansi(), "select 'delete from t'", "dml-without-where");
}

#[test]
fn not_in_subquery() {
    hit(ansi(), "select a from t where b not in (select c from u)", "not-in-subquery", "not in");
    miss(ansi(), "select a from t where b not in (1, 2)", "not-in-subquery");
    miss(ansi(), "select a from t where not exists (select 1 from u)", "not-in-subquery");
}

#[test]
fn leading_wildcard() {
    hit(ansi(), "select a from t where b like '%x'", "leading-wildcard", "'%x'");
    hit(ms(), "select a from t where b like N'%x'", "leading-wildcard", "'%x'");
    hit(ms(), "select a from t where b like '%' + @x", "leading-wildcard", "'%'");
    hit(my(), "select a from t where b like concat('%', ?)", "leading-wildcard", "'%'");
    hit(pg(), "select a from t where b ilike '%x%'", "leading-wildcard", "'%x%'");
    miss(ansi(), "select a from t where b like 'x%'", "leading-wildcard");
    miss(ansi(), "select 'like ''%x''' from t", "leading-wildcard");
}

#[test]
fn function_on_column() {
    hit(ms(), "select a from t where YEAR(created) = 2024", "function-on-column", "YEAR(created)");
    hit(ansi(), "select a from t where x = 1 and upper(t.name) = 'A'", "function-on-column", "upper(t.name)");
    hit(ms(), "select a from t where datepart(year, created) = 2024", "function-on-column", "datepart(year, created)");
    miss(ansi(), "select upper(a) from t where a = 'x'", "function-on-column");
    miss(ms(), "select a from t where a = upper(@x)", "function-on-column");
    miss(ms(), "select a from t where upper(@x) = a", "function-on-column");
    miss(ansi(), "select a from t where a = 1 order by upper(b) = 1", "function-on-column");
}

#[test]
fn equals_null() {
    hit(ansi(), "select a from t where b = null", "equals-null", "= null");
    hit(ansi(), "select a from t where b <> NULL", "equals-null", "<> NULL");
    hit(my(), "select a from t where x = 1 and b != null", "equals-null", "!= null");
    hit(ansi(), "select case when a = null then 1 end from t", "equals-null", "= null");
    miss(ansi(), "update t set a = null where b is null", "equals-null");
    miss(ms(), "create procedure p @x int = null as select 1", "equals-null");
    miss(ms(), "declare @x int = null", "equals-null");
    miss(ora(), "begin x := null; end;", "equals-null");
    miss(ansi(), "select a from t where b = 'null'", "equals-null");
}

#[test]
fn order_by_ordinal_and_random() {
    hit(ansi(), "select a, b from t order by 2 desc", "order-by-ordinal", "2");
    miss(ansi(), "select a, b from t order by b", "order-by-ordinal");
    miss(ansi(), "select a from t order by a limit 1", "order-by-ordinal");
    hit(my(), "select a from t order by rand() limit 1", "order-by-random", "order by rand()");
    hit(pg(), "select a from t order by random()", "order-by-random", "order by random()");
    hit(ms(), "select top 1 a from t order by newid()", "order-by-random", "order by newid()");
    miss(my(), "select rand() from t order by a", "order-by-random");
}

#[test]
fn implicit_cross_join() {
    hit(ansi(), "select * from a, b", "implicit-cross-join", ", b");
    hit(ansi(), "select * from a x, b y where x.id = 1", "implicit-cross-join", ", b y");
    miss(ansi(), "select * from a x, b y where x.id = y.a_id", "implicit-cross-join");
    miss(ansi(), "select * from a join b on a.id = b.a_id", "implicit-cross-join");
    miss(pg(), "select * from t, unnest(t.arr) u", "implicit-cross-join");
    miss(pg(), "select * from t, jsonb_array_elements(t.j) e", "implicit-cross-join");
    miss(ansi(), "select a from t where b in (1, 2)", "implicit-cross-join");
}

#[test]
fn insert_without_columns() {
    hit(ansi(), "insert into t values (1, 2)", "insert-without-columns", "insert into t");
    hit(ms(), "INSERT dbo.t SELECT * FROM u", "insert-without-columns", "INSERT dbo.t");
    hit(ms(), "insert into #tmp exec p", "insert-without-columns", "insert into #tmp");
    miss(ansi(), "insert into t (a, b) values (1, 2)", "insert-without-columns");
    miss(ms(), "insert into t default values", "insert-without-columns");
    miss(my(), "insert into t set a = 1", "insert-without-columns");
    miss(ora(), "insert all into t values (1) select 1 from dual", "insert-without-columns");
    miss(my(), "select insert('abc', 1, 1, 'x')", "insert-without-columns");
}

#[test]
fn distinct_with_group_by() {
    hit(ansi(), "select distinct a from t group by a", "distinct-group-by", "distinct");
    miss(ansi(), "select distinct a from t", "distinct-group-by");
    miss(ansi(), "select a, count(distinct b) from t group by a", "distinct-group-by");
}

#[test]
fn union_without_all() {
    hit(ansi(), "select a from t union select a from u", "union-distinct", "union");
    miss(ansi(), "select a from t union all select a from u", "union-distinct");
    miss(ansi(), "select a from t union distinct select a from u", "union-distinct");
}

#[test]
fn tsql_rules() {
    hit(ms(), "select a from t with (nolock)", "nolock", "nolock");
    hit(ms(), "select a from t (NOLOCK)", "nolock", "NOLOCK");
    hit(ms(), "set transaction isolation level read uncommitted", "nolock", "read uncommitted");
    miss(ms(), "select nolock from t", "nolock");
    miss(ms(), "select a from t -- with (nolock)", "nolock");

    hit(ms(), "declare c cursor for select a from t", "cursor", "cursor");
    hit(ms(), "declare c cursor local fast_forward for select a from t", "cursor", "cursor");
    miss(ms(), "select cursor from t", "cursor");

    hit(ms(), "set rowcount 10", "set-rowcount", "set rowcount");
    miss(ms(), "set rowcount 0", "set-rowcount");
    miss(ms(), "select @@rowcount", "set-rowcount");

    hit(ms(), "insert into t (a) values (1); select @@IDENTITY", "global-identity", "@@IDENTITY");
    miss(ms(), "select scope_identity()", "global-identity");
    miss(ms(), "select '@@identity'", "global-identity");

    hit(ms(), "create procedure dbo.sp_load as set nocount on; select 1", "sp-prefix", "sp_load");
    miss(ms(), "create procedure dbo.usp_load as set nocount on; select 1", "sp-prefix");
    miss(ms(), "exec sp_who", "sp-prefix");

    hit(ms(), "create or alter procedure dbo.p as\nbegin\n  select 1;\nend", "set-nocount", "create or alter procedure dbo.p");
    miss(ms(), "create procedure p as\nbegin\n  set nocount on;\n  select 1;\nend", "set-nocount");
    miss(ms(), "create view v as select 1 a", "set-nocount");
}

#[test]
fn for_update_without_nowait() {
    hit(pg(), "select a from t where id = 1 for update", "for-update-wait", "for update");
    hit(my(), "select a from t where id = 1 for share", "for-update-wait", "for share");
    hit(pg(), "select a from t for no key update", "for-update-wait", "for no key update");
    miss(pg(), "select a from t for update skip locked", "for-update-wait");
    miss(pg(), "select a from t for update nowait", "for-update-wait");
    miss(ora(), "select a from t for update wait 5", "for-update-wait");
    miss(pg(), "create policy p on t for update using (true)", "for-update-wait");
    miss(pg(), "create trigger x before update on t for each row execute function f()", "for-update-wait");
}

#[test]
fn serial_vs_identity() {
    hit(pg(), "create table t (id serial primary key, n text)", "serial-identity", "serial");
    hit(pg(), "alter table t add column big bigserial", "serial-identity", "bigserial");
    miss(pg(), "create table t (id int generated always as identity)", "serial-identity");
    miss(pg(), "select serial from t", "serial-identity");
    miss(pg(), "create table t (\"serial\" int)", "serial-identity");
}

#[test]
fn mysql_group_by() {
    hit(my(), "select a, b, count(*) from t group by a", "group-by-nonaggregated", "b");
    hit(my(), "select t.a, t.b from t group by t.a", "group-by-nonaggregated", "t.b");
    miss(my(), "select a, b, count(*) from t group by a, b", "group-by-nonaggregated");
    miss(my(), "select a, b, count(*) from t group by 1, 2", "group-by-nonaggregated");
    miss(my(), "select t.a as x, count(*) from t group by x", "group-by-nonaggregated");
    miss(my(), "select a, max(b) from t group by a", "group-by-nonaggregated");
    miss(my(), "select a, b from t", "group-by-nonaggregated");
}

#[test]
fn oracle_rules() {
    hit(ora(), "select a from t where rownum <= 10 order by a", "rownum-order-by", "rownum");
    miss(ora(), "select * from (select a from t order by a) where rownum <= 10", "rownum-order-by");
    hit(ora(), "select a.x from a, b where a.id = b.id(+)", "outer-join-plus", "(+)");
    miss(ora(), "select a.x from a left join b on a.id = b.id", "outer-join-plus");
    miss(ora(), "select '(+)' from dual", "outer-join-plus");
}

#[test]
fn influx_rules() {
    hit(influx(), "DELETE FROM cpu WHERE host = 'a'", "delete-without-time", "DELETE FROM cpu WHERE");
    miss(influx(), "DELETE FROM cpu WHERE time < '2024-01-01'", "delete-without-time");
    hit(influx(), "DROP SERIES FROM cpu WHERE host = 'a'", "drop-series", "DROP SERIES");
    hit(influx(), "drop measurement cpu", "drop-series", "drop measurement");
    miss(influx(), "SELECT * FROM cpu WHERE time > now() - 1h", "drop-series");
}

#[test]
fn cql_rules() {
    hit(cql(), "SELECT * FROM ks.users WHERE age > 30 ALLOW FILTERING", "allow-filtering", "ALLOW FILTERING");
    miss(cql(), "SELECT * FROM ks.users WHERE id = 1", "allow-filtering");
    hit(cql(), "SELECT name FROM ks.users", "no-partition-key", "SELECT name FROM ks");
    miss(cql(), "SELECT name FROM ks.users WHERE id = 1", "no-partition-key");
    miss(cql(), "SELECT * FROM system.local", "no-partition-key");
    hit(
        cql(),
        "BEGIN BATCH\n  INSERT INTO a (id, v) VALUES (1, 'x');\n  INSERT INTO b (id, v) VALUES (1, 'x');\nAPPLY BATCH;",
        "batch-partitions",
        "BEGIN BATCH",
    );
    hit(
        cql(),
        "BEGIN UNLOGGED BATCH\n  UPDATE a SET v = 1 WHERE id = 1;\n  UPDATE a SET v = 2 WHERE id = 2;\nAPPLY BATCH;",
        "batch-partitions",
        "BEGIN UNLOGGED BATCH",
    );
    miss(
        cql(),
        "BEGIN BATCH\n  INSERT INTO a (id, c, v) VALUES (1, 1, 'x');\n  UPDATE a SET v = 'y' WHERE id = 1 AND c = 2;\nAPPLY BATCH;",
        "batch-partitions",
    );
    hit(cql(), "SELECT * FROM t WHERE id = 1", "select-star", "*");
}

#[test]
fn mongodb_rules() {
    let m = Profile::Mongodb;
    hit(m, "db.users.deleteMany({})", "write-all", "deleteMany({})");
    hit(m, "db.users.updateMany({ }, { $set: { a: 1 } })", "write-all", "updateMany({ }, { $set: { a: 1 } })");
    miss(m, "db.users.deleteMany({ a: 1 })", "write-all");
    miss(m, "db.users.updateMany({ a: 1 }, { $set: { b: 2 } })", "write-all");
    miss(m, "// db.users.deleteMany({})\ndb.users.find({ a: 1 })", "write-all");
    miss(m, "db.users.insertOne({ note: 'deleteMany({})' })", "write-all");

    hit(m, "db.users.find({ $where: 'this.a > 1' })", "where-operator", "$where");
    hit(m, "db.users.find({ \"$where\": \"this.a > 1\" })", "where-operator", "\"$where\"");
    miss(m, "db.users.find({ note: '$where' })", "where-operator");

    hit(m, "db.users.find({ name: /abc/i })", "unanchored-regex", "/abc/i");
    hit(m, "db.users.find({ name: { $regex: 'abc' } })", "unanchored-regex", "'abc'");
    miss(m, "db.users.find({ name: /^abc/ })", "unanchored-regex");
    miss(m, "db.users.find({ name: { $regex: \"^abc\" } })", "unanchored-regex");
    miss(m, "db.users.find({ n: 'a/b/c' })", "unanchored-regex");

    hit(m, "db.users.find()", "read-all", "find()");
    hit(m, "db.users.find({}).limit(5)", "read-all", "find({})");
    miss(m, "db.users.find({ a: 1 })", "read-all");
}

#[test]
fn couchdb_rules() {
    let c = Profile::Couchdb;
    hit(c, "{\"selector\": {}, \"limit\": 10}", "read-all", "\"selector\": {}");
    miss(c, "{\"selector\": {\"a\": 1}}", "read-all");
    hit(c, "{\"selector\": {\"name\": {\"$regex\": \"abc\"}}}", "unanchored-regex", "\"abc\"");
    miss(c, "{\"selector\": {\"name\": {\"$regex\": \"^abc\"}}}", "unanchored-regex");
    miss(c, "# {\"selector\": {}}\nGET /db/_all_docs", "read-all");
}

#[test]
fn search_rules() {
    let s = Profile::Search;
    hit(s, "POST /logs/_delete_by_query\n{ \"query\": { \"match_all\": {} } }", "write-all", "POST /logs/_delete_by_query");
    hit(s, "POST /logs/_delete_by_query\n{}", "write-all", "POST /logs/_delete_by_query");
    miss(s, "POST /logs/_delete_by_query\n{ \"query\": { \"term\": { \"a\": 1 } } }", "write-all");
    miss(s, "GET /logs/_search\n{ \"query\": { \"match_all\": {} } }", "write-all");
    hit(s, "POST /solr/core/update\n{ \"delete\": { \"query\": \"*:*\" } }", "write-all", "POST /solr/core/update");
    miss(s, "POST /solr/core/update\n{ \"delete\": { \"query\": \"id:1\" } }", "write-all");

    hit(s, "GET /i/_search\n{ \"query\": { \"wildcard\": { \"name\": \"*abc\" } } }", "leading-wildcard", "\"*abc\"");
    hit(s, "GET /i/_search\n{ \"query\": { \"wildcard\": { \"name\": { \"value\": \"?bc\" } } } }", "leading-wildcard", "\"?bc\"");
    hit(s, "GET /i/_search\n{ \"query\": { \"query_string\": { \"query\": \"name:*abc\" } } }", "leading-wildcard", "\"name:*abc\"");
    hit(s, "GET /solr/core/select?q=name:*abc", "leading-wildcard", "GET /solr/core/select?q=name:*abc");
    miss(s, "GET /solr/core/select?q=*:*", "leading-wildcard");
    miss(s, "GET /i/_search\n{ \"query\": { \"wildcard\": { \"name\": \"abc*\" } } }", "leading-wildcard");
    miss(s, "# GET /solr/core/select?q=name:*abc\nGET /i/_count", "leading-wildcard");
}

#[test]
fn redis_rules() {
    let r = Profile::Redis;
    hit(r, "KEYS *", "keys-command", "KEYS *");
    hit(r, "get a\nkeys user:*", "keys-command", "keys user:*");
    miss(r, "SCAN 0 MATCH user:* COUNT 100", "keys-command");
    miss(r, "GET keys", "keys-command");
    miss(r, "# KEYS *\nGET a", "keys-command");
    hit(r, "FLUSHALL", "flush", "FLUSHALL");
    hit(r, "flushdb async", "flush", "flushdb async");
    miss(r, "SET flushall 1", "flush");
    hit(r, "HGETALL user:1", "big-read", "HGETALL user:1");
    hit(r, "SMEMBERS tags", "big-read", "SMEMBERS tags");
    hit(r, "LRANGE q 0 -1", "big-read", "LRANGE q 0 -1");
    miss(r, "LRANGE q 0 9", "big-read");
    miss(r, "HGET user:1 name", "big-read");
}

#[test]
fn etcd_rules() {
    let e = Profile::Etcd;
    hit(e, "del \"\" --prefix", "write-all", "del \"\" --prefix");
    hit(e, "del --from-key '\\0'", "write-all", "del --from-key '\\0'");
    miss(e, "del /app/ --prefix", "write-all");
    hit(e, "get \"\" --prefix", "read-all", "get \"\" --prefix");
    miss(e, "get \"\" --prefix --limit=100", "read-all");
    miss(e, "get /app/x", "read-all");
}

#[test]
fn cypher_rules() {
    let c = Profile::Cypher;
    hit(c, "MATCH (n) WHERE n.name = 'x' RETURN n", "match-without-label", "(n)");
    miss(c, "MATCH (n:Person) WHERE n.name = 'x' RETURN n", "match-without-label");
    miss(c, "MATCH (n {name: 'x'}) RETURN n", "match-without-label");
    miss(c, "MATCH (a:Person) MATCH (a)-->(b) RETURN b", "match-without-label");
    miss(c, "MATCH ()-[r:KNOWS]->() RETURN count(r)", "match-without-label");
    miss(c, "// MATCH (n) RETURN n\nMATCH (n:A) RETURN n", "match-without-label");

    hit(c, "MATCH (a:A), (b:B) RETURN a, b", "cartesian-product", "(b:B)");
    miss(c, "MATCH (a:A), (b:B), (a)-->(b) RETURN a, b", "cartesian-product");
    miss(c, "MATCH (a:A)-->(b:B) RETURN a, b", "cartesian-product");

    hit(c, "MATCH (n) DETACH DELETE n", "detach-delete-all", "DETACH DELETE");
    hit(c, "MATCH (n:Temp) DETACH DELETE n", "detach-delete-all", "DETACH DELETE");
    miss(c, "MATCH (n:Temp) WHERE n.at < 5 DETACH DELETE n", "detach-delete-all");
    miss(c, "MATCH (n:Temp {id: 1}) DETACH DELETE n", "detach-delete-all");
    miss(c, "RETURN 'MATCH (n) DETACH DELETE n'", "detach-delete-all");
}

#[test]
fn flux_has_no_rules() {
    assert!(lint("from(bucket: \"b\") |> range(start: 0)", Profile::None).is_empty());
}

/// A 5,000-line script lints in under 50 ms (a release build; a debug
/// build gets more room).
#[test]
fn big_scripts_are_fast() {
    let unit = "SELECT a.id, upper(b.name) FROM a JOIN b ON a.id = b.a_id WHERE a.x = 'ñ%' AND b.y LIKE 'z%' ORDER BY 1;\n\
                UPDATE t SET v = NULL WHERE id IN (SELECT id FROM u WHERE w = 1);\n\
                INSERT INTO t (a, b) VALUES (1, 'x');\n\
                -- a comment\n\
                DELETE FROM t WHERE id = 2;\n";
    let script = unit.repeat(1000);
    assert_eq!(script.lines().count(), 5000);
    let limit = if cfg!(debug_assertions) { 500 } else { 50 };
    for p in [pg(), ms(), my(), ora()] {
        let started = std::time::Instant::now();
        let found = lint(&script, p);
        let took = started.elapsed();
        assert!(!found.is_empty());
        assert!(took.as_millis() < limit, "{p:?}: {took:?}");
    }
    let mongo = "db.users.find({ name: /abc/ }).limit(5)\n".repeat(5000);
    let started = std::time::Instant::now();
    lint(&mongo, Profile::Mongodb);
    assert!(started.elapsed().as_millis() < limit);
}
