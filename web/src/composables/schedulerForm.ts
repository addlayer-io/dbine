// The scheduler designer's form logic ("Programadas › En los servidores",
// docs/server-jobs.md): which fields, lists and tabs show, a new object's and
// a new row's values, the checks before a script or a save, what a reference
// offers, what goes back to the driver, and what an object of the tree
// offers (edit, delete, its kind's actions). Plain functions over what the
// driver describes (api/scheduler.ts), so `node --test` runs them
// (schedulerForm.test.mjs). The driver stays the authority: it checks every
// value again when it writes the script.

import type {
  FormField, FormList, FormRow, FormSection, FormValues, ObjectForm, SchedulerAction, SchedulerItem, SchedulerKind,
} from '../api/scheduler';
import type { Language } from '../api/types';

/** Values by field key: the form's top level, or one row of a list. */
export type Scope = Record<string, unknown>;

/** Where a field's `when` (and a ref source's `{field}`) is looked up: the
 *  fields beside it and their values; a row's context has the form's top
 *  level as its parent. */
export interface FormCtx {
  fields: FormField[];
  values: Scope;
  parent?: FormCtx;
  /** The form's top level (`topContext`): the tabs each field is on. A
   *  field whose tabs all hide doesn't show, so it holds no condition. */
  tabsOf?: Map<FormField, FormSection[]>;
}

/** A condition: a field's key and the values that show what hangs from it. */
export type Cond = [string, string[]];

const clone = <T>(v: T): T => (v === undefined ? v : (JSON.parse(JSON.stringify(v)) as T));

/** A value as `when` compares it: text; booleans as "true" / "false"; nothing as "". */
export function whenText(v: unknown): string {
  if (v === null || v === undefined) return '';
  if (typeof v === 'boolean') return v ? 'true' : 'false';
  if (typeof v === 'object') return JSON.stringify(v);
  return String(v);
}

/** The value matches one of `wanted` (a list value: any of its items does). */
function matches(v: unknown, wanted: string[]): boolean {
  if (Array.isArray(v)) return v.some((x) => wanted.includes(whenText(x)));
  return wanted.includes(whenText(v));
}

/** What a visibility check has already been through (fields and tabs): met
 *  again, it's a cycle. */
type Seen = Set<FormField | FormSection>;

/** Whether one condition holds: the field it names (of the same row or
 *  form, else of the form's top level) shows and has one of the values. A
 *  top-level field shows only on a tab that shows: a value left on a tab
 *  that hid holds nothing elsewhere. A condition naming no field compares
 *  the value as it is. */
function holds([key, wanted]: Cond, ctx: FormCtx, self: FormField | null, seen: Seen): boolean {
  for (let c: FormCtx | undefined = ctx; c; c = c.parent) {
    const owners = c.fields.filter((f) => f.key === key && f !== self);
    if (!owners.length) continue;
    // Several fields may share a key under exclusive `when`s: one shown is enough.
    const at = c;
    if (!owners.some((o) => { const s = new Set(seen); return isVisible(o, at, s) && onShownTab(o, at, s); })) return false;
    return matches(c.values[key], wanted);
  }
  return matches(key in ctx.values ? ctx.values[key] : ctx.parent?.values[key], wanted);
}

/** A top-level field is on a tab that shows (any field elsewhere: true). */
function onShownTab(field: FormField, ctx: FormCtx, seen: Seen): boolean {
  const tabs = ctx.tabsOf?.get(field);
  return !tabs || tabs.some((s) => tabShows(s, ctx, seen));
}

function tabShows(section: FormSection, top: FormCtx, seen: Seen): boolean {
  if (!section.when) return true;
  if (seen.has(section)) return true; // a cycle: don't hide everything over it
  seen.add(section);
  return holds(section.when, top, null, seen);
}

/** Whether a field shows: its `when` and every `when_all` hold. Each names
 *  a field of the same row (or form), else of the form's top level; a field
 *  that depends on a hidden one is hidden too (the frequency's interval
 *  under a frequency that doesn't apply, a field of a tab that hid). */
export function isVisible(field: FormField, ctx: FormCtx, seen: Seen = new Set()): boolean {
  const conds = [...(field.when ? [field.when] : []), ...(field.when_all ?? [])];
  if (!conds.length) return true;
  if (seen.has(field)) return true; // a cycle: don't hide everything over it
  seen.add(field);
  return conds.every((c) => holds(c, ctx, field, seen));
}

