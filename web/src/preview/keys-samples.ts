// view=keys[&indexes=0|err][&schemams=N]: the explorer on a mocked Redis
// database of a few thousand keys, to see the key search (pattern, type,
// pages, namespaces) without a server, and its search indexes (the driver's
// other kind) next to it, as Redis 8 reports them (`database_schema`, an
// index's columns and its FT.CREATE); &indexes=0: a server without any;
// &indexes=err: their read fails as a schema read held too long does;
// &schemams: how long that read takes (80 ms). `window.__keysCalls`: the
// commands the backend got; `window.__keysIndexes(0|err|all)` changes what
// the next read answers.

import type { ColumnInfo, DriverInfo, KeyEntry, KeyPage, KeyScan } from '../api/types';
import type { TableSchema } from '../api/schema-types';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers } from './monitor-samples';

const redis: DriverInfo = {
  ...sampleDrivers[0],
  id: 'redis', name: 'Redis', family: 'key_value', language: 'redis', dialect: '', default_port: 6379,
  databases_label: 'Bases de datos', has_schemas: false, supports_explain: false, supports_profiler: true,
  object_kinds: [
    { id: 'key', label: 'Claves', has_columns: true, browsable: true, has_definition: true },
    { id: 'index', label: 'Índices de búsqueda', has_columns: true, browsable: true, has_definition: true },
  ],
  key_search: { syntax: 'glob', separator: ':', types: ['string', 'hash', 'list', 'set', 'zset', 'stream', 'json'], case_sensitive: true },
  designer: null,
  create_templates: [{ kind: 'index', label: 'Nuevo índice vectorial (hashes)', template: 'FT.CREATE idx ON HASH PREFIX 1 doc: SCHEMA embedding VECTOR HNSW 6 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE' }],
};

// The search indexes as Redis 8 reported them (dbine-test-redis8, the
// driver's database_schema, columns and definition), renamed.
const col = (name: string, data_type: string, key = false): ColumnInfo =>
  ({ name, data_type, nullable: !key, primary_key: key, auto_increment: false, default_value: null });
const INDEXES: { schema: TableSchema; columns: ColumnInfo[]; definition: string }[] = [
  {
    schema: {
      kind: 'index', schema: null, name: 'idx:docs', primary_key: null, foreign_keys: [], comment: null,
      options: { on: 'HASH', prefix: 'doc:' },
      columns: [
        { name: 'title', data_type: 'TEXT', nullable: true, default_value: null, auto_increment: false, comment: null, options: {} },
        { name: 'embedding', data_type: 'VECTOR(3, FLOAT32)', nullable: true, default_value: null, auto_increment: false, comment: null, options: {} },
      ],
      indexes: [{ name: 'embedding', columns: ['embedding'], unique: false, kind: 'hnsw', filter: null, vector: { algorithm: 'hnsw', metric: 'cosine', params: { ef_construction: '128', ef_runtime: '10', m: '24' } } }],
    },
    columns: [col('id', 'key', true), col('title', 'TEXT'), col('embedding', 'VECTOR(3, FLOAT32)')],
    definition: 'FT.CREATE idx:docs ON HASH PREFIX 1 doc: SCHEMA title TEXT embedding VECTOR HNSW 12 TYPE FLOAT32 DIM 3 DISTANCE_METRIC COSINE M 24 EF_CONSTRUCTION 128 EF_RUNTIME 10',
  },
  {
    schema: {
      kind: 'index', schema: null, name: 'idx:products', primary_key: null, foreign_keys: [], comment: null,
      options: { on: 'JSON', prefix: 'product:' },
      columns: [
        { name: 'title', data_type: 'TEXT', nullable: true, default_value: null, auto_increment: false, comment: null, options: {} },
        { name: 'vec', data_type: 'VECTOR(4, FLOAT32)', nullable: true, default_value: null, auto_increment: false, comment: null, options: {} },
      ],
      indexes: [{ name: 'vec', columns: ['vec'], unique: false, kind: 'flat', filter: null, vector: { algorithm: 'flat', metric: 'l2', params: {} } }],
    },
    columns: [col('id', 'key', true), col('title', 'TEXT'), col('vec', 'VECTOR(4, FLOAT32)')],
    definition: 'FT.CREATE idx:products ON JSON PREFIX 1 product: SCHEMA $.title AS title TEXT $.vec AS vec VECTOR FLAT 6 TYPE FLOAT32 DIM 4 DISTANCE_METRIC L2',
  },
];
const calls: { cmd: string; args: unknown }[] = [];
(window as unknown as Record<string, unknown>).__keysCalls = calls;

