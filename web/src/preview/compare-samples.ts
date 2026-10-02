// Sample data for dev-preview.html?view=compare: two databases, and the
// compare commands faked in the browser. The comparison is worked out here
// (a small version of dbine-schema/src/compare.rs), so the arrows and
// "Eliminar" change what's listed; the sync script is a rough rendering of
// the changes, and "Qué depende de lo que se borra" answers after a delay
// (one target fails on purpose, another comes back incomplete).
import type { CheckDef, ColumnDef, ForeignKeyDef, IndexDef, TableSchema } from '../api/schema-types';
import type { CodeObject, CompareResult, DbModel, ItemDiff, ObjectChange, Status, TableChange, TableDiff } from '../api/compare';
import type { DependencyReport, DependencyTarget, Dependent, IndexUsage, IndexUsageReport } from '../api/types';

const col = (name: string, data_type: string, nullable = true, extra: Partial<ColumnDef> = {}): ColumnDef =>
  ({ name, data_type, nullable, default_value: null, auto_increment: false, comment: null, options: {}, ...extra });
const table = (name: string, columns: ColumnDef[], extra: Partial<TableSchema> = {}): TableSchema =>
  ({ kind: 'table', schema: 'public', name, columns, primary_key: { name: `${name}_pkey`, columns: ['id'] }, foreign_keys: [], indexes: [], comment: null, options: {}, ...extra });
const ix = (name: string, columns: string[], unique = false): IndexDef => ({ name, columns, unique, kind: null, filter: null });
const fk = (name: string, columns: string[], ref_table: string): ForeignKeyDef =>
  ({ name, columns, ref_schema: 'public', ref_table, ref_columns: ['id'], on_delete: null, on_update: null });
const ck = (name: string, expression: string): CheckDef => ({ name, expression });
const view = (name: string, body: string): CodeObject => ({ kind: 'view', schema: 'public', name, definition: `CREATE OR REPLACE VIEW public.${name} AS\n${body}` });
const trigger: CodeObject = {
  kind: 'trigger', schema: 'public', name: 'trg_clientes_audit',
  definition: 'CREATE TRIGGER trg_clientes_audit AFTER UPDATE ON public.clientes FOR EACH ROW EXECUTE FUNCTION auditar()',
};

const left: DbModel & { warnings: string[] } = {
  driver: 'postgres',
  tables: [
    table('clientes', [col('id', 'integer', false, { auto_increment: true }), col('nombre', 'varchar(10)'), col('email', 'varchar(100)', false, { default_value: "''" }), col('alta', 'date')], {
      // ix_clientes_legacy: the same on both sides and never read (the case "Eliminar en ambos lados" is for).
      indexes: [ix('ix_clientes_nombre', ['nombre']), ix('ix_clientes_email', ['email'], true), ix('ix_clientes_legacy', ['alta', 'nombre'])],
      checks: [ck('ck_clientes_email', "email <> ''")],
      comment: 'Clientes activos e históricos',
    }),
    table('pedidos', [col('id', 'integer', false), col('cliente_id', 'integer', false), col('total', 'numeric(12,2)'), col('estado', 'varchar(20)')], {
      foreign_keys: [fk('pedidos_cliente_id_fkey', ['cliente_id'], 'clientes')],
      indexes: [ix('ix_pedidos_cliente', ['cliente_id'])],
      checks: [ck('ck_pedidos_total', 'total >= 0')],
    }),
    table('productos', [col('id', 'integer', false), col('nombre', 'text')]),
    table('auditoria', [col('id', 'bigint', false), col('evento', 'text')]),
  ],
  objects: [
    view('v_clientes', ' SELECT id,\n    nombre\n   FROM clientes\n  WHERE alta IS NOT NULL;'),
    view('v_pedidos_abiertos', " SELECT id, cliente_id, total\n   FROM pedidos\n  WHERE estado = 'abierto';"),
    trigger,
  ],
  warnings: [],
};
const right: DbModel & { warnings: string[] } = {
  driver: 'postgres',
  tables: [
    table('clientes', [col('id', 'integer', false, { auto_increment: true }), col('nombre', 'varchar(5)'), col('email', 'varchar(100)'), col('telefono', 'text'), col('alta', 'date')], {
      indexes: [ix('ix_clientes_email', ['email'], true), ix('ix_clientes_legacy', ['alta', 'nombre'])],
      checks: [ck('ck_clientes_email', "email <> ''")],
      comment: 'Clientes',
    }),
    table('pedidos', [col('id', 'integer', false), col('cliente_id', 'integer', false), col('total', 'numeric(10,2)'), col('estado', 'varchar(20)')], {
      foreign_keys: [fk('pedidos_cliente_id_fkey', ['cliente_id'], 'clientes')],
      indexes: [ix('ix_pedidos_cliente', ['cliente_id'])],
    }),
    table('productos', [col('id', 'integer', false), col('nombre', 'text')]),
    table('tmp_import', [col('id', 'integer', false), col('payload', 'jsonb')]),
  ],
  objects: [
    view('v_clientes', ' SELECT id,\n    nombre\n   FROM clientes;'),
    view('v_pedidos_abiertos', " SELECT id, cliente_id, total\n   FROM pedidos\n  WHERE estado = 'abierto';"),
    trigger,
  ],
  warnings: [],
};

