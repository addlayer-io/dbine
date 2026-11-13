// Tests for the engine's functions in completion (functionCompletion.ts):
// where each kind is offered, in each language, and what picking one
// inserts. Run: `node --test src/composables/functionCompletion.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// functionCompletion.ts imports its siblings without an extension (as Vite
// does): resolve them to the .ts files.
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { EditorState } = await import('@codemirror/state');
const { CompletionContext } = await import('@codemirror/autocomplete');
const { StandardSQL, MSSQL, PostgreSQL } = await import('@codemirror/lang-sql');
const { json } = await import('@codemirror/lang-json');
const { ensureSyntaxTree } = await import('@codemirror/language');
const {
  functionCompletion, functionFlavor, functionIndex, matchShown, scanCode, withoutFunctionNames, exprSlot, tableSlotKind, offersAny, LOOK_BACK,
} = await import('./functionCompletion.ts');
const { qCompletion } = await import('./qLanguage.ts');

const F = (name, kind, extra = {}) => ({ name, kind, args: '', returns: '', description: '', schema: '', ...extra });
const overloads = (n) => `+${n} overloads`;

/** `‸` marks the cursor (`|` is Flux's pipe). */
function ctxOf(text, { lang = StandardSQL.language, explicit = false } = {}) {
  const pos = text.indexOf('‸');
  assert.ok(pos >= 0, 'no cursor');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const state = EditorState.create({ doc, extensions: lang ? [lang] : [] });
  return new CompletionContext(state, pos, explicit);
}
const labels = (r) => (r ? r.options.map((o) => o.label) : []);

// -- SQL ---------------------------------------------------------------------------------------
const SQL = functionIndex([
  F('count', 'aggregate', { args: '(expr)', returns: 'bigint', description: 'Number of input rows.' }),
  F('upper', 'scalar', { args: '(text)', returns: 'text' }),
  F('round', 'scalar', { args: '(x)' }),
  F('round', 'scalar', { args: '(x, digits)' }),
  F('round', 'scalar', { args: '(x numeric, s int)', description: 'Rounds to s decimal places.' }),
  F('left', 'scalar', { args: '(str, n)' }),
  F('row_number', 'window', { args: '()', returns: 'bigint' }),
  F('generate_series', 'table', { args: '(start, stop)' }),
  F('total_sales', 'scalar', { args: '(id integer)', returns: 'numeric', schema: 'sales' }),
  F('monthly', 'table', { args: '(m int)', schema: 'sales' }),
], overloads);
const sql = (text, o = {}) => functionCompletion(ctxOf(text, o), SQL, 'sql');
const EXPR = ['count', 'left', 'round', 'row_number', 'total_sales', 'upper'];

test('each language takes the catalog its way; q merges it into its own completion', () => {
  assert.equal(functionFlavor('sql', 'postgres'), 'sql');
  assert.equal(functionFlavor('cql', 'cql'), 'sql');
  assert.equal(functionFlavor(undefined, ''), 'sql');
  assert.equal(functionFlavor('cypher', 'neo4j'), 'cypher');
  assert.equal(functionFlavor('flux', ''), 'flux');
  assert.equal(functionFlavor('json', ''), 'json');
  assert.equal(functionFlavor('redis', ''), 'commands');
  assert.equal(functionFlavor('redis', 'etcd'), 'commands');
  assert.equal(functionFlavor('redis', 'q'), null);
});

test('an entry shows its arguments and type; overloads are one entry, with their count', () => {
  const r = sql('SELECT ‸', { explicit: true });
  const by = (l) => r.options.find((o) => o.label === l);
  assert.equal(by('count').detail, '(expr) → bigint');
  assert.equal(by('count').info, 'Number of input rows.');
  assert.equal(by('count').type, 'function');
  assert.equal(by('upper').info, undefined);
  assert.equal(r.options.filter((o) => o.label === 'round').length, 1);
  assert.equal(by('round').detail, '(x)');
  assert.equal(by('round').info, 'Rounds to s decimal places.\n+2 overloads');
  // Above the keywords (CodeEditor ranks them at -2).
  assert.ok(r.options.every((o) => o.boost > -2));
});