/** The form's top level as conditions see it, with the tabs of each field. */
export function topContext(form: ObjectForm, values: Scope): FormCtx {
  const tabsOf = new Map<FormField, FormSection[]>();
  for (const s of form.sections) for (const f of s.fields ?? []) tabsOf.set(f, [...(tabsOf.get(f) ?? []), s]);
  return { fields: formFields(form), values, tabsOf };
}

/** Whether a section's tab shows: its `when` names a top-level field (a
 *  job's type picks its tabs). */
export function sectionVisible(section: FormSection, top: FormCtx): boolean {
  return tabShows(section, top, new Set());
}

/** Whether a list shows (and is checked and sent): its `when`, on a
 *  top-level field, holds. Its section's own `when` is `sectionVisible`'s. */
export function listVisible(list: FormList, top: FormCtx): boolean {
  return !list.when || holds(list.when, top, null, new Set());
}

/** The lists that show now: those of the tabs that show, whose own `when`
 *  holds. Only they are checked and sent. */
export function visibleLists(form: ObjectForm, top: FormCtx): FormList[] {
  return form.sections.filter((s) => sectionVisible(s, top)).flatMap((s) => (s.lists ?? []).filter((l) => listVisible(l, top)));
}

/** Every top-level field of the form, section after section. */
export function formFields(form: ObjectForm): FormField[] {
  return form.sections.flatMap((s) => s.fields ?? []);
}

/** Every list of the form, section after section. */
export function formLists(form: ObjectForm): FormList[] {
  return form.sections.flatMap((s) => s.lists ?? []);
}

export function findList(form: ObjectForm, key: string): FormList | undefined {
  return formLists(form).find((l) => l.key === key);
}

/** The form's top level as the designer starts it: what the driver sent,
 *  and for a new object each missing field's default. A secret never comes
 *  with a value (null: keep the server's). */
export function initialValues(form: ObjectForm, isNew: boolean): Scope {
  const out: Scope = clone(form.values ?? {});
  for (const f of formFields(form)) {
    if (f.kind.type === 'secret') out[f.key] = null;
    else if (isNew && !(f.key in out) && f.default !== undefined && f.default !== null) out[f.key] = clone(f.default);
  }
  return out;
}

/** Each list's rows as they came (copies: the designer edits them). */
export function initialLists(form: ObjectForm): Record<string, FormRow[]> {
  return Object.fromEntries(formLists(form).map((l) => [l.key, clone(l.rows ?? [])]));
}

/** A row added with "Agregar": the defaults of the list's fields. */
export function newRow(list: FormList): FormRow {
  const row: FormRow = {};
  for (const f of list.fields) {
    if (f.kind.type === 'secret') row[f.key] = null;
    else if (!(f.key in row) && f.default !== undefined && f.default !== null) row[f.key] = clone(f.default);
  }
  return row;
}

/** A row of a list, to leave out of that list's own choices. */
export interface RowRef {
  list: string;
  /** Its place in the list; null: a row being added (not in it yet). */
  index: number | null;
}

/** What a `ref` field offers: `choices[source]`, or for `list:<key>` the
 *  rows of that list as they are now, valued by their `title_field` and
 *  labeled "N · value" (a job's steps by number and name). `skip` leaves a
 *  row out of its own list's choices: a step doesn't go to itself. */
export function refChoices(source: string, form: ObjectForm, lists: Record<string, FormRow[]>, skip?: RowRef): [string, string][] {
  if (!source.startsWith('list:')) return form.choices?.[source] ?? [];
  const list = findList(form, source.slice(5));
  if (!list) return [];
  const out: [string, string][] = [];
  (lists[list.key] ?? []).forEach((row, i) => {
    if (skip && skip.list === list.key && skip.index === i) return;
    const v = row[list.title_field];
    if (v === null || v === undefined || v === '') return;
    out.push([String(v), `${i + 1} · ${String(v)}`]);
  });
  return out;
}

const PLACEHOLDER = /\{([^{}]+)\}/g;

/** The fields a `ref` source names between braces (`perf_counter:{object}`). */
export function sourceFields(source: string): string[] {
  return [...source.matchAll(PLACEHOLDER)].map((m) => m[1]);
}

/** A source with each `{field}` filled in from the context: the row's own
 *  field first (inside a list), else the form's top level; nothing as "". */