// -- comparison ------------------------------------------------------------------------------
type Item = ColumnDef | IndexDef | ForeignKeyDef | CheckDef;
type Section = 'columns' | 'indexes' | 'foreign_keys' | 'checks';
const lc = (s: string | null | undefined) => (s ?? '').toLowerCase();
/** JSON with sorted keys: equal values give equal text. */
const sj = (x: unknown) => JSON.stringify(x ?? null, (_k, v) => (v && typeof v === 'object' && !Array.isArray(v) ? Object.fromEntries(Object.entries(v).sort(([a], [b]) => (a < b ? -1 : 1))) : v));
const keyOf = (x: { schema: string | null; name: string }, noSchema: boolean) => (x.schema && !noSchema ? `${lc(x.schema)}.${lc(x.name)}` : lc(x.name));
const shown = (x: { schema: string | null; name: string }) => (x.schema ? `${x.schema}.${x.name}` : x.name);

function ids(section: Section, x: Item): [string, string] {
  if (section === 'columns') return [lc((x as ColumnDef).name), ''];
  if (section === 'indexes') return [lc((x as IndexDef).name), `${(x as IndexDef).columns.map(lc).join(',')}|${(x as IndexDef).unique}`];
  if (section === 'foreign_keys') {
    const f = x as ForeignKeyDef;
    return [`${f.columns.map(lc).join(',')}>${lc(f.ref_table)}`, lc(f.name)];
  }
  return [lc((x as CheckDef).name), (x as CheckDef).expression.replace(/[\s()]/g, '').toLowerCase()];
}
const FIELD: Record<string, string> = { data_type: 'type', default_value: 'default' };
function fieldsOf(a: object, b: object): string[] {
  const keys = new Set([...Object.keys(a), ...Object.keys(b)].filter((k) => k !== 'name'));
  return [...keys].filter((k) => sj((a as Record<string, unknown>)[k]) !== sj((b as Record<string, unknown>)[k])).map((k) => FIELD[k] ?? k);
}
function pairItems(section: Section, a: Item[], b: Item[]): ItemDiff[] {
  const out: ItemDiff[] = [];
  const used = new Set<number>();
  a.forEach((x, i) => {
    const [p, f] = ids(section, x);
    let j = b.findIndex((y, k) => !used.has(k) && !!p && ids(section, y)[0] === p);
    if (j < 0) j = b.findIndex((y, k) => !used.has(k) && !!f && ids(section, y)[1] === f);
    const name = (x as { name: string | null }).name ?? ids(section, x)[0];
    if (j < 0) { out.push({ name, left: i, right: null, status: 'only_left', fields: [] }); return; }
    used.add(j);
    const fields = fieldsOf(x, b[j]);
    out.push({ name, left: i, right: j, status: fields.length ? 'changed' : 'equal', fields });
  });
  b.forEach((y, j) => { if (!used.has(j)) out.push({ name: (y as { name: string | null }).name ?? ids(section, y)[0], left: null, right: j, status: 'only_right', fields: [] }); });
  return out;
}
function compareModels(l: DbModel, r: DbModel, noSchema: boolean): CompareResult {
  const tables: TableDiff[] = [];
  const usedT = new Set<number>();
  const SECTIONS: Section[] = ['columns', 'indexes', 'foreign_keys', 'checks'];
  const one = (t: TableSchema, li: number | null, ri: number | null, status: Status): TableDiff =>
    ({ key: shown(t), left: li, right: ri, status, columns: [], indexes: [], foreign_keys: [], checks: [], primary_key: 'equal', fields: [] });
  l.tables.forEach((a, i) => {
    const j = r.tables.findIndex((b, k) => !usedT.has(k) && keyOf(b, noSchema) === keyOf(a, noSchema));
    if (j < 0) { tables.push(one(a, i, null, 'only_left')); return; }
    usedT.add(j);
    const b = r.tables[j];
    const d = one(a, i, j, 'equal');
    for (const s of SECTIONS) d[s] = pairItems(s, (a[s] ?? []) as Item[], (b[s] ?? []) as Item[]);
    d.primary_key = sj(a.primary_key?.columns.map(lc)) === sj(b.primary_key?.columns.map(lc)) ? 'equal' : !b.primary_key ? 'only_left' : !a.primary_key ? 'only_right' : 'changed';
    d.fields = [sj(a.comment) !== sj(b.comment) ? 'comment' : '', sj(a.options) !== sj(b.options) ? 'options' : ''].filter(Boolean);
    const differs = SECTIONS.some((s) => d[s].some((x) => x.status !== 'equal')) || d.primary_key !== 'equal' || d.fields.length;
    d.status = differs ? 'changed' : 'equal';
    tables.push(d);
  });
  r.tables.forEach((b, j) => { if (!usedT.has(j)) tables.push(one(b, null, j, 'only_right')); });
  const objects: CompareResult['objects'] = [];
  const usedO = new Set<number>();
  l.objects.forEach((a, i) => {
    const j = r.objects.findIndex((b, k) => !usedO.has(k) && b.kind === a.kind && keyOf(b, noSchema) === keyOf(a, noSchema));
    if (j < 0) { objects.push({ kind: a.kind, key: shown(a), left: i, right: null, status: 'only_left' }); return; }
    usedO.add(j);
    objects.push({ kind: a.kind, key: shown(a), left: i, right: j, status: a.definition.trim() === r.objects[j].definition.trim() ? 'equal' : 'changed' });
  });
  r.objects.forEach((b, j) => { if (!usedO.has(j)) objects.push({ kind: b.kind, key: shown(b), left: null, right: j, status: 'only_right' }); });
  return { tables, objects };
}

