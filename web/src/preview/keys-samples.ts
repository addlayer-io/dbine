// view=keys: the explorer on a mocked Redis database of a few thousand keys,
// to see the key search (pattern, type, pages, namespaces) without a server.

import type { DriverInfo, KeyEntry, KeyPage, KeyScan } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers } from './monitor-samples';

const redis: DriverInfo = {
  ...sampleDrivers[0],
  id: 'redis', name: 'Redis', family: 'key_value', language: 'redis', dialect: '', default_port: 6379,
  databases_label: 'Bases de datos', has_schemas: false, supports_explain: false, supports_profiler: true,
  object_kinds: [{ id: 'key', label: 'Claves', has_columns: true, browsable: true, has_definition: true }],
  key_search: { syntax: 'glob', separator: ':', types: ['string', 'hash', 'list', 'set', 'zset', 'stream', 'json'], case_sensitive: true },
  designer: null, create_templates: [],
};

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
  try { localStorage.setItem('dbine.openFolders', JSON.stringify(['c:c1', 'd:c1:db0', 'f:c1:db0:key', ...open])); } catch { /* preview */ }
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async (cmd: string, a?: { args?: { scan?: KeyScan } }) => {
      if (cmd === 'scan_keys') {
        await new Promise((r) => setTimeout(r, 150));
        return scan(a!.args!.scan!);
      }
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