test('scalar, aggregate and window functions where an expression starts', () => {
  for (const text of [
    'SELECT co‸', 'SELECT DISTINCT co‸', 'SELECT a, co‸', 'SELECT a,\n  co‸', 'SELECT * FROM t WHERE co‸', 'SELECT * FROM t WHERE a = co‸',
    'SELECT * FROM t WHERE a > 1 AND co‸', 'SELECT * FROM t WHERE a OR NOT co‸', 'SELECT * FROM a JOIN b ON co‸', 'SELECT 1 FROM t GROUP BY co‸',
    'SELECT 1 FROM t ORDER BY a, co‸', 'SELECT CASE WHEN a THEN co‸', 'SELECT CASE WHEN co‸', 'SELECT 1 FROM t HAVING co‸', 'SELECT (co‸',
    'SELECT upper(co‸', 'SELECT round(a, co‸', 'SELECT a + co‸', 'SELECT a * co‸', 'SELECT a || co‸', 'INSERT INTO t VALUES (co‸',
    'INSERT INTO t VALUES (1, co‸', 'UPDATE t SET a = co‸', 'SELECT * FROM t WHERE a IN (co‸', 'SELECT * FROM t WHERE a BETWEEN co‸',
    'SELECT * FROM t WHERE a LIKE co‸', 'DELETE FROM t RETURNING co‸', 'CREATE TABLE t (a int DEFAULT co‸', 'SELECT 1;\nSELECT co‸',
    'SELECT * FROM t WHERE a = 1 -- note\n  AND co‸', "SELECT 'x', co‸",
    // A window's PARTITION BY and ORDER BY items are expressions.
    'SELECT row_number() OVER (PARTITION BY a, co‸', 'SELECT row_number() OVER (ORDER BY a, co‸', 'SELECT row_number() OVER (PARTITION BY co‸',
  ]) {
    assert.deepEqual(labels(sql(text)).sort(), EXPR, text);
  }
  // T-SQL: after TOP n.
  assert.deepEqual(labels(sql('SELECT TOP 10 co‸', { lang: MSSQL.language })).sort(), EXPR);
  assert.deepEqual(labels(sql('SELECT TOP (5) PERCENT co‸', { lang: MSSQL.language })).sort(), EXPR);
  // ⌃Space with nothing typed.
  assert.deepEqual(labels(sql('SELECT ‸', { explicit: true })).sort(), EXPR);
  assert.deepEqual(labels(sql('SELECT row_number() OVER (ORDER BY a, ‸', { explicit: true })).sort(), EXPR);
});

test('…and nowhere else: names, clause words, strings, comments, after a dot', () => {
  for (const text of [
    // A statement's first word, after a whole statement, and the list not asked for.
    'co‸', 'SELECT 1;\nco‸', 'SELECT ‸',
    // Where a name is given or a clause keyword goes.
    'SELECT a co‸', 'SELECT a AS co‸', 'SELECT count(*) co‸', 'SELECT * FROM t co‸', 'SELECT * FROM t WHERE a co‸', 'SELECT 1 co‸',
    'SELECT * FROM t WHERE a IS NOT co‸', 'SELECT * co‸', 'SELECT t.* co‸', 'WITH co‸', 'UPDATE t SET co‸', 'EXEC co‸', 'CALL co‸',
    // Lists of names: columns or parameters being defined, a join's columns, a window.
    'CREATE TABLE t (co‸', 'CREATE TABLE IF NOT EXISTS t (id int, co‸', 'INSERT INTO t (a, co‸', 'SELECT * FROM a JOIN b USING (co‸',
    'SELECT row_number() OVER (co‸', 'CREATE FUNCTION f(a int, co‸', 'ALTER TABLE t ADD PRIMARY KEY (co‸',
    // Strings and comments.
    "SELECT 'co‸'", 'SELECT "co‸"', '-- co‸', 'SELECT 1 /* co‸ */', "SELECT 'it''s co‸'",
    // After a dot (a column, a table), a variable, a bind, a cast.
    'SELECT t.co‸', 'SELECT t.‸', 'SELECT @co‸', 'SELECT :co‸', 'SELECT a::te‸', 'SELECT 1.5‸',
  ]) {
    assert.equal(sql(text, { explicit: text === 'SELECT t.‸' }), null, text);
  }
});

