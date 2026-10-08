import type { DriverInfo } from '../api/types';

// Snippets (live templates): an abbreviation + Tab expands to code with tab
// stops. Built-in sets per dialect family, in each one's syntax, plus the
// user's own (Configuración › Snippets, setting `editor.snippets`). The body
// uses CodeMirror's template syntax: `${1:placeholder}` is a tab stop with
// default text, `${}` where the cursor ends.

export const SNIPPETS_SETTING = 'editor.snippets';

/** A user snippet as stored in the settings. */
export interface UserSnippet {
  abbr: string;
  /** A family id (below), `sql` (every SQL engine) or `all`. */
  family: string;
  body: string;
  description?: string;
}

export interface Snippet {
  abbr: string;
  body: string;
  /** What it inserts (the completion list's detail). */
  detail: string;
  user?: boolean;
}

/** The families with their own built-in set. */
export const SNIPPET_FAMILIES = [
  'tsql', 'postgres', 'mysql', 'oracle', 'sqlite', 'db2', 'sql', 'cql', 'mongodb', 'redis', 'cypher', 'flux', 'search', 'couchdb', 'etcd',
] as const;
export type SnippetFamily = (typeof SNIPPET_FAMILIES)[number];

/** Which family an engine's editor uses; null: none (an engine without an editor). */
export function snippetFamily(d: DriverInfo | undefined | null): SnippetFamily | null {
  if (!d) return null;
  switch (d.language) {
    case 'cql': return 'cql';
    case 'cypher': return 'cypher';
    case 'flux': return 'flux';
    case 'redis': return d.dialect === 'etcd' ? 'etcd' : 'redis';
    case 'json':
      if (['mongodb', 'ferretdb', 'documentdb'].includes(d.id)) return 'mongodb';
      if (d.id === 'couchdb') return 'couchdb';
      return 'search';
    default: break;
  }
  switch (d.dialect) {
    case 'mssql': case 'sybase': return 'tsql';
    case 'postgres': return 'postgres';
    case 'mysql': case 'mariadb': return 'mysql';
    case 'oracle': return 'oracle';
    case 'sqlite': return 'sqlite';
    case 'db2': return 'db2';
    default: return 'sql';
  }
}

const SQL_FAMILIES = new Set<string>(['tsql', 'postgres', 'mysql', 'oracle', 'sqlite', 'db2', 'sql']);

/** `SELECT … FROM t` limited to a few rows, as the dialect writes it. */
function select(family: string, dialect: string, cols: string, rest = ''): string {
  if (family === 'tsql' || dialect === 'access' || dialect === 'teradata') return `SELECT TOP \${2:100} ${cols}\nFROM \${1:table}${rest}`;
  if (family === 'oracle' || family === 'db2') return `SELECT ${cols}\nFROM \${1:table}${rest}\nFETCH FIRST \${2:100} ROWS ONLY`;
  if (dialect === 'informix') return `SELECT FIRST \${2:100} ${cols}\nFROM \${1:table}${rest}`;
  return `SELECT ${cols}\nFROM \${1:table}${rest}\nLIMIT \${2:100}`;
}

function identity(family: string, dialect: string): string {
  switch (family) {
    case 'tsql': return dialect === 'sybase' ? 'NUMERIC(10) IDENTITY' : 'INT IDENTITY(1,1)';
    case 'postgres': return 'BIGINT GENERATED ALWAYS AS IDENTITY';
    case 'mysql': return 'BIGINT AUTO_INCREMENT';
    case 'oracle': return 'NUMBER GENERATED ALWAYS AS IDENTITY';
    case 'db2': return 'INTEGER GENERATED ALWAYS AS IDENTITY';
    case 'sqlite': return 'INTEGER';
    default: return 'INTEGER';
  }
}

function textType(family: string): string {
  return family === 'tsql' ? 'NVARCHAR(100)' : family === 'oracle' ? 'VARCHAR2(100)' : family === 'sqlite' ? 'TEXT' : 'VARCHAR(100)';
}

