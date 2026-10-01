<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { EditorState, Compartment, type Extension } from '@codemirror/state';
import { EditorView, keymap, placeholder as cmPlaceholder } from '@codemirror/view';
import { basicSetup } from 'codemirror';
import { startCompletion, type CompletionContext } from '@codemirror/autocomplete';
import { indentWithTab } from '@codemirror/commands';
import {
  PostgreSQL, MySQL, MariaSQL, MSSQL, SQLite, StandardSQL, PLSQL, Cassandra, type SQLDialect,
  keywordCompletionSource, schemaCompletionSource,
} from '@codemirror/lang-sql';
import { json } from '@codemirror/lang-json';
import { oneDark } from '@codemirror/theme-one-dark';
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
  /** table → columns, for completion. */
  schema?: Record<string, string[]>;
  placeholder?: string;
}>(), { language: 'sql', dialect: '', readOnly: false, schema: () => ({}), placeholder: '' });

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
}>();

const host = ref<HTMLDivElement | null>(null);
let view: EditorView | null = null;
const lang = new Compartment();
const ro = new Compartment();
const ph = new Compartment();

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

/** What `sql()` builds, except that keywords stay out of the list where a
 *  table name goes, so the tables aren't buried under them. */
function sqlLanguage(dialect: SQLDialect): Extension {
  const keywords = keywordCompletionSource(dialect, true);
  return [
    dialect.language,
    dialect.language.data.of({ autocomplete: schemaCompletionSource({ dialect, schema: props.schema }) }),
    dialect.language.data.of({ autocomplete: (ctx: CompletionContext) => (tableSlot(ctx) ? null : keywords(ctx)) }),
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
  view?.dispatch({ effects: lang.reconfigure(languageExt()) });
});
watch(() => props.readOnly, (r) => view?.dispatch({ effects: ro.reconfigure(EditorState.readOnly.of(r)) }));
// The placeholder follows a language switch.
watch(() => props.placeholder, (p) => view?.dispatch({ effects: ph.reconfigure(cmPlaceholder(p)) }));

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