export function fillSource(source: string, ctx?: FormCtx): string {
  return source.replace(PLACEHOLDER, (_, key: string) => {
    for (let c: FormCtx | undefined = ctx; c; c = c.parent) {
      if (key in c.values || c.fields.some((f) => f.key === key)) return whenText(c.values[key]);
    }
    return '';
  });
}

/** What a `ref` field offers where it is (`ctx`: its row, or the top level),
 *  or null when it takes free text: its source names fields and the driver
 *  listed nothing for the filled-in source (`perf_counter:SQLServer:Locks`).
 *  Other sources as `refChoices`. */
export function refOptions(source: string, form: ObjectForm, lists: Record<string, FormRow[]>, ctx?: FormCtx, skip?: RowRef): [string, string][] | null {
  if (!sourceFields(source).length) return refChoices(source, form, lists, skip);
  const listed = form.choices?.[fillSource(source, ctx)];
  return listed?.length ? listed : null;
}

/** A select's or a ref's options as the driver gave them (labels in its
 *  Spanish, untranslated): the select's own, or the ref's `choices`. */
export function fieldOptions(field: FormField, choices: [string, string][] = []): [string, string][] {
  const k = field.kind;
  if (k.type === 'select' || k.type === 'multi_select') return k.options;
  return k.type === 'ref' ? choices : [];
}

/** The label (untranslated) of the option a select's or a ref's value is,
 *  or null when it's none of them. An option may have "" as its value (the
 *  Agent's service account), so "" is looked up like any other value. */
export function optionLabel(field: FormField, value: unknown, choices: [string, string][] = []): string | null {
  if (value === null || value === undefined) return null;
  const v = whenText(value);
  return fieldOptions(field, choices).find(([o]) => o === v)?.[1] ?? null;
}

/** After a row of list `listKey` changed its title (`from` → `to`): every
 *  reference to it — a `list:<listKey>` field of the form's top level or of
 *  any list's rows — follows the new title (a job's start step, a step's
 *  "go to step"). Hidden fields too: they keep their values. Nothing changes
 *  when another row still has the old title (the references still name a
 *  row). Returns copies where something changed, and how many fields. */
export function renameRefs(
  form: ObjectForm, values: Scope, lists: Record<string, FormRow[]>, listKey: string, from: string, to: string,
): { values: Scope; lists: Record<string, FormRow[]>; count: number } {
  const list = findList(form, listKey);
  const still = list && (lists[listKey] ?? []).some((r) => whenText(r[list.title_field]) === from);
  if (!list || !from || !to || from === to || still) return { values, lists, count: 0 };
  const source = `list:${listKey}`;
  const refKeys = (fields: FormField[]) => [...new Set(fields.filter((f) => f.kind.type === 'ref' && f.kind.source === source).map((f) => f.key))];
  let count = 0;
  const fix = (scope: Scope, keys: string[]): Scope => {
    let out = scope;
    for (const k of keys) {
      const v = scope[k];
      let next = v;
      if (Array.isArray(v)) {
        if (v.some((x) => whenText(x) === from)) next = v.map((x) => (whenText(x) === from ? to : x));
      } else if (v !== null && v !== undefined && whenText(v) === from) {
        next = to;
      }
      if (next === v) continue;
      if (out === scope) out = { ...scope };
      out[k] = next;
      count++;
    }
    return out;
  };
  const outValues = fix(values, refKeys(formFields(form)));
  const outLists = { ...lists };
  for (const l of formLists(form)) {
    const keys = refKeys(l.fields);
    const rows = lists[l.key];
    if (!keys.length || !rows) continue;
    const fixed = rows.map((r) => fix(r, keys));
    if (fixed.some((r, i) => r !== rows[i])) outLists[l.key] = fixed;
  }
  return { values: outValues, lists: outLists, count };
}

/** What's wrong with one value. */
export type Problem =
  | { code: 'required' }
  | { code: 'min'; n: number }
  | { code: 'max'; n: number }
  | { code: 'minRows'; n: number }
  /** A `list:` reference to a row that isn't there (renamed, removed, or itself). */
  | { code: 'missing'; value: string };

type Translate = (key: string, options?: Record<string, unknown>) => string;