test('table functions where a table goes, never where only a name goes', () => {
  for (const text of ['SELECT * FROM ge‸', 'SELECT * FROM a JOIN ge‸', 'SELECT * FROM a, ge‸', 'SELECT * FROM a CROSS JOIN LATERAL ge‸', 'SELECT * FROM a CROSS APPLY ge‸', 'SELECT * FROM TABLE(ge‸']) {
    assert.deepEqual(labels(sql(text)).sort(), ['generate_series', 'monthly'], text);
  }
  for (const text of ['INSERT INTO ge‸', 'UPDATE ge‸', 'TRUNCATE TABLE ge‸']) assert.equal(sql(text), null, text);
  assert.equal(tableSlotKind('SELECT * FROM '), 'function');
  assert.equal(tableSlotKind('INSERT INTO '), 'name');
  assert.equal(tableSlotKind('SELECT '), null);
});

test("a schema's functions after its name and a dot, and by their name anywhere, written whole", () => {
  assert.deepEqual(labels(sql('SELECT sales.to‸')), ['total_sales']);
  assert.deepEqual(labels(sql('SELECT sales.‸')), ['total_sales']);
  assert.deepEqual(labels(sql('SELECT "sales".‸')), ['total_sales']);
  assert.deepEqual(labels(sql('SELECT db.sales.‸')), ['total_sales']);
  assert.deepEqual(labels(sql('SELECT * FROM sales.‸')), ['monthly']);
  assert.equal(sql('SELECT other.‸'), null);
  assert.equal(sql('INSERT INTO sales.‸'), null);
  const r = sql('SELECT tot‸');
  const o = r.options.find((x) => x.label === 'total_sales');
  assert.equal(o.displayLabel, 'sales.total_sales');
  // What's highlighted is the name, at the end of what's shown.
  assert.deepEqual(matchShown(o, [0, 3]), [6, 9]);
  assert.deepEqual(matchShown(r.options.find((x) => x.label === 'upper'), [0, 2]), [0, 2]);
});

/** Pick `label` at the cursor: the text and the cursor after it. */
function pick(r, label, text) {
  const pos = text.indexOf('‸');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  let state = EditorState.create({ doc });
  const view = { get state() { return state; }, dispatch(spec) { state = state.update(spec).state; } };
  const o = r.options.find((x) => x.label === label);
  o.apply(view, o, r.from, pos);
  const at = state.selection.main.head;
  const out = state.doc.toString();
  return out.slice(0, at) + '‸' + out.slice(at);
}

test('picking a function inserts its call with the cursor inside', () => {
  assert.equal(pick(sql('SELECT up‸'), 'upper', 'SELECT up‸'), 'SELECT upper(‸)');
  assert.equal(pick(sql('SELECT up‸(a)'), 'upper', 'SELECT up‸(a)'), 'SELECT upper(‸a)');
  assert.equal(pick(sql('SELECT tot‸'), 'total_sales', 'SELECT tot‸'), 'SELECT sales.total_sales(‸)');
  assert.equal(pick(sql('SELECT sales.to‸'), 'total_sales', 'SELECT sales.to‸'), 'SELECT sales.total_sales(‸)');
});

test("keywords a function the list offers stands for are left out of it", () => {
  const keywords = { from: 7, options: [{ label: 'LEFT' }, { label: 'SELECT' }, { label: 'ROUND' }, { label: 'TOTAL_SALES' }] };
  const kept = withoutFunctionNames(keywords, sql('SELECT le‸'));
  assert.deepEqual(labels(kept), ['SELECT', 'TOTAL_SALES']);
  // Where no function is offered (LEFT JOIN after a table), every keyword stays.
  assert.equal(withoutFunctionNames(keywords, sql('SELECT * FROM t le‸')), keywords);
});

test('a catalog without entries (an engine without one) offers nothing', () => {
  const none = functionIndex([], overloads);
  for (const [text, flavor] of [['SELECT co‸', 'sql'], ['RETURN co‸', 'cypher'], ['fr‸', 'flux'], ['{ "$ma‸', 'json'], ['HS‸', 'commands']]) {
    assert.equal(functionCompletion(ctxOf(text, { lang: null }), none, flavor), null, text);
  }
});