function sqlSet(family: string, dialect: string): Snippet[] {
  const s = (abbr: string, body: string, detail: string): Snippet => ({ abbr, body, detail });
  /** Engines without joins, CTEs or DDL in their SQL (document stores). */
  const docStore = ['partiql', 'cosmos'].includes(dialect);
  const out: Snippet[] = [
    s('sel', select(family, dialect, '*'), 'SELECT * FROM …'),
    s('selw', select(family, dialect, '*', '\nWHERE ${3:condition}'), 'SELECT * FROM … WHERE …'),
    s('selc', 'SELECT COUNT(*)\nFROM ${1:table}', 'SELECT COUNT(*) FROM …'),
    s('seld', 'SELECT DISTINCT ${2:column}\nFROM ${1:table}', 'SELECT DISTINCT … FROM …'),
    s('grp', 'SELECT ${2:column}, COUNT(*) AS total\nFROM ${1:table}\nGROUP BY ${2:column}\nORDER BY total DESC', 'SELECT …, COUNT(*) … GROUP BY …'),
    s('upd', 'UPDATE ${1:table}\nSET ${2:column} = ${3:value}\nWHERE ${4:condition};', 'UPDATE … SET … WHERE …'),
    s('del', 'DELETE FROM ${1:table}\nWHERE ${2:condition};', 'DELETE FROM … WHERE …'),
    s('case', 'CASE WHEN ${1:condition} THEN ${2:value} ELSE ${3:other} END', 'CASE WHEN … END'),
  ];
  if (dialect === 'partiql') {
    out.push(s('ins', "INSERT INTO \"${1:table}\" VALUE {'${2:key}': ${3:value}}", 'INSERT INTO … VALUE {…}'));
  } else if (dialect !== 'cosmos') {
    out.push(s('ins', 'INSERT INTO ${1:table} (${2:columns})\nVALUES (${3:values});', 'INSERT INTO … VALUES (…)'));
  }
  if (!docStore) {
    out.push(
      s('cte', 'WITH ${1:name} AS (\n\tSELECT ${2:*}\n\tFROM ${3:table}\n)\nSELECT *\nFROM ${1:name};', 'WITH … AS (…) SELECT …'),
      s('ij', 'INNER JOIN ${1:table} ${2:t2} ON ${2:t2}.${3:id} = ${4:t1}.${5:id}', 'INNER JOIN … ON …'),
      s('lj', 'LEFT JOIN ${1:table} ${2:t2} ON ${2:t2}.${3:id} = ${4:t1}.${5:id}', 'LEFT JOIN … ON …'),
      s('crt', `CREATE TABLE \${1:table} (\n\tid ${identity(family, dialect)} PRIMARY KEY,\n\t\${2:name} ${textType(family)} NOT NULL\${}\n);`, 'CREATE TABLE …'),
      s('exi', 'WHERE EXISTS (\n\tSELECT 1\n\tFROM ${1:table} ${2:x}\n\tWHERE ${2:x}.${3:id} = ${4:t}.${5:id}\n)', 'WHERE EXISTS (…)'),
    );
  }
  if (family === 'tsql') {
    out.push(
      s('tran', 'BEGIN TRANSACTION;\n\n${}\n\nCOMMIT;', 'BEGIN TRANSACTION … COMMIT'),
      s('try', 'BEGIN TRY\n\t${}\nEND TRY\nBEGIN CATCH\n\tTHROW;\nEND CATCH', 'BEGIN TRY … END CATCH'),
      s('proc', 'CREATE OR ALTER PROCEDURE ${1:dbo}.${2:name}\n\t@${3:param} INT\nAS\nBEGIN\n\tSET NOCOUNT ON;\n\t${}\nEND', 'CREATE OR ALTER PROCEDURE …'),
    );
  } else if (family === 'postgres') {
    out.push(
      s('tran', 'BEGIN;\n\n${}\n\nCOMMIT;', 'BEGIN … COMMIT'),
      s('func', 'CREATE OR REPLACE FUNCTION ${1:name}(${2:p integer})\nRETURNS ${3:integer}\nLANGUAGE plpgsql\nAS $$\nBEGIN\n\t${}\nEND;\n$$;', 'CREATE FUNCTION … plpgsql'),
      s('ups', 'INSERT INTO ${1:table} (${2:id}, ${3:column})\nVALUES (${4:1}, ${5:value})\nON CONFLICT (${2:id}) DO UPDATE SET ${3:column} = EXCLUDED.${3:column};', 'INSERT … ON CONFLICT DO UPDATE'),
    );
  } else if (family === 'mysql') {
    out.push(
      s('tran', 'START TRANSACTION;\n\n${}\n\nCOMMIT;', 'START TRANSACTION … COMMIT'),
      s('ups', 'INSERT INTO ${1:table} (${2:id}, ${3:column})\nVALUES (${4:1}, ${5:value})\nON DUPLICATE KEY UPDATE ${3:column} = VALUES(${3:column});', 'INSERT … ON DUPLICATE KEY UPDATE'),
    );
  } else if (family === 'oracle') {
    out.push(
      s('blk', 'BEGIN\n\t${}\nEND;\n/', 'BEGIN … END; /'),
      s('proc', 'CREATE OR REPLACE PROCEDURE ${1:name} (${2:p IN NUMBER}) AS\nBEGIN\n\t${}\nEND;\n/', 'CREATE OR REPLACE PROCEDURE …'),
    );
  } else if (family === 'sqlite') {
    out.push(s('tran', 'BEGIN;\n\n${}\n\nCOMMIT;', 'BEGIN … COMMIT'));
  }
  return out;
}