function keyspace(): KeyEntry[] {
  const out: KeyEntry[] = [];
  for (let i = 1; i <= 2000; i++) {
    out.push({ name: `user:${i}:profile`, key_type: 'hash', ttl_ms: null });
    if (i % 3 === 0) out.push({ name: `user:${i}:cart`, key_type: 'list', ttl_ms: 86_400_000 });
  }
  for (let i = 1; i <= 800; i++) out.push({ name: `cache:product:${i}`, key_type: 'string', ttl_ms: (i % 60) * 60_000 + 30_000 });
  for (let i = 0; i < 240; i++) out.push({ name: `session:${(i * 2654435761 >>> 0).toString(16)}`, key_type: 'string', ttl_ms: 1_800_000 - i * 5000 });
  out.push(
    { name: 'queue:emails', key_type: 'list', ttl_ms: null },
    { name: 'queue:webhooks', key_type: 'list', ttl_ms: null },
    { name: 'events:orders', key_type: 'stream', ttl_ms: null },
    { name: 'leaderboard', key_type: 'zset', ttl_ms: null },
    { name: 'config', key_type: 'hash', ttl_ms: null },
    { name: 'feature-flags', key_type: 'json', ttl_ms: null },
  );
  // SCAN has no order.
  return out.sort((a, b) => ((a.name.length * 31 + a.name.charCodeAt(a.name.length - 1)) % 17) - ((b.name.length * 31 + b.name.charCodeAt(b.name.length - 1)) % 17));
}
const KEYS = keyspace();

/** Redis MATCH, and plain text as "contains". */
function matcher(pattern: string): (name: string) => boolean {
  if (!pattern) return () => true;
  if (!/(^|[^\\])[*?[]/.test(pattern)) return (n) => n.includes(pattern);
  const re = pattern.replace(/\\(.)|([.+^${}()|])|(\*)|(\?)/g, (_, esc, meta, star, q) =>
    esc ? `\\${esc}` : meta ? `\\${meta}` : star ? '.*' : q ? '.' : '');
  const rx = new RegExp(`^${re}$`);
  return (n) => rx.test(n);
}

function scan(s: KeyScan): KeyPage {
  const match = matcher(s.pattern);
  const from = Number(s.cursor ?? 0);
  // Like SCAN: look at a slice of the keyspace, keep what matches.
  const slice = KEYS.slice(from, from + 1000);
  const keys = slice.filter((k) => match(k.name) && (!s.key_type || k.key_type === s.key_type));
  const next = from + slice.length;
  return { keys, cursor: next < KEYS.length ? String(next) : null, total: s.cursor ? null : KEYS.length, scanned: slice.length };
}

export function installKeysPreview() {
  const conns = useConnectionsStore();
  conns.drivers = [redis];
  conns.list = [{
    id: 'c1', name: 'Redis · caché prod', color: '#dc382d', folder_id: null, tags: ['prod'], save_password: true, updated_at: '',
    config: { driver: 'redis', host: 'cache-prod', port: 6379, database: '0', username: null, password: null,
      encrypt: false, trust_server_certificate: false, read_only: false, options: {} },
  }];
  conns.live.c1 = { status: 'connected', serverVersion: 'Redis 7.4.1', databases: ['db0', 'db1'], defaultDatabase: 'db0', error: null };
  const open = new URLSearchParams(location.search).get('open')?.split(',').map((p) => `kn:c1:db0:${p}`) ?? [];
  const params0 = new URLSearchParams(location.search);
  let answer = params0.get('indexes') ?? 'all';
  (window as unknown as Record<string, unknown>).__keysIndexes = (a: string) => { answer = a; };
  const indexesNow = () => (answer === '0' ? [] : INDEXES);
  const schemaMs = Number(params0.get('schemams') ?? 80);
  // &openix=1: the indexes' folder and the first index open too.
  const openIx = params0.get('openix') === '1' ? ['f:c1:db0:index', 'o:c1:db0:index::idx:docs'] : [];
  try { localStorage.setItem('dbine.openFolders', JSON.stringify(['c:c1', 'd:c1:db0', 'f:c1:db0:key', ...open, ...openIx])); } catch { /* preview */ }
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: { scan?: KeyScan; object?: { name: string } } }) => {
      if (!cmd.startsWith('plugin:')) calls.push({ cmd, args: a?.args ?? null });
      if (cmd === 'scan_keys') {
        await new Promise((r) => setTimeout(r, 150));
        return scan(a!.args!.scan!);
      }
      if (cmd === 'database_schema') {
        const now = answer;
        await new Promise((r) => setTimeout(r, schemaMs));
        if (now === 'err') {
          throw { kind: 'busy', message: 'la conexión sigue ocupada leyendo la estructura (por ejemplo, una comparación de esquemas en curso); volvé a intentar en un momento.' };
        }
        return (now === '0' ? [] : INDEXES).map((x) => x.schema);
      }
      const ix = indexesNow().find((x) => x.schema.name === a?.args?.object?.name);
      if (cmd === 'get_columns' && ix) return ix.columns;
      if (cmd === 'get_definition' && ix) return ix.definition;
      if (cmd === 'get_cached') return null;
      if (cmd === 'list_queries') return [];
      if (cmd === 'plugin:event|listen') return 0;
      throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
    },
  };
  conns.loadQueries('c1', 'db0');
  // &q=user:1*&type=hash starts on a search; &open=ns folders to open.
  const params = new URLSearchParams(location.search);
  conns.loadObjects('c1', 'db0').then(() => {
    const q = params.get('q');
    const t = params.get('type');
    if (q || t) conns.searchKeys('c1', 'db0', q ?? '', t ?? '');
  });
}