// -- Cypher -----------------------------------------------------------------------------------
const CYPHER = functionIndex([
  F('count', 'aggregate', { args: '(input :: ANY)', returns: 'INTEGER', description: 'Returns the number of values or rows.' }),
  F('toUpper', 'scalar', { args: '(input :: STRING)', returns: 'STRING' }),
  F('apoc.coll.sum', 'scalar', { args: '(coll :: LIST<NUMBER>)', returns: 'FLOAT' }),
  F('date.truncate', 'scalar', { args: '(unit, input)' }),
  F('date', 'scalar', { args: '(input = DEFAULT_TEMPORAL_ARGUMENT)' }),
], overloads);
const cypher = (text, explicit = false) => functionCompletion(ctxOf(text, { lang: null, explicit }), CYPHER, 'cypher');

test('Cypher: functions where an expression starts, by the same rules, with a lexical check', () => {
  for (const text of [
    'MATCH (n) RETURN co‸', 'MATCH (n) WHERE co‸', 'MATCH (n) WITH n, co‸', 'MATCH (n) RETURN n.a + co‸', 'MATCH (n) RETURN toUpper(co‸',
    'MATCH (n) RETURN {total: co‸', 'UNWIND co‸', 'MATCH (n) RETURN CASE WHEN co‸', 'MATCH (n) WHERE n.a IN co‸', 'MATCH (n) RETURN (co‸',
    // A list, an index, a comparison: expressions.
    'MATCH (n) RETURN [co‸', 'MATCH (n) RETURN n.list[co‸', 'MATCH (n) WHERE n.a = (co‸',
  ]) {
    assert.ok(labels(cypher(text)).includes('count'), text);
  }
  for (const text of [
    'co‸', 'MATCH (co‸', 'MATCH (n:Co‸', 'MATCH (n)-[:CO‸', 'MATCH (n)-->(co‸', 'MATCH (n) RETURN n.co‸', 'MATCH (n) RETURN $co‸',
    "MATCH (n) RETURN 'co‸'", 'MATCH (n) RETURN "co‸"', 'MATCH (n) RETURN `co‸`', '// co‸', 'MATCH (n) /* co‸', 'MATCH (n) RETURN {co‸',
    'MATCH (n) RETURN n co‸', 'MATCH (n) RETURN 1;\nco‸',
    // A relationship's variable, a path's.
    'MATCH (a)-[re‸', 'MATCH (a)<-[re‸', 'MATCH (a) - [re‸', 'MATCH p = (re‸', 'OPTIONAL MATCH p=(re‸', 'MERGE p = (re‸', 'MATCH p = (a)-[re‸',
  ]) {
    assert.equal(cypher(text), null, text);
  }
});

test('Cypher: namespaced functions after their namespace, and whole by their name', () => {
  assert.deepEqual(labels(cypher('RETURN apoc.‸')), ['coll.sum']);
  assert.deepEqual(labels(cypher('RETURN apoc.coll.‸')), ['sum']);
  assert.deepEqual(labels(cypher('RETURN date.‸')), ['truncate']);
  const r = cypher('RETURN su‸');
  assert.equal(r.options.find((o) => o.label === 'sum').displayLabel, 'apoc.coll.sum');
  assert.equal(pick(r, 'sum', 'RETURN su‸'), 'RETURN apoc.coll.sum(‸)');
  assert.equal(pick(cypher('RETURN apoc.coll.‸'), 'sum', 'RETURN apoc.coll.‸'), 'RETURN apoc.coll.sum(‸)');
});

// -- Flux -------------------------------------------------------------------------------------
const FLUX = functionIndex([
  F('from', 'scalar', { args: '(bucket, host, org, token)' }),
  F('filter', 'scalar', { args: '(<-tables, fn, onEmpty)' }),
  F('now', 'scalar', { args: '()' }),
  F('toUpper', 'scalar', { args: '(v)', schema: 'strings' }),
  F('measurements', 'scalar', { args: '(bucket)', schema: 'influxdata/influxdb/schema' }),
], overloads);
const flux = (text, explicit = false) => functionCompletion(ctxOf(text, { lang: null, explicit }), FLUX, 'flux');