function builtIn(family: SnippetFamily, dialect: string): Snippet[] {
  const s = (abbr: string, body: string, detail: string): Snippet => ({ abbr, body, detail });
  if (SQL_FAMILIES.has(family)) return sqlSet(family, dialect);
  switch (family) {
    case 'cql': return [
      s('sel', 'SELECT *\nFROM ${1:table}\nLIMIT ${2:100};', 'SELECT * FROM … LIMIT'),
      s('selw', 'SELECT *\nFROM ${1:table}\nWHERE ${2:key} = ${3:value}\nLIMIT ${4:100};', 'SELECT * FROM … WHERE …'),
      s('selc', 'SELECT COUNT(*)\nFROM ${1:table};', 'SELECT COUNT(*) FROM …'),
      s('ins', 'INSERT INTO ${1:table} (${2:columns})\nVALUES (${3:values});', 'INSERT INTO … VALUES (…)'),
      s('upd', 'UPDATE ${1:table}\nSET ${2:column} = ${3:value}\nWHERE ${4:key} = ${5:value};', 'UPDATE … SET … WHERE …'),
      s('del', 'DELETE FROM ${1:table}\nWHERE ${2:key} = ${3:value};', 'DELETE FROM … WHERE …'),
      s('crt', 'CREATE TABLE ${1:table} (\n\t${2:id} uuid,\n\t${3:name} text,\n\tPRIMARY KEY (${2:id})\n);', 'CREATE TABLE … PRIMARY KEY'),
      s('bat', 'BEGIN BATCH\n\t${}\nAPPLY BATCH;', 'BEGIN BATCH … APPLY BATCH'),
    ];
    case 'mongodb': return [
      s('find', 'db.${1:collection}.find({ ${2:field}: ${3:value} }).limit(${4:20})', 'db.….find({…})'),
      s('sel', 'db.${1:collection}.find({}).limit(${2:20})', 'db.….find({})'),
      s('selc', 'db.${1:collection}.countDocuments({${}})', 'db.….countDocuments({})'),
      s('agg', 'db.${1:collection}.aggregate([\n\t{ $match: { ${2:field}: ${3:value} } },\n\t{ $group: { _id: "$${4:field}", total: { $sum: 1 } } }\n])', 'db.….aggregate([…])'),
      s('ij', '{ $lookup: { from: "${1:other}", localField: "${2:field}", foreignField: "${3:_id}", as: "${4:joined}" } }', '$lookup'),
      s('ins', 'db.${1:collection}.insertOne({ ${2:field}: ${3:value} })', 'db.….insertOne({…})'),
      s('upd', 'db.${1:collection}.updateMany({ ${2:field}: ${3:value} }, { $set: { ${4:field}: ${5:value} } })', 'db.….updateMany(…, { $set })'),
      s('del', 'db.${1:collection}.deleteMany({ ${2:field}: ${3:value} })', 'db.….deleteMany({…})'),
      s('crt', 'db.createCollection("${1:name}")', 'db.createCollection(…)'),
      s('idx', 'db.${1:collection}.createIndex({ ${2:field}: 1 })', 'db.….createIndex({…})'),
    ];
    case 'redis': return [
      s('get', 'GET ${1:key}', 'GET key'),
      s('set', 'SET ${1:key} ${2:value}${3: EX 3600}', 'SET key value'),
      s('hgetall', 'HGETALL ${1:key}', 'HGETALL key'),
      s('hset', 'HSET ${1:key} ${2:field} ${3:value}', 'HSET key field value'),
      s('scan', 'SCAN 0 MATCH ${1:prefix}:* COUNT ${2:100}', 'SCAN 0 MATCH …'),
      s('del', 'DEL ${1:key}', 'DEL key'),
      s('ttl', 'TTL ${1:key}', 'TTL key'),
      s('lr', 'LRANGE ${1:key} 0 ${2:-1}', 'LRANGE key 0 -1'),
      s('zr', 'ZRANGE ${1:key} 0 ${2:-1} WITHSCORES', 'ZRANGE … WITHSCORES'),
    ];
    case 'etcd': return [
      s('get', 'get ${1:key}', 'get key'),
      s('getp', 'get ${1:prefix} --prefix', 'get prefix --prefix'),
      s('put', 'put ${1:key} ${2:value}', 'put key value'),
      s('del', 'del ${1:key}', 'del key'),
    ];
    case 'cypher': return [
      s('sel', 'MATCH (${1:n}:${2:Label})\nRETURN ${1:n}\nLIMIT ${3:25}', 'MATCH (n:Label) RETURN n'),
      s('selw', 'MATCH (${1:n}:${2:Label})\nWHERE ${1:n}.${3:prop} = ${4:value}\nRETURN ${1:n}\nLIMIT ${5:25}', 'MATCH … WHERE … RETURN'),
      s('selc', 'MATCH (${1:n}:${2:Label})\nRETURN count(${1:n})', 'MATCH … RETURN count(n)'),
      s('rel', 'MATCH (${1:a}:${2:Label})-[${3:r}:${4:TYPE}]->(${5:b})\nRETURN ${1:a}, ${3:r}, ${5:b}\nLIMIT ${6:25}', 'MATCH (a)-[r]->(b)'),
      s('ins', 'CREATE (${1:n}:${2:Label} {${3:name}: ${4:value}})', 'CREATE (n:Label {…})'),
      s('mrg', 'MERGE (${1:n}:${2:Label} {${3:id}: ${4:value}})\nON CREATE SET ${1:n}.${5:prop} = ${6:value}', 'MERGE … ON CREATE SET'),
      s('upd', 'MATCH (${1:n}:${2:Label} {${3:id}: ${4:value}})\nSET ${1:n}.${5:prop} = ${6:value}', 'MATCH … SET …'),
      s('del', 'MATCH (${1:n}:${2:Label} {${3:id}: ${4:value}})\nDETACH DELETE ${1:n}', 'MATCH … DETACH DELETE'),
    ];
    case 'flux': return [
      s('sel', 'from(bucket: "${1:bucket}")\n\t|> range(start: ${2:-1h})\n\t|> filter(fn: (r) => r._measurement == "${3:measurement}")\n\t|> limit(n: ${4:100})', 'from() |> range() |> filter()'),
      s('agg', 'from(bucket: "${1:bucket}")\n\t|> range(start: ${2:-1h})\n\t|> filter(fn: (r) => r._measurement == "${3:measurement}")\n\t|> aggregateWindow(every: ${4:1m}, fn: ${5:mean})', 'aggregateWindow()'),
    ];
    case 'search': return [
      s('sel', 'GET /${1:index}/_search\n{\n\t"query": { "match_all": {} },\n\t"size": ${2:20}\n}', 'GET /index/_search'),
      s('selw', 'GET /${1:index}/_search\n{\n\t"query": { "match": { "${2:field}": "${3:text}" } }\n}', '_search match'),
      s('selc', 'GET /${1:index}/_count', 'GET /index/_count'),
      s('agg', 'GET /${1:index}/_search\n{\n\t"size": 0,\n\t"aggs": { "${2:by}": { "terms": { "field": "${3:field}" } } }\n}', '_search aggs terms'),
      s('ins', 'POST /${1:index}/_doc\n{ "${2:field}": "${3:value}" }', 'POST /index/_doc'),
      s('del', 'DELETE /${1:index}/_doc/${2:id}', 'DELETE /index/_doc/id'),
    ];
    case 'couchdb': return [
      s('sel', '{ "selector": { "${1:field}": { "$eq": "${2:value}" } }, "limit": ${3:20} }', 'Mango { selector }'),
      s('all', 'GET /${1:db}/_all_docs?include_docs=true&limit=${2:10}', 'GET _all_docs'),
      s('idx', 'POST _index { "index": { "fields": ["${1:field}"] } }', 'POST _index'),
    ];
    default: return [];
  }
}

/** What applies to an engine: its built-in set and the user's snippets for
 *  it (a user snippet replaces a built-in one with the same abbreviation). */
export function snippetsFor(d: DriverInfo | undefined | null, user: UserSnippet[]): Snippet[] {
  const family = snippetFamily(d);
  if (!family) return [];
  const mine = user
    .filter((u) => u.abbr.trim() && u.body && (u.family === 'all' || u.family === family || (u.family === 'sql' && SQL_FAMILIES.has(family))))
    .map((u) => ({ abbr: u.abbr.trim(), body: u.body, detail: u.description || u.body.split('\n')[0], user: true }));
  const taken = new Set(mine.map((m) => m.abbr.toLowerCase()));
  return [...mine, ...builtIn(family, d?.dialect ?? '').filter((b) => !taken.has(b.abbr.toLowerCase()))];
}

/** The built-in set of a family, for Configuración (with a sample dialect). */
export function builtInSnippets(family: SnippetFamily): Snippet[] {
  return builtIn(family, '');
}
