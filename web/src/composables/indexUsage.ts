import { reactive } from 'vue';
import { api, errorMessage } from '../api/client';
import type { IndexUsage, IndexUsageReport, ObjectRef } from '../api/types';
import type { ForeignKeyDef } from '../api/schema-types';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';

// A table's indexes and how they're used (`get_index_usage`), shared by the
// explorer's "Índices" folder, the "Índices" tab and schema compare: one
// cache per connection, database and table. The derived numbers (reads,
// read share, unused) come filled by the backend (`IndexUsageReport::derive`).

export interface IndexUsageEntry {
  status: 'loading' | 'ready' | 'error';
  /** null: the driver doesn't report it. */
  report: IndexUsageReport | null;
  error: string | null;
}

const entries = reactive<Record<string, IndexUsageEntry>>({});
const pending = new Map<string, Promise<IndexUsageReport | null>>();

type TableRef = { schema?: string | null; name: string };

export function indexUsageKey(connectionId: string, database: string, table: TableRef): string {
  return [connectionId, database, table.schema ?? '', table.name].join('\u0001');
}

export function indexUsageEntry(connectionId: string, database: string, table: TableRef): IndexUsageEntry | undefined {
  return entries[indexUsageKey(connectionId, database, table)];
}

/** Read once per table (again with `force`); concurrent callers share the read. */
export function loadIndexUsage(connectionId: string, database: string, table: ObjectRef, force = false): Promise<IndexUsageReport | null> {
  const k = indexUsageKey(connectionId, database, table);
  const had = entries[k];
  if (!force && had?.status === 'ready') return Promise.resolve(had.report);
  const running = pending.get(k);
  if (running) return running;
  entries[k] = { status: 'loading', report: had?.report ?? null, error: null };
  const p = (async () => {
    try {
      if (!(await useConnectionsStore().ensureConnected(connectionId))) throw new Error(t('common:error'));
      const report = await api.getIndexUsage(connectionId, database, { kind: table.kind, schema: table.schema ?? null, name: table.name });
      entries[k] = { status: 'ready', report, error: null };
      return report;
    } catch (e) {
      entries[k] = { status: 'error', report: null, error: errorMessage(e) };
      return null;
    } finally {
      pending.delete(k);
    }
  })();
  pending.set(k, p);
  return p;
}

/** The short tag of an index's type: PK, UNIQUE, CLUSTERED, NC, COLUMNSTORE… */
export function indexTag(i: IndexUsage): string {
  const kind = i.kind.toUpperCase();
  if (i.primary_key) return 'PK';
  if (kind.includes('COLUMNSTORE')) return 'COLUMNSTORE';
  if (i.unique) return 'UNIQUE';
  if (kind === 'CLUSTERED') return 'CLUSTERED';
  if (kind === 'NONCLUSTERED') return 'NC';
  return kind;
}

export type SeekHealth = 'good' | 'warn' | 'bad';
export interface UsageBadge {
  text: string;
  unused: boolean;
  /** Its color: seeks against scans (`seek_health`, derived by the backend). */
  health: SeekHealth | null;
  /** Why that color (null without seeks or scans). */
  healthTip: string | null;
}

/** The usage badge: the read share, "sin uso" (only where writes are
 *  counted), "0%" (counters, but no reads
 *  on the table yet) or "sin datos" (the engine/login gives no counters).
 *  `null` only without a report to say anything about. */
export function usageBadge(i: IndexUsage, report?: Pick<IndexUsageReport, 'stats_available' | 'note' | 'writes_counted'> | null): UsageBadge | null {
  if (report && !report.stats_available) {
    return { text: t('explorer:indexes.noData'), unused: false, health: null, healthTip: report.note ? tb(report.note) : t('explorer:indexes.noDataTip') };
  }
  // Without write counts no index can be judged unused (the backend already
  // leaves `unused` false; a report from an older host might not).
  if (i.unused && report?.writes_counted !== false) return { text: t('explorer:indexes.unused'), unused: true, health: null, healthTip: null };
  if (i.read_share == null) {
    return report ? { text: '0%', unused: false, health: null, healthTip: t('explorer:indexes.noReadsTip') } : null;
  }
  return { text: sharePct(i.read_share), unused: false, health: i.seek_health ?? null, healthTip: seekTip(i) };
}

/** The badge's classes: `unused`, or its seek health (`h-good`, `h-warn`, `h-bad`). */
export function badgeClass(b: UsageBadge): string[] {
  return b.unused ? ['unused'] : b.health ? [`h-${b.health}`] : [];
}

/** Why an index's seek health is what it is. */
export function seekTip(i: IndexUsage): string | null {
  if (!i.seek_health) return null;
  const n = { seeks: i.seeks.toLocaleString(), scans: i.scans.toLocaleString() };
  if (i.seek_health === 'good' && i.kind.toUpperCase().includes('COLUMNSTORE') && (i.seek_ratio ?? 1) < 0.8) return t('explorer:indexes.health.columnstore', n);
  return t(`explorer:indexes.health.${i.seek_health}`, n);
}

/** 0–1 as a percentage: "<1%" for a share above zero that rounds to it. */
export function sharePct(share: number): string {
  const pct = Math.round(share * 100);
  return pct === 0 && share > 0 ? '<1%' : `${pct}%`;
}

/** Each foreign-key column of the table, with what it references. */
export function foreignKeyColumns(report: IndexUsageReport | null | undefined): Map<string, string> {
  const out = new Map<string, string>();
  for (const fk of (report?.foreign_keys ?? []) as ForeignKeyDef[]) {
    fk.columns.forEach((c, n) => {
      const table = fk.ref_schema ? `${fk.ref_schema}.${fk.ref_table}` : fk.ref_table;
      out.set(c, `${table}.${fk.ref_columns[n] ?? ''}`);
    });
  }
  return out;
}