test('Flux: a statement, a pipe or a value starts with a function; a parameter name never does', () => {
  for (const text of [
    'fr‸', 'x = 1\nfr‸', 'from(bucket: "b")\n  |> fil‸', 'from(bucket: "b") |> fil‸', 'from(bucket: "b") |> range(start: no‸',
    'data = fr‸', 'filter(fn: (r) => r._value > no‸', 'f = () => no‸', 'x = [no‸', 'r = {r with _value: no‸', 'return no‸',
    // Over several lines: a pipe, a value, a function's body, a list's item.
    'from(bucket: "b")\n  |> range(start: -1h)\n  |> fil‸', 'range(\n  start: no‸', 'filter(fn: (r) =>\n  no‸',
    'f = (x) => {\n  no‸', 'f = (x) => {\n  y = 1\n  no‸', 'x = [\n  no‸', 'x = [1,\n  no‸', 'from(bucket: "b")\n\nfr‸',
  ]) {
    assert.ok(labels(flux(text)).includes('now'), text);
  }
  for (const text of [
    'range(st‸', 'range(start: -1h, st‸', 'filter(fn: (r‸', '{ _va‸', 'r._va‸', 'x = "no‸', '// no‸', 'x = 1 no‸', 'x = /no‸/',
    // A parameter name or a key on its own line.
    'range(\n  st‸', '|> range(start: -1h,\n  st‸', 'aggregateWindow(\n  every: 1m,\n  st‸', 'map(fn: (r) => ({\n r with\n _value: x,\n no‸',
    'r = {\n  _va‸', 'f(\n  a: 1,\n\n  st‸',
  ]) {
    assert.equal(flux(text), null, text);
  }
});

test("Flux: a package's functions after its name (an import path's last part), whole by their name", () => {
  assert.deepEqual(labels(flux('strings.toU‸')), ['toUpper']);
  assert.deepEqual(labels(flux('x = schema.‸')), ['measurements']);
  const r = flux('toU‸');
  assert.equal(r.options.find((o) => o.label === 'toUpper').displayLabel, 'strings.toUpper');
  assert.equal(pick(r, 'toUpper', 'x = toU‸'.replace('x = ', '')), 'strings.toUpper(‸)');
});

// -- Redis, etcd -----------------------------------------------------------------------------
const REDIS = functionIndex([
  F('GET', 'command', { args: 'key', description: 'Returns the string value of a key.' }),
  F('HSET', 'command', { args: 'key field value [field value ...]' }),
  F('CLIENT KILL', 'command', { args: '[ID client-id]' }),
  F('CLIENT LIST', 'command'),
  F('JSON.SET', 'command', { args: 'key path value' }),
], overloads);
const redis = (text, explicit = false) => functionCompletion(ctxOf(text, { lang: null, explicit }), REDIS, 'commands');

test('Redis and etcd: commands as the first word of a line, subcommands after their container', () => {
  assert.deepEqual(labels(redis('HS‸')).sort(), ['CLIENT KILL', 'CLIENT LIST', 'GET', 'HSET', 'JSON.SET']);
  assert.equal(redis('  hs‸').from, 2);
  assert.equal(redis('GET a\nhs‸').from, 6);
  assert.deepEqual(labels(redis('CLIENT K‸')), ['CLIENT KILL', 'CLIENT LIST']);
  assert.equal(redis('client ‸').from, 0);
  assert.ok(redis('‸', true));
  for (const text of ['‸', 'SET key HS‸', 'GET ‸', '# HS‸', 'SET "a b" HS‸']) assert.equal(redis(text), null, text);
  const r = redis('h‸');
  assert.equal(r.options.find((o) => o.label === 'GET').info, 'Returns the string value of a key.');
  assert.equal(pick(r, 'HSET', 'h‸'), 'HSET ‸');
  assert.equal(pick(redis('h‸ k'), 'HSET', 'h‸ k'), 'HSET ‸k');
  assert.equal(pick(redis('JSON.‸'), 'JSON.SET', 'JSON.‸'), 'JSON.SET ‸');
});

