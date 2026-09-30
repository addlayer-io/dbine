import type { Cell, ResultColumn } from '../api/types';
import { t } from '../i18n';
import { useSettingsStore } from '../stores/settings';
import { rt } from './i18nLabels';

// Copying grid rows to the clipboard in several formats.
// The default format (what ⌘C copies) is a user preference (synced).

export type CopyFormat =
  | 'tsv_headers' | 'tsv' | 'headers' | 'csv' | 'json' | 'json_lines' | 'yaml'
  | 'sql_insert' | 'sql_update' | 'mongo_insert';

/** A format whose name is a key (translated) or the format's own name. */
function copyFormat(id: CopyFormat, name: string, keys?: { label: string; name: string }) {
  return {
    id,
    get label() { return keys ? rt(keys.label) : rt('core:copy.as', { format: name }); },
    get name() { return keys ? rt(keys.name) : name; },
  };
}

/** `label` and `name` are getters in the current language. */
export const COPY_FORMATS: { readonly id: CopyFormat; readonly label: string; readonly name: string }[] = [
  copyFormat('tsv_headers', '', { label: 'core:copy.withHeadersLabel', name: 'core:copy.withHeaders' }),
  copyFormat('tsv', '', { label: 'core:copy.withoutHeadersLabel', name: 'core:copy.withoutHeaders' }),
  copyFormat('headers', '', { label: 'core:copy.headersOnlyLabel', name: 'core:copy.headersOnly' }),
  copyFormat('csv', 'CSV'),
  copyFormat('json', 'JSON'),
  copyFormat('json_lines', 'JSON Lines / NDJSON'),
  copyFormat('yaml', 'YAML'),
  copyFormat('sql_insert', 'SQL INSERTs'),
  copyFormat('sql_update', 'SQL UPDATEs'),
  copyFormat('mongo_insert', 'Mongo INSERTs'),
];

const KEY = 'grid.copyFormat';
export function defaultCopyFormat(): CopyFormat {
  const f = useSettingsStore().get<CopyFormat>(KEY, 'tsv');
  return COPY_FORMATS.some((x) => x.id === f) ? f : 'tsv';
}
export function setDefaultCopyFormat(f: CopyFormat) {
  useSettingsStore().set(KEY, f);
}

export interface CopyContext {
  /** Table the rows come from (SQL/Mongo formats); "tabla" when unknown. */
  table?: { schema: string | null; name: string } | null;
  /** Driver's SQL dialect, for identifier quoting. */
  dialect?: string;
  /** WHERE columns of SQL UPDATEs; guessed ("id", else the first) when empty. */
  keyColumns?: string[];
}

function quoteIdent(dialect: string, name: string) {
  if (['mssql', 'sybase', 'access'].includes(dialect)) return `[${name.replace(/]/g, ']]')}]`;
  if (['mysql', 'bigquery', 'hive', 'clickhouse', 'sparksql', 'databricks'].includes(dialect)) return `\`${name.replace(/`/g, '``')}\``;
  return `"${name.replace(/"/g, '""')}"`;
}

function sqlLiteral(v: Cell): string {
  if (v === null) return 'NULL';
  if (typeof v === 'boolean') return v ? '1' : '0';
  if (typeof v === 'number') return String(v);
  return `'${v.replace(/'/g, "''")}'`;
}

function text(v: Cell): string {
  return v === null ? '' : String(v);
}

function csvField(s: string) {
  return /[",\r\n]/.test(s) ? `"${s.replace(/"/g, '""')}"` : s;
}

function yamlScalar(v: Cell): string {
  if (v === null) return 'null';
  if (typeof v !== 'string') return String(v);
  // Quote when YAML would read it as something else.
  return /^$|^[\s\-?:,[\]{}#&*!|>'"%@`]|: |\s#|^(true|false|null|yes|no|~|[-+]?\d[\d._]*(e[-+]?\d+)?)$/i.test(v) || v.includes('\n')
    ? JSON.stringify(v)
    : v;
}

function objects(columns: ResultColumn[], rows: Cell[][]) {
  return rows.map((r) => Object.fromEntries(columns.map((c, i) => [c.name, r[i] ?? null])));
}

export function formatRows(format: CopyFormat, columns: ResultColumn[], rows: Cell[][], ctx: CopyContext = {}): string {
  const names = columns.map((c) => c.name);
  const dialect = ctx.dialect ?? '';
  const table = ctx.table ?? { schema: null, name: t('core:export.defaultTable') };
  const tableName = (table.schema ? `${quoteIdent(dialect, table.schema)}.` : '') + quoteIdent(dialect, table.name);
  const tsvCell = (v: Cell) => text(v).replace(/\t/g, ' ').replace(/\r?\n/g, ' ');
  switch (format) {
    case 'tsv_headers':
      return [names.join('\t'), ...rows.map((r) => r.map(tsvCell).join('\t'))].join('\r\n');
    case 'tsv':
      return rows.map((r) => r.map(tsvCell).join('\t')).join('\r\n');
    case 'headers':
      return names.join('\t');
    case 'csv':
      return [names.map(csvField).join(','), ...rows.map((r) => r.map((v) => csvField(text(v))).join(','))].join('\r\n');
    case 'json':
      return JSON.stringify(objects(columns, rows), null, 2);
    case 'json_lines':
      return objects(columns, rows).map((o) => JSON.stringify(o)).join('\r\n');
    case 'yaml':
      return objects(columns, rows)
        .map((o) => Object.entries(o).map(([k, v], i) => `${i === 0 ? '- ' : '  '}${yamlScalar(k)}: ${yamlScalar(v)}`).join('\n'))
        .join('\n');
    case 'sql_insert': {
      const cols = names.map((n) => quoteIdent(dialect, n)).join(', ');
      return rows.map((r) => `INSERT INTO ${tableName} (${cols}) VALUES (${r.map(sqlLiteral).join(', ')});`).join('\n');
    }
    case 'sql_update': {
      const keys = ctx.keyColumns?.length
        ? ctx.keyColumns
        : [names.find((n) => n.toLowerCase() === 'id') ?? names[0]];
      const keyIdx = keys.map((k) => names.indexOf(k)).filter((i) => i >= 0);
      return rows
        .map((r) => {
          const sets = names.map((n, i) => (keyIdx.includes(i) ? null : `${quoteIdent(dialect, n)} = ${sqlLiteral(r[i])}`)).filter(Boolean);
          const where = keyIdx.map((i) => `${quoteIdent(dialect, names[i])} = ${sqlLiteral(r[i])}`).join(' AND ');
          return `UPDATE ${tableName} SET ${sets.join(', ')} WHERE ${where};`;
        })
        .join('\n');
    }
    case 'mongo_insert': {
      const coll = /^[A-Za-z_][\w]*$/.test(table.name) ? `db.${table.name}` : `db.getCollection(${JSON.stringify(table.name)})`;
      return objects(columns, rows).map((o) => `${coll}.insertOne(${JSON.stringify(o, null, 2)});`).join('\n');
    }
  }
}