/** A problem as the user reads it, with the UI's translate function. */
export function problemText(p: Problem, t: Translate): string {
  switch (p.code) {
    case 'required': return t('scheduled:designer.errors.required');
    case 'min': return t('scheduled:designer.errors.min', { n: p.n });
    case 'max': return t('scheduled:designer.errors.max', { n: p.n });
    case 'missing': return t('scheduled:designer.errors.missing', { value: p.value });
    default: return t('scheduled:designer.errors.minRows', { count: p.n });
  }
}

export interface RowProblem {
  row: number;
  field: string;
  problem: Problem;
}

export interface Problems {
  /** Top-level field key → its problem. */
  fields: Record<string, Problem>;
  /** List key → too few rows. */
  lists: Record<string, Problem>;
  /** List key → its rows' problems. */
  rows: Record<string, RowProblem[]>;
  /** The sections with a problem, in their order. */
  sections: string[];
  count: number;
}

/** Nothing typed: null, empty or blank text, an empty list. */
export function isBlank(v: unknown): boolean {
  if (v === null || v === undefined) return true;
  if (typeof v === 'string') return !v.trim();
  if (Array.isArray(v)) return !v.length;
  if (typeof v === 'number') return Number.isNaN(v);
  return false;
}

/** One field's problem, or null. A read-only field is the server's, not the
 *  user's to fill. A required secret of an existing object may stay empty
 *  (null keeps the one the server has). Number bounds are checked here, not
 *  by the control (it would change a server value as it shows it). With
 *  `choices`, a `list:` reference must name one of them (a row renamed or
 *  removed since), and so must a reference whose source names fields (a
 *  counter of another object); other sources aren't checked (the server may
 *  have values this login can't list). */
export function fieldProblem(field: FormField, value: unknown, isNew: boolean, choices?: [string, string][]): Problem | null {
  if (field.read_only) return null;
  const kind = field.kind;
  if (field.required && isBlank(value) && !(kind.type === 'secret' && !isNew)) return { code: 'required' };
  if (kind.type === 'number' && typeof value === 'number') {
    if (kind.min != null && value < kind.min) return { code: 'min', n: kind.min };
    if (kind.max != null && value > kind.max) return { code: 'max', n: kind.max };
  }
  if (kind.type === 'duration' && typeof value === 'number' && value < 0) return { code: 'min', n: 0 };
  const checked = kind.type === 'ref' && (kind.source.startsWith('list:') || sourceFields(kind.source).length > 0);
  if (checked && choices && !isBlank(value)) {
    const gone = (Array.isArray(value) ? value : [value]).map(whenText).find((v) => v !== '' && !choices.some(([c]) => c === v));
    if (gone !== undefined) return { code: 'missing', value: gone };
  }
  return null;
}

/** The choices a `ref` field's value must be one of where it is, or
 *  undefined when it isn't checked. */
export type RefsOf = (field: FormField, ctx: FormCtx) => [string, string][] | undefined;

/** The references the checks look at: a `list:` source's rows as they are
 *  now (`skip`: the row being checked, which can't name itself), and a
 *  source with `{field}`s while the driver listed something for it (with
 *  nothing listed, the field takes free text). */
export function refsToCheck(form: ObjectForm, lists: Record<string, FormRow[]>, skip?: RowRef): RefsOf {
  return (f, ctx) => {
    if (f.kind.type !== 'ref') return undefined;
    const src = f.kind.source;
    if (sourceFields(src).length) return refOptions(src, form, lists, ctx) ?? undefined;
    return src.startsWith('list:') ? refChoices(src, form, lists, skip) : undefined;
  };
}

/** One row's problems (its visible fields), in the order of the list's
 *  fields. `refs`: what its references are checked against. */
export function rowProblems(list: FormList, row: FormRow, top: FormCtx | undefined, isNew: boolean, refs?: RefsOf): { field: string; problem: Problem }[] {
  const ctx: FormCtx = { fields: list.fields, values: row, parent: top };
  const out: { field: string; problem: Problem }[] = [];
  for (const f of list.fields) {
    if (!isVisible(f, ctx)) continue;
    const p = fieldProblem(f, row[f.key], isNew, refs?.(f, ctx));
    if (p && !out.some((x) => x.field === f.key)) out.push({ field: f.key, problem: p });
  }
  return out;
}

/** The checks before a script or a save: required fields that show, list
 *  sizes, number bounds and references that still name one of their
 *  choices, rows included. Nothing is sent while there's one. Tabs and lists
 *  that don't show aren't checked (they aren't sent), nor are lists that
 *  can't be edited (the user couldn't fix them). */