// -- JSON --------------------------------------------------------------------------------------
const MONGO = functionIndex([
  F('$match', 'operator'), F('$sum', 'operator'), F('$dateTrunc', 'operator'), F('$gt', 'operator'),
  F('find', 'scalar', { args: '(filter, projection)' }), F('sort', 'scalar', { args: '(spec)' }),
  F('createCollection', 'scalar', { args: '(name, options)', schema: 'db' }),
], overloads);
const SOLR = functionIndex([F('sum', 'scalar', { args: '(x, y, ...)' }), F('div', 'scalar', { args: '(x, y)' })], overloads);
const jsonAt = (index, text, explicit = false) => functionCompletion(ctxOf(text, { lang: json().language, explicit }), index, 'json');

test('JSON: operators at a key that starts with $, quoted or bare; never at a $ value', () => {
  const ops = ['$dateTrunc', '$gt', '$match', '$sum'];
  let r = jsonAt(MONGO, '[{ "$ma‸');
  assert.deepEqual(labels(r).sort(), ops);
  assert.equal(r.from, 4);
  r = jsonAt(MONGO, 'db.orders.aggregate([{ $ma‸');
  assert.deepEqual(labels(r).sort(), ops);
  assert.equal(r.from, 23);
  for (const text of [
    "db.orders.find({ total: { '$g‸", '[{ "$ma‸": {} }]', '{ "a": 1, "$o‸', '{ a: 1, $o‸', '{ $ma‸: {} }', '{\n  "$ma‸', '[{ $match: {} }, { $s‸',
  ]) {
    assert.deepEqual(labels(jsonAt(MONGO, text)).sort(), ops, text);
  }
  for (const text of [
    '{ "name": "jo‸', '{ "na‸', '{ na‸', '// $ma‸', 'db.orders.find({ name: /$ma‸', '{ "a$ma‸', '{ x$ma‸',
    // A field path or a variable, as a value or a list's item.
    '[{ "$match": { "total": "$su‸" } }]', '{ $sum: "$pri‸', '{ _id: "$cat‸', "{ _id: '$cat‸", '{ $concat: ["$a", "$b‸', '{ $sum: $pr‸',
  ]) {
    assert.equal(jsonAt(MONGO, text), null, text);
  }
});

test("JSON: a shell's methods after a dot that follows a name, ) or ], outside strings", () => {
  assert.deepEqual(labels(jsonAt(MONGO, 'db.orders.fi‸')).sort(), ['createCollection', 'find', 'sort']);
  assert.deepEqual(labels(jsonAt(MONGO, 'db.orders.find({}).so‸')).sort(), ['createCollection', 'find', 'sort']);
  assert.deepEqual(labels(jsonAt(MONGO, 'db.orders.find({}).‸')).sort(), ['createCollection', 'find', 'sort']);
  assert.deepEqual(labels(jsonAt(MONGO, 'db.‸')), ['createCollection']);
  assert.equal(pick(jsonAt(MONGO, 'db.orders.fi‸'), 'find', 'db.orders.fi‸'), 'db.orders.find(‸)');
  // A number's decimal point.
  for (const text of ['{ "a": 1.5‸', '{ "$gt": 1.‸', '{ "$gt": 10.‸', '[1.‸', '1.‸', '{ "a": -2.‸']) {
    assert.equal(jsonAt(MONGO, text), null, text);
    assert.equal(jsonAt(SOLR, text), null, text);
  }
  assert.equal(jsonAt(SOLR, '{ "rows": 1.‸'), null);
  // A string's dot isn't a name's.
  assert.equal(jsonAt(MONGO, '{ "a": "x".‸'), null);
});