// -- sync script -----------------------------------------------------------------------------
const q = (x: { schema: string | null; name: string }) => (x.schema ? `"${x.schema}"."${x.name}"` : `"${x.name}"`);
const gone = <T extends Item>(section: Section, from: T[] = [], to: T[] = []) => from.filter((x) => !to.some((y) => ids(section, y)[0] === ids(section, x)[0]));
function script(tables: TableChange[], objects: ObjectChange[]) {
  const dropFks: string[] = [];
  const drops: string[] = [];
  const rest: string[] = [];
  const warnings: string[] = [];
  for (const c of objects) {
    const o = c.object;
    if (c.op !== 'drop') { rest.push(o.definition); continue; }
    const on = o.definition.match(/\bON\s+(\S+)/i)?.[1];
    drops.unshift(o.kind === 'trigger' && on ? `DROP TRIGGER "${o.name}" ON ${on};` : `DROP ${o.kind.toUpperCase().replace('_', ' ')} ${q(o)};`);
  }
  for (const c of tables) {
    if (c.op === 'drop') {
      drops.push(`DROP TABLE ${q(c.table)};`);
      warnings.push(`Se borra la tabla ${shown(c.table)} con todos sus datos.`);
    } else if (c.op === 'create') {
      rest.push(`CREATE TABLE ${q(c.table)} (\n${c.table.columns.map((x) => `  "${x.name}" ${x.data_type}`).join(',\n')}\n);`);
    } else {
      const { old: a, new: b } = c;
      const t = q(a);
      for (const f of gone('foreign_keys', a.foreign_keys, b.foreign_keys)) dropFks.push(`ALTER TABLE ${t} DROP CONSTRAINT "${f.name}";`);
      for (const x of gone('indexes', a.indexes, b.indexes)) rest.push(`DROP INDEX "${a.schema}"."${x.name}";`);
      for (const x of gone('checks', a.checks, b.checks)) rest.push(`ALTER TABLE ${t} DROP CONSTRAINT "${x.name}";`);
      if (a.primary_key && !b.primary_key) rest.push(`ALTER TABLE ${t} DROP CONSTRAINT "${a.primary_key.name}";`);
      for (const x of gone('columns', a.columns, b.columns)) {
        rest.push(`ALTER TABLE ${t} DROP COLUMN "${x.name}";`);
        warnings.push(`Se borra la columna ${shown(a)}.${x.name} con sus datos.`);
      }
      for (const x of gone('columns', b.columns, a.columns)) rest.push(`ALTER TABLE ${t} ADD COLUMN "${x.name}" ${x.data_type};`);
      for (const x of b.columns) {
        const was = a.columns.find((y) => lc(y.name) === lc(x.name));
        if (was && was.data_type !== x.data_type) rest.push(`ALTER TABLE ${t} ALTER COLUMN "${x.name}" TYPE ${x.data_type};`);
      }
      if (a.comment !== b.comment) rest.push(`COMMENT ON TABLE ${t} IS ${b.comment ? `'${b.comment}'` : 'NULL'};`);
      for (const x of gone('indexes', b.indexes, a.indexes)) rest.push(`CREATE ${x.unique ? 'UNIQUE ' : ''}INDEX "${x.name}" ON ${t} (${x.columns.join(', ')});`);
      for (const x of gone('checks', b.checks, a.checks)) rest.push(`ALTER TABLE ${t} ADD CONSTRAINT "${x.name}" CHECK (${x.expression});`);
      for (const f of gone('foreign_keys', b.foreign_keys, a.foreign_keys)) rest.push(`ALTER TABLE ${t} ADD CONSTRAINT "${f.name}" FOREIGN KEY (${f.columns.join(', ')}) REFERENCES "${f.ref_table}" (${f.ref_columns.join(', ')});`);
    }
  }
  return { statements: [...dropFks, ...drops, ...rest], warnings };
}