export function validate(form: ObjectForm, values: Scope, lists: Record<string, FormRow[]>, isNew: boolean): Problems {
  const out: Problems = { fields: {}, lists: {}, rows: {}, sections: [], count: 0 };
  const top = topContext(form, values);
  const topRefs = refsToCheck(form, lists);
  for (const s of form.sections) {
    if (!sectionVisible(s, top)) continue;
    let bad = false;
    for (const f of s.fields ?? []) {
      if (!isVisible(f, top) || out.fields[f.key]) continue;
      const p = fieldProblem(f, values[f.key], isNew, topRefs(f, top));
      if (p) { out.fields[f.key] = p; out.count++; bad = true; }
    }
    for (const l of s.lists ?? []) {
      if (l.read_only || !listVisible(l, top)) continue;
      const rows = lists[l.key] ?? [];
      if (rows.length < (l.min_rows ?? 0)) { out.lists[l.key] = { code: 'minRows', n: l.min_rows ?? 0 }; out.count++; bad = true; }
      const rp: RowProblem[] = [];
      rows.forEach((r, i) => {
        const refs = refsToCheck(form, lists, { list: l.key, index: i });
        for (const p of rowProblems(l, r, top, isNew, refs)) rp.push({ row: i, ...p });
      });
      if (rp.length) { out.rows[l.key] = rp; out.count += rp.length; bad = true; }
    }
    if (bad) out.sections.push(s.key);
  }
  return out;
}

/** A value as it's sent: a secret left empty is null (keep the server's). */
function outValue(field: FormField | undefined, v: unknown): unknown {
  if (field?.kind.type === 'secret' && isBlank(v)) return null;
  return v === undefined ? null : v;
}

/** What goes to `script` and `save`: the top-level values and the rows of
 *  each list that shows, in their current order (a list that doesn't show
 *  — its `when` or its tab's — isn't sent: no key). Rows keep every key
 *  they came with (their ids: the driver tells an edited row from a new
 *  one); hidden fields keep their values (the driver ignores what doesn't
 *  apply). */
export function formValues(form: ObjectForm, values: Scope, lists: Record<string, FormRow[]>): FormValues {
  const fields = formFields(form);
  const out: FormValues = { values: {}, lists: {} };
  for (const [k, v] of Object.entries(values)) out.values[k] = outValue(fields.find((f) => f.key === k), v);
  for (const l of visibleLists(form, topContext(form, values))) {
    out.lists[l.key] = (lists[l.key] ?? []).map((r) => {
      const row: FormRow = {};
      for (const [k, v] of Object.entries(r)) row[k] = outValue(l.fields.find((f) => f.key === k), v);
      return row;
    });
  }
  return out;
}

/** A secret was typed (top level or a row's of a list that's sent): the
 *  script shows it as the driver's mask, so that script can't be run as it
 *  is — only "Guardar" writes the real value. */
export function hasTypedSecret(form: ObjectForm, values: Scope, lists: Record<string, FormRow[]>): boolean {
  const typed = (fields: FormField[], scope: Scope) => fields.some((f) => f.kind.type === 'secret' && !isBlank(scope[f.key]));
  return typed(formFields(form), values)
    || visibleLists(form, topContext(form, values)).some((l) => (lists[l.key] ?? []).some((r) => typed(l.fields, r)));
}

/** JSON with sorted object keys: two equal values give the same text. */
export function stableJson(v: unknown): string {
  return JSON.stringify(v, (_, x: unknown) => (x && typeof x === 'object' && !Array.isArray(x)
    ? Object.fromEntries(Object.entries(x as Record<string, unknown>).sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0)))
    : x));
}

/** The designer's state as text, to tell whether it changed since it was read. */
export function snapshot(form: ObjectForm, values: Scope, lists: Record<string, FormRow[]>): string {
  return stableJson(formValues(form, values, lists));
}

/** Up to three short columns a list's table shows besides the row's title. */
export function summaryFields(list: FormList): FormField[] {
  const short = new Set(['text', 'select', 'bool', 'number', 'ref', 'duration', 'time', 'date']);
  const out: FormField[] = [];
  for (const f of list.fields) {
    if (f.key === list.title_field || !short.has(f.kind.type) || out.some((o) => o.key === f.key)) continue;
    out.push(f);
    if (out.length === 3) break;
  }
  return out;
}