test('JSON: function queries in string values when the catalog has no operators', () => {
  assert.deepEqual(labels(jsonAt(SOLR, '{ "sort": "su‸')).sort(), ['div', 'sum']);
  assert.deepEqual(labels(jsonAt(SOLR, '{ "fl": "id, di‸')).sort(), ['div', 'sum']);
  assert.deepEqual(labels(jsonAt(SOLR, '{ "q": "{!func}su‸')).sort(), ['div', 'sum']);
  assert.ok(jsonAt(SOLR, '{ "sort": "‸', true));
  // A value in a list is a value too.
  assert.deepEqual(labels(jsonAt(SOLR, '[ "su‸"')).sort(), ['div', 'sum']);
  // Keys, field paths, code, and the list not asked for.
  for (const text of ['{ "su‸', '{ "q": "x", "fl‸', '{ "q": "a.su‸', '{ "sort": "‸', '{ su‸', '{ "su‸": 1 }']) {
    assert.equal(jsonAt(SOLR, text), null, text);
  }
  assert.equal(pick(jsonAt(SOLR, '{ "sort": "su‸'), 'sum', '{ "sort": "su‸'), '{ "sort": "sum(‸)');
});

// -- the lexical scan, the expression rule ---------------------------------------------------
test('the lexical scan blanks strings and comments and says where it ended', () => {
  const rules = { block: true, quotes: `'"\``, oneLine: false, regex: false };
  let s = scanCode('RETURN "a//b", x // c\n/* d */ y', rules);
  assert.equal(s.inside, 'code');
  assert.equal(s.masked, 'RETURN "    ", x     \n        y');
  s = scanCode("RETURN 'it\\'s", rules);
  assert.equal(s.inside, 'string');
  assert.equal(s.stringStart, 7);
  assert.equal(scanCode('x /* open', rules).inside, 'comment');
  // JSON: a string ends with its line; a regex after `:`.
  const j = { block: true, quotes: `"'`, oneLine: true, regex: true };
  assert.equal(scanCode('{ "a\n', j).inside, 'code');
  assert.equal(scanCode('{ a: /x', j).inside, 'string');
  assert.equal(scanCode('{ a: /x/i, b: ', j).inside, 'code');
});

test('the expression rule, flavor by flavor', () => {
  assert.equal(exprSlot('', 'sql'), false);
  assert.equal(exprSlot('', 'flux'), true);
  assert.equal(exprSlot('SELECT ', 'sql'), true);
  assert.equal(exprSlot('SELECT a ', 'sql'), false);
  assert.equal(exprSlot('x :', 'cypher'), true);
  assert.equal(exprSlot('x::', 'sql'), false);
  assert.equal(exprSlot('f(a, ', 'flux'), false);
  assert.equal(exprSlot('[a, ', 'flux'), true);
  assert.equal(exprSlot('f(\n  ', 'flux'), false);
  assert.equal(exprSlot('f(a: 1,\n  ', 'flux'), false);
  assert.equal(exprSlot('x = 1\n', 'flux'), true);
  assert.equal(exprSlot('f = (x) => {\n  ', 'flux'), true);
  assert.equal(exprSlot('x = {\n  ', 'flux'), false);
  assert.equal(exprSlot('MATCH (a)-[', 'cypher'), false);
  assert.equal(exprSlot('RETURN [', 'cypher'), true);
  assert.equal(exprSlot('SELECT f() OVER (', 'sql'), false);
  assert.equal(exprSlot('SELECT f() OVER (PARTITION BY a, ', 'sql'), true);
  assert.equal(exprSlot('SELECT * FROM a JOIN b USING (x, ', 'sql'), false);
});

// -- q ---------------------------------------------------------------------------------------------
test("q: the catalog's parameters on the names q completes, and its other names added", () => {
  const catalog = [
    F('count', 'scalar', { args: '[x]' }),
    F('upd', 'scalar', { args: '[t;x]', schema: '.u' }),
    F('select', 'operator'),
    F('wavg', 'scalar', { args: '[w;x]' }),
  ];
  const ctx = ctxOf('‸', { lang: null, explicit: true });
  const { result } = qCompletion(ctx, { tables: {}, others: [{ label: '.u.upd', kind: 'function' }], functions: catalog });
  const by = (l) => result.options.filter((o) => o.label === l);
  assert.equal(by('count').length, 1);
  assert.equal(by('count')[0].detail, '[x]');
  assert.equal(by('.u.upd').length, 1);
  assert.equal(by('.u.upd')[0].detail, '[t;x]');
  assert.equal(by('select').length, 1);
  assert.equal(by('select')[0].type, 'keyword');
  // Without a catalog, as before.
  const plain = qCompletion(ctx, { tables: {}, others: [] }).result;
  assert.equal(plain.options.find((o) => o.label === 'count').detail, undefined);
});

