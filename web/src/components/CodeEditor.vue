<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { EditorState, Compartment, type Extension } from '@codemirror/state';
import { EditorView, keymap, placeholder as cmPlaceholder } from '@codemirror/view';
import { basicSetup } from 'codemirror';
import { startCompletion, type Completion, type CompletionContext, type CompletionResult } from '@codemirror/autocomplete';
import { indentWithTab } from '@codemirror/commands';
import { syntaxTree } from '@codemirror/language';
import {
  PostgreSQL, MySQL, MariaSQL, MSSQL, SQLite, StandardSQL, PLSQL, Cassandra, type SQLDialect,
  keywordCompletionSource,
} from '@codemirror/lang-sql';
import { json } from '@codemirror/lang-json';
import { oneDark } from '@codemirror/theme-one-dark';
import { linter, lintGutter, type Diagnostic } from '@codemirror/lint';
import type { Language } from '../api/types';

// CodeMirror 6 wrapped for DBine: the language comes from the driver
// (`language` + `dialect`), `schema` feeds table/column completion.
// ⌘↵ runs the selection (or everything), ⌘⇧↵ the statement at the cursor,
// ⌘L shows the estimated plan, ⌘⇧L runs with the actual plan, ⌘S saves.

const props = withDefaults(defineProps<{
  modelValue: string;
  language?: Language;
  dialect?: string;
  readOnly?: boolean;
  /** table → columns, for completion; `schema.table` keys put the table
   *  under its schema. */
  schema?: Record<string, string[]>;
  placeholder?: string;
  /** "Calidad de código": the problems of a text (QueryView asks the
   *  backend). Null: no marks. */
  lint?: ((doc: string) => Promise<Diagnostic[]>) | null;
}>(), { language: 'sql', dialect: '', readOnly: false, schema: () => ({}), placeholder: '', lint: null });

const emit = defineEmits<{
  'update:modelValue': [value: string];
  /** ⌘↵: the selection (or everything); `from`: where it starts in the text. */
  run: [text: string, from: number];
  /** ⌘⇧↵: the statement at the cursor (`cursor`, an index into `doc`). */
  runStatement: [doc: string, cursor: number];
  /** ⌘L: estimated plan. ⌘⇧L: run with the actual plan. */
  plan: [text: string, actual: boolean, from: number];
  save: [];
  /** ⇧⌥F: format the code. */
  format: [];
  /** Completion reached a table whose columns aren't loaded: `[schema, table]`
   *  or `[table]`. The list reopens when `schema` brings them. */
  needColumns: [path: string[]];
}>();

const host = ref<HTMLDivElement | null>(null);
let view: EditorView | null = null;
const lang = new Compartment();
const ro = new Compartment();
const ph = new Compartment();
const lintC = new Compartment();

/** Gutter marks and squiggles, a moment after typing stops. */
function lintExt(): Extension {
  const source = props.lint;
  return source ? [linter((v) => source(v.state.doc.toString()), { delay: 600 }), lintGutter()] : [];
}

const DIALECTS: Record<string, SQLDialect> = {
  postgres: PostgreSQL, mysql: MySQL, mariadb: MariaSQL, mssql: MSSQL, sybase: MSSQL,
  sqlite: SQLite, oracle: PLSQL, cql: Cassandra,
};

/** Where a table name goes: right after FROM, JOIN, INTO, UPDATE or TABLE. */
const TABLE_SLOT = /\b(from|join|into|update|table)\s+$/i;