/** The rows with row `i` moved one place up (-1) or down (1). */
export function moveRow<T>(rows: T[], i: number, dir: -1 | 1): T[] {
  const j = i + dir;
  if (i < 0 || i >= rows.length || j < 0 || j >= rows.length) return rows;
  const out = rows.slice();
  [out[i], out[j]] = [out[j], out[i]];
  return out;
}

/** A row can be taken out: the list is editable and keeps its minimum. */
export function canRemoveRow(list: FormList, count: number, readOnly: boolean): boolean {
  return !readOnly && !list.read_only && count > (list.min_rows ?? 0);
}

/** A code language the editor draws as plain text (it has no grammar for it). */
const PLAIN: Language = 'redis';

/** How the code editor shows a `code` field: SQL in the engine's dialect
 *  (T-SQL and PL/SQL in theirs), JSON, everything else plain. */
export function codeMode(language: string, driverDialect: string): { language: Language; dialect: string } {
  const l = language.toLowerCase();
  if (l === 'tsql' || l === 't-sql' || l === 'mssql') return { language: 'sql', dialect: 'mssql' };
  if (l === 'plsql' || l === 'pl/sql' || l === 'oracle') return { language: 'sql', dialect: 'oracle' };
  if (l === 'plpgsql' || l === 'pgsql' || l === 'postgres') return { language: 'sql', dialect: 'postgres' };
  if (l === 'mysql') return { language: 'sql', dialect: 'mysql' };
  if (l === 'sql') return { language: 'sql', dialect: driverDialect };
  if (l === 'json') return { language: 'json', dialect: '' };
  return { language: PLAIN, dialect: '' };
}

/** An RFC 3339 date-time split into what the picker edits (local, without
 *  offset) and the offset to put back as it came ("Z", "-03:00" or ""). */
export function splitOffset(v: string): [string, string] {
  if (!v.includes('T')) return [v, ''];
  const m = /^(.*?)(Z|[+-]\d{2}:?\d{2})$/i.exec(v);
  return m ? [m[1], m[2]] : [v, ''];
}

/** What an object of the tree offers before the connection has its say (a
 *  read-only one offers no change). */
export interface Abilities {
  edit: boolean;
  delete: boolean;
  /** Its kind's actions it offers now, in the kind's order. */
  actions: SchedulerAction[];
}

/** An object's abilities: its own `can_edit` / `can_delete` where the
 *  driver sent them, else its kind's (the contract: absent is as the kind
 *  says, `system` objects included — a driver that protects one sends
 *  false), and its kind's actions, narrowed to the keys it lists. */
export function itemAbilities(
  kind: SchedulerKind | undefined,
  item?: Pick<SchedulerItem, 'can_edit' | 'can_delete' | 'actions'>,
): Abilities {
  const all = kind?.actions ?? [];
  const keys = item?.actions;
  return {
    edit: item?.can_edit ?? !!kind?.can_edit,
    delete: item?.can_delete ?? !!kind?.can_delete,
    actions: keys ? all.filter((a) => keys.includes(a.key)) : all,
  };
}

/** Seconds in words the reader takes in at a glance: "1 h 30 min", "45 s". */
export function humanDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '';
  const s = Math.round(seconds);
  const d = Math.floor(s / 86400);
  const h = Math.floor((s % 86400) / 3600);
  const m = Math.floor((s % 3600) / 60);
  const r = s % 60;
  const parts: string[] = [];
  if (d) parts.push(`${d} d`);
  if (h) parts.push(`${h} h`);
  if (m) parts.push(`${m} min`);
  if (r || !parts.length) parts.push(`${r} s`);
  return parts.join(' ');
}

/** The warnings a driver put in a script: comment lines whose text starts
 *  with "⚠" (`-- ⚠ …`, `# ⚠ …`, `// ⚠ …`, ` * ⚠ …`), in order, without the
 *  marker. The confirm dialog lists them and asks "Continuar" (a change that
 *  recreates an object or loses its history, a non-atomic edit…). */
export function scriptWarnings(script: string): string[] {
  const out: string[] = [];
  for (const line of script.split(/\r?\n/)) {
    const m = /^\s*(?:--|#|\/\/|\/\*|\*)?\s*⚠\uFE0F?\s*(.*?)\s*(?:\*\/)?\s*$/.exec(line);
    if (m && m[1]) out.push(m[1]);
  }
  return out;
}