test('PostgreSQL dialect strings ($$ bodies) are strings too', () => {
  assert.equal(functionCompletion(ctxOf('SELECT $$ co‸ $$', { lang: PostgreSQL.language }), SQL, 'sql'), null);
});

// -- long scripts --------------------------------------------------------------------------------
test('a long script is read from LOOK_BACK before the cursor, never from inside a string or comment', () => {
  const far = 'x'.repeat(LOOK_BACK + 500);
  // The window starts inside a string, a comment, a quoted name: read from where it ends.
  for (const text of [`SELECT '${far}', co‸`, `SELECT 1 /* ${far} */, co‸`, `SELECT "${far}", co‸`, `-- ${far}\nSELECT co‸`]) {
    assert.deepEqual(labels(sql(text)).sort(), EXPR, text.slice(0, 12));
  }
  for (const text of [`SELECT '${far} co‸`, `SELECT 1 /* ${far} co‸`]) assert.equal(sql(text), null, text.slice(0, 12));
  // Statements before it don't count; the one being typed does.
  const script = 'SELECT a FROM t WHERE b = 1;\n'.repeat(2000);
  assert.deepEqual(labels(sql(`${script}SELECT co‸`)).sort(), EXPR);
  assert.equal(sql(`${script}SELECT a co‸`), null);
  // The other languages: from a line's start (a one-line string read whole).
  const lines = '{ "a": "x // y", "b": [1, 2, 3] }\n'.repeat(1000);
  assert.ok(jsonAt(MONGO, `${lines}[{ "$ma‸`));
  assert.equal(jsonAt(MONGO, `${lines}{ "name": "$ma‸`), null);
  assert.ok(labels(flux(`${'x = "a // b"\n'.repeat(1000)}fr‸`)).includes('from'));
  assert.equal(cypher(`${"RETURN 'a // b';\n".repeat(1000)}MATCH (n) RETURN n co‸`), null);
  assert.ok(labels(cypher(`${"RETURN 'a // b';\n".repeat(1000)}MATCH (n) RETURN co‸`)).includes('count'));
  // A minified JSON line longer than that: out of the string the window starts in.
  const minified = `[{ "k": "${'z'.repeat(2 * LOOK_BACK)}" }, { "a": 1 }, `;
  /** As the editor has it: the syntax tree parsed past the cursor. Without
   *  a tree (`parse` false: no language), the whole line is read. */
  const atEnd = (text, parse) => {
    const ctx = ctxOf(text, { lang: parse ? json().language : null });
    let { state } = ctx;
    if (parse) {
      assert.ok(ensureSyntaxTree(state, state.doc.length, 5000));
      state = state.update({}).state;
    }
    return functionCompletion(new CompletionContext(state, ctx.pos, false), MONGO, 'json');
  };
  for (const parse of [true, false]) {
    assert.ok(atEnd(`${minified}{ "$ma‸`, parse), String(parse));
    assert.equal(atEnd(`${minified}{ "a": "$ma‸`, parse), null, String(parse));
  }
});

test('nothing typed, a number, or a catalog without what the language takes: nothing is read', () => {
  // Typing a blank or a symbol after code asks nothing (only right after a dot).
  for (const text of ['SELECT a ‸', 'SELECT a =‸', 'SELECT (‸']) assert.equal(sql(text), null, text);
  assert.deepEqual(labels(sql('SELECT sales.‸')), ['total_sales']);
  for (const text of ['{ "a": ‸', '{ "a": 1‸']) assert.equal(jsonAt(MONGO, text), null, text);
  // Commands only in a catalog of functions, functions only in one of commands.
  assert.equal(offersAny(REDIS, 'sql'), false);
  assert.equal(offersAny(SQL, 'commands'), false);
  assert.equal(offersAny(MONGO, 'json'), true);
  assert.equal(offersAny(SOLR, 'json'), true);
  assert.equal(offersAny(functionIndex([], overloads), 'json'), false);
  assert.equal(functionCompletion(ctxOf('SELECT co‸'), REDIS, 'sql'), null);
});