// -- dependents (get_dependents) -------------------------------------------------------------
const dep = (d: Partial<Dependent> & Pick<Dependent, 'kind' | 'name' | 'relation' | 'confidence'>): Dependent =>
  ({ schema: 'public', parent: null, detail: null, mentions: [], ...d });
function dependents(target: DependencyTarget): Promise<DependencyReport> {
  const name = lc(target.object.name);
  const column = lc(target.column);
  const wait = <T>(ms: number, v: () => T) => new Promise<T>((ok, fail) => setTimeout(() => { try { ok(v()); } catch (e) { fail(e); } }, ms));
  if (name === 'auditoria' || column === 'telefono') {
    return wait(1600, () => { throw { kind: 'permission', message: 'permiso denegado para la relación pg_depend' }; });
  }
  let items: Dependent[] = [];
  let unreadable: string[] = [];
  if (name === 'clientes' && !column) {
    items = [
      dep({ kind: 'table', name: 'pedidos', relation: 'foreign_key', confidence: 'confirmed', detail: 'pedidos_cliente_id_fkey (cliente_id) → public.clientes (id)' }),
      dep({ kind: 'view', name: 'v_clientes', relation: 'code', confidence: 'confirmed', mentions: [{ line: 4, text: '   FROM clientes', dynamic: false }] }),
      dep({ kind: 'trigger', name: 'trg_clientes_audit', parent: 'clientes', relation: 'code', confidence: 'confirmed' }),
      dep({ kind: 'procedure', name: 'p_baja_clientes', relation: 'code', confidence: 'probable', mentions: [{ line: 6, text: 'DELETE FROM clientes WHERE alta < $1', dynamic: false }] }),
      dep({ kind: 'function', name: 'f_buscar', relation: 'code', confidence: 'review', mentions: [{ line: 3, text: "EXECUTE 'SELECT * FROM clientes WHERE ' || filtro", dynamic: true }] }),
    ];
  } else if (name === 'clientes' && column === 'email') {
    items = [
      dep({ kind: 'table', name: 'clientes', relation: 'index', confidence: 'confirmed', detail: 'ix_clientes_email (email)' }),
      dep({ kind: 'table', name: 'clientes', relation: 'check', confidence: 'confirmed', detail: "ck_clientes_email: (email <> '')" }),
      dep({ kind: 'procedure', name: 'p_enviar_avisos', relation: 'code', confidence: 'probable', mentions: [{ line: 9, text: 'SELECT email FROM clientes', dynamic: false }] }),
    ];
  } else if (name === 'clientes' && column === 'alta') {
    items = [dep({ kind: 'view', name: 'v_clientes', relation: 'code', confidence: 'probable', mentions: [{ line: 5, text: '  WHERE alta IS NOT NULL;', dynamic: false }] })];
    unreadable = ['public.p_cifrado'];
  } else if (name === 'pedidos' && !column) {
    items = [dep({ kind: 'view', name: 'v_pedidos_abiertos', relation: 'code', confidence: 'confirmed' })];
  } else if (name === 'v_clientes') {
    items = [dep({ kind: 'procedure', name: 'p_reporte_mensual', relation: 'code', confidence: 'probable', mentions: [{ line: 12, text: 'FROM v_clientes c', dynamic: false }] })];
  } else if (name === 'productos') {
    unreadable = ['public.p_cifrado', 'public.f_precio'];
  }
  return wait(900 + Math.random() * 1200, () => ({ items, scanned: 14, unreadable, note: null }));
}