function tableSlot(ctx: CompletionContext): boolean {
  const word = ctx.matchBefore(/[\w$#]*/);
  const line = ctx.state.doc.lineAt(ctx.pos);
  return TABLE_SLOT.test(ctx.state.sliceDoc(line.from, word ? word.from : ctx.pos));
}

/** A name completion offers: a schema, a table or a column. */
interface NameNode {
  label: string;
  type: 'namespace' | 'class' | 'property';
  /** From the top: `[schema, table]`, `[table]`… */
  path: string[];
  children: NameNode[];
}

/** The schemas, tables and columns of `schema`. A name can be a schema and a
 *  table at once (`Person` and `Person.Person`): both stay, each with its own
 *  icon. */
function nameTree(schema: Record<string, string[]>): NameNode {
  const top: NameNode = { label: '', type: 'namespace', path: [], children: [] };
  const child = (parent: NameNode, label: string, type: NameNode['type']) => {
    let n = parent.children.find((c) => c.label === label && c.type === type);
    if (!n) parent.children.push((n = { label, type, path: [...parent.path, label], children: [] }));
    return n;
  };
  for (const [key, cols] of Object.entries(schema)) {
    const dot = key.indexOf('.');
    const table = dot < 0
      ? child(top, key, 'class')
      : child(child(top, key.slice(0, dot), 'namespace'), key.slice(dot + 1), 'class');
    for (const c of cols) child(table, c, 'property');
  }
  return top;
}

/** The children of `node` called `name`: the exact spelling, or else any
 *  case, since most engines don't tell `people` from `People`. */
function childrenNamed(node: NameNode, name: string): NameNode[] {
  const exact = node.children.filter((c) => c.label === name);
  if (exact.length) return exact;
  const lower = name.toLowerCase();
  return node.children.filter((c) => c.label.toLowerCase() === lower);
}

/** An identifier, bare or quoted ([x], "x", `x`). */
const ID = String.raw`(?:[\w$#]+|\[[^\]\n]+\]|"[^"\n]+"|\x60[^\x60\n]+\x60)`;
const PARENT_BEFORE = new RegExp(String.raw`(${ID})\s*\.\s*$`);
/** `FROM sales.orders o`, `JOIN people AS p`, `, items i`. */
const ALIAS = new RegExp(String.raw`(?:\b(?:from|join|update|into)|,)\s*(${ID}(?:\s*\.\s*${ID})*)\s+(?:as\s+)?(${ID})`, 'gi');
/** Words that follow a table name without being its alias. */
const NOT_ALIAS = new Set(
  'where on join inner left right full cross outer natural group order having union except intersect limit offset set values select from using with as'.split(' '),
);

function unquote(id: string): string {
  return /^[[\"`]/.test(id) ? id.slice(1, -1) : id;
}

/** The `a.b.` before `pos`, as `['a', 'b']`. */
function parentsBefore(text: string): string[] {
  const parents: string[] = [];
  for (let rest = text; ;) {
    const m = PARENT_BEFORE.exec(rest);
    if (!m) return parents;
    parents.unshift(unquote(m[1]));
    rest = rest.slice(0, m.index);
  }
}

/** alias (lower case) → the path it stands for. */
function aliasesIn(doc: string): Map<string, string[]> {
  const out = new Map<string, string[]>();
  for (const m of doc.matchAll(ALIAS)) {
    const alias = unquote(m[2]);
    if (NOT_ALIAS.has(alias.toLowerCase())) continue;
    out.set(alias.toLowerCase(), m[1].split(/\s*\.\s*/).map(unquote));
  }
  return out;
}

/** Schemas, tables and columns, with what comes after a dot resolved by
 *  name (`sales.`, `orders.`, an alias). A table whose columns aren't loaded
 *  yet asks for them, and the list reopens when they arrive. */
function nameSource(dialect: SQLDialect) {
  const tree = nameTree(props.schema);
  const asked = new Set<string>();
  // SQL Server takes "x" too, but [x] is what everyone writes there.
  const quotes = dialect.spec.identifierQuotes ?? '"';
  const open = quotes.includes('[') ? '[' : quotes[0];
  const close = open === '[' ? ']' : open;
  const option = (label: string, type: string): Completion =>
    /^[a-z_][\w$#]*$/i.test(label) ? { label, type } : { label, type, apply: open + label + close };

  return (ctx: CompletionContext): CompletionResult | null => {
    if (/String|Comment|QuotedIdentifier/.test(syntaxTree(ctx.state).resolveInner(ctx.pos, -1).name)) return null;
    const word = ctx.matchBefore(/[\w$#]*/) ?? { from: ctx.pos, text: '' };
    let parents = parentsBefore(ctx.state.sliceDoc(Math.max(0, word.from - 300), word.from));
    if (!parents.length && !word.text && !ctx.explicit) return null;
    const aliases = aliasesIn(ctx.state.doc.toString());
    if (parents.length === 1) parents = aliases.get(parents[0].toLowerCase()) ?? parents;

    let level = [tree];
    for (const name of parents) {
      level = level.flatMap((n) => childrenNamed(n, name));
      if (!level.length) return null;
    }
    if (parents.length && level.every((n) => !n.children.length)) {
      const table = level.find((n) => n.type === 'class');
      if (table && !asked.has(table.path.join('.'))) {
        asked.add(table.path.join('.'));
        reopenAt = ctx.pos;
        emit('needColumns', table.path);
      }
      return null;
    }

    const options = level.flatMap((n) => n.children).map((c) => option(c.label, c.type));
    if (!parents.length) for (const [alias] of aliases) options.push({ label: alias, type: 'variable' });
    return { from: word.from, options, validFor: /^[\w$#]*$/ };
  };
}

/** Where the cursor was when a table's columns were asked for. */
let reopenAt: number | null = null;

/** The SQL language with DBine's name completion, and keywords kept out of
 *  the list where a table name goes (or after a dot), so the names aren't
 *  buried under them. */
function sqlLanguage(dialect: SQLDialect): Extension {
  const keywords = keywordCompletionSource(dialect, true);
  const afterDot = (ctx: CompletionContext) => {
    const word = ctx.matchBefore(/[\w$#]*/);
    return /\.\s*$/.test(ctx.state.sliceDoc(Math.max(0, (word?.from ?? ctx.pos) - 2), word?.from ?? ctx.pos));
  };
  return [
    dialect.language,
    dialect.language.data.of({ autocomplete: nameSource(dialect) }),
    dialect.language.data.of({ autocomplete: (ctx: CompletionContext) => (tableSlot(ctx) || afterDot(ctx) ? null : keywords(ctx)) }),
  ];
}

function languageExt(): Extension {
  switch (props.language) {
    case 'json': return json();
    case 'cql': return sqlLanguage(Cassandra);
    case 'sql': return sqlLanguage(DIALECTS[props.dialect] ?? StandardSQL);
    default: return [];
  }
}

/** Typing the space after FROM/JOIN opens the table list without ⌃Space. */
const openTableList = EditorView.updateListener.of((u) => {
  if (!u.docChanged || !u.transactions.some((tr) => tr.isUserEvent('input.type'))) return;
  if (props.language !== 'sql' && props.language !== 'cql') return;
  if (!Object.keys(props.schema).length) return;
  const pos = u.state.selection.main.head;
  const line = u.state.doc.lineAt(pos);
  // Not from inside this update: the editor refuses a dispatch mid-update.
  if (TABLE_SLOT.test(u.state.sliceDoc(line.from, pos))) queueMicrotask(() => startCompletion(u.view));
});

/** The selection, or the whole text when nothing is selected. */
function runnableText(v: EditorView): string {
  return runnable(v).text;
}

/** The selection (or the whole text) and where it starts in the document. */
function runnable(v: EditorView): { text: string; from: number } {
  const sel = v.state.selection.main;
  return sel.empty ? { text: v.state.doc.toString(), from: 0 } : { text: v.state.sliceDoc(sel.from, sel.to), from: sel.from };
}

onMounted(() => {
  view = new EditorView({
    parent: host.value!,
    state: EditorState.create({
      doc: props.modelValue,
      extensions: [
        keymap.of([
          { key: 'Shift-Alt-f', preventDefault: true, run: () => { emit('format'); return true; } },
          { key: 'Mod-Enter', preventDefault: true, run: (v) => { const r = runnable(v); emit('run', r.text, r.from); return true; } },
          { key: 'Shift-Mod-Enter', preventDefault: true, run: (v) => { emit('runStatement', v.state.doc.toString(), v.state.selection.main.head); return true; } },
          { key: 'Mod-l', preventDefault: true, run: (v) => { const r = runnable(v); emit('plan', r.text, false, r.from); return true; } },
          { key: 'Shift-Mod-l', preventDefault: true, run: (v) => { const r = runnable(v); emit('plan', r.text, true, r.from); return true; } },
          { key: 'Mod-s', preventDefault: true, run: () => { emit('save'); return true; } },
          indentWithTab,
        ]),
        basicSetup,
        oneDark,
        lang.of(languageExt()),
        ro.of(EditorState.readOnly.of(props.readOnly)),
        ph.of(cmPlaceholder(props.placeholder)),
        lintC.of(lintExt()),
        EditorView.updateListener.of((u) => {
          if (u.docChanged) emit('update:modelValue', u.state.doc.toString());
        }),
        openTableList,
      ],
    }),
  });
});

onBeforeUnmount(() => view?.destroy());

// External changes (another tab loaded the query, a script was inserted).
watch(() => props.modelValue, (v) => {
  if (view && v !== view.state.doc.toString()) {
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: v } });
  }
});
watch(() => [props.language, props.dialect, props.schema], () => {
  if (!view) return;
  view.dispatch({ effects: lang.reconfigure(languageExt()) });
  // The columns asked for after a dot arrived: show them if the cursor stayed.
  const at = reopenAt;
  reopenAt = null;
  if (at !== null && view.state.selection.main.empty && view.state.selection.main.head === at) startCompletion(view);
});
watch(() => props.readOnly, (r) => view?.dispatch({ effects: ro.reconfigure(EditorState.readOnly.of(r)) }));
// The placeholder follows a language switch.
watch(() => props.placeholder, (p) => view?.dispatch({ effects: ph.reconfigure(cmPlaceholder(p)) }));
// Another connection or other rules: lint again with them.
watch(() => props.lint, () => view?.dispatch({ effects: lintC.reconfigure(lintExt()) }));

/** Add `text` at the end, on lines of its own, and show it selected. */
function appendText(text: string) {
  if (!view) return;
  const doc = view.state.doc;
  const body = text.replace(/\s+$/, '');
  const sep = doc.length === 0 ? '' : doc.toString().endsWith('\n\n') ? '' : doc.toString().endsWith('\n') ? '\n' : '\n\n';
  const from = doc.length + sep.length;
  view.dispatch({
    changes: { from: doc.length, insert: sep + body + '\n' },
    selection: { anchor: from, head: from + body.length },
    scrollIntoView: true,
  });
  view.focus();
}

/** Replace the whole text (one undoable change). */
function replaceAll(text: string) {
  if (!view) return;
  view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: text }, scrollIntoView: true });
  view.focus();
}

/** Format the selection (or everything) as one change that ⌘Z undoes.
 *  `fmt` turns text into formatted text; it may throw with the reason. */
async function format(fmt: (text: string) => Promise<string>) {
  if (!view) return;
  const sel = view.state.selection.main;
  const [from, to] = sel.empty ? [0, view.state.doc.length] : [sel.from, sel.to];
  const before = view.state.sliceDoc(from, to);
  const after = await fmt(before);
  if (after === before || !view) return;
  view.dispatch({ changes: { from, to, insert: after }, selection: { anchor: from, head: from + after.length } });
  view.focus();
}

defineExpose({
  focus: () => view?.focus(),
  runnableText: () => (view ? runnableText(view) : props.modelValue),
  runnable: () => (view ? runnable(view) : { text: props.modelValue, from: 0 }),
  /** The cursor, as an index into the text. */
  cursor: () => view?.state.selection.main.head ?? 0,
  /** 1-based line of a position of the text. */
  lineAt: (pos: number) => (view ? view.state.doc.lineAt(Math.min(Math.max(0, pos), view.state.doc.length)).number : 1),
  /** Put the cursor at `pos` (or at the start of 1-based `line`) and show it. */
  goTo: (o: { pos?: number | null; line?: number | null }) => {
    if (!view) return;
    const doc = view.state.doc;
    const at = o.pos != null ? Math.min(Math.max(0, o.pos), doc.length)
      : o.line != null ? doc.line(Math.min(Math.max(1, o.line), doc.lines)).from : null;
    if (at === null) return;
    view.dispatch({ selection: { anchor: at }, scrollIntoView: true });
    view.focus();
  },
  selectionText: () => {
    if (!view) return '';
    const r = view.state.selection.main;
    return r.empty ? '' : view.state.sliceDoc(r.from, r.to);
  },
  appendText,
  replaceAll,
  format,
});
</script>

<template>
  <div ref="host" class="ce" />
</template>

<style scoped>
.ce { height: 100%; min-height: 0; overflow: hidden; }
</style>