// -- index usage (get_index_usage) -----------------------------------------------------------
function usage(database: string, table: string): IndexUsageReport | null {
  const t = (database === 'ventas' ? left : right).tables.find((x) => lc(x.name) === lc(table));
  if (!t) return null;
  const reads: Record<string, number> = { ix_clientes_nombre: 1200, ix_clientes_email: 48_000, ix_clientes_legacy: 0, ix_pedidos_cliente: 91_000 };
  const total = Object.values(reads).reduce((a, b) => a + b, 0) + 30_000;
  const one = (name: string, columns: string[], unique: boolean, pk: boolean): IndexUsage => {
    const r = pk ? 30_000 : reads[name] ?? 0;
    return {
      name, kind: 'btree', unique, primary_key: pk, key_columns: columns, included_columns: [], filter: null, size_kb: 2048,
      seeks: r, scans: 0, lookups: 0, updates: 3_400, last_read: null, last_write: null, reads: r,
      read_share: r ? r / total : null, unused: r === 0, writes_per_read: r ? 3_400 / r : null, seek_ratio: r ? 1 : null, seek_health: r ? 'good' : null,
    };
  };
  return {
    since: '2026-09-01T00:00:00Z', stats_available: true, note: null, foreign_keys: t.foreign_keys,
    indexes: [
      ...(t.primary_key ? [one(t.primary_key.name ?? 'pk', t.primary_key.columns, true, true)] : []),
      ...t.indexes.map((x) => one(x.name, x.columns, x.unique, false)),
    ],
  };
}

/** The compare commands (and what the compare view reads besides), or `undefined` for anything else. */
export function compareMock(cmd: string, a?: { args?: Record<string, unknown> }): unknown {
  const args = a?.args ?? {};
  switch (cmd) {
    case 'schema_compare_load':
      return JSON.parse(JSON.stringify(args.database === 'ventas' ? left : right));
    case 'schema_compare': {
      const o = args.options as { ignore_schema: boolean } | undefined;
      return compareModels(args.left as DbModel, args.right as DbModel, !!o?.ignore_schema);
    }
    case 'schema_compare_convert':
      return { tables: args.tables ?? [], warnings: [] };
    case 'schema_sync_script':
      return script((args.tables ?? []) as TableChange[], (args.objects ?? []) as ObjectChange[]);
    case 'get_dependents':
      return dependents(args.target as DependencyTarget);
    case 'get_index_usage':
      return usage(String(args.database), String((args.object as { name: string }).name));
    default:
      return undefined;
  }
}
