<script lang="ts">
import type { EditorView as View } from '@codemirror/view';
import { Vim } from '@replit/codemirror-vim';

// Shared by every editor: Vim's `:w` (defined once, for all of them) saves
// the editor it was typed in, and the editor that had the focus last runs
// the editor commands the palette asks for.
const saveOf = new WeakMap<View, () => void>();
Vim.defineEx('write', 'w', (cm) => saveOf.get(cm.cm6 as View)?.());
let lastClaim: (() => void) | null = null;
</script>

<script setup lang="ts">
import { onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { EditorState, Compartment, StateEffect, StateField, type Extension } from '@codemirror/state';
import { Decoration, EditorView, keymap, placeholder as cmPlaceholder, showTooltip, tooltips, type DecorationSet, type Tooltip } from '@codemirror/view';
import { basicSetup } from 'codemirror';
import {
  CompletionContext, closeCompletion, snippet, snippetCompletion, startCompletion, type Completion, type CompletionResult,
} from '@codemirror/autocomplete';
import { indentWithTab, redo, selectAll, toggleComment, undo } from '@codemirror/commands';
import { vim } from '@replit/codemirror-vim';
import { gotoLine, openSearchPanel } from '@codemirror/search';
import { syntaxTree } from '@codemirror/language';
import {
  PostgreSQL, MySQL, MariaSQL, MSSQL, SQLite, StandardSQL, PLSQL, Cassandra, type SQLDialect,
  keywordCompletionSource,
} from '@codemirror/lang-sql';
import { json } from '@codemirror/lang-json';
import { editorTheme } from '../composables/editorTheme';
import { editorFontSize, resolvedTheme } from '../composables/appearance';
import { linter, lintGutter, type Diagnostic } from '@codemirror/lint';
import type { Language } from '../api/types';
import { useTranslation } from 'i18next-vue';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import type { Snippet } from '../composables/snippets';
import { insertColumnAt } from '../composables/insertColumns';
import {
  fileCompletion, isTableSlot, nameCompletion, nameTree, quotePath, readerPathAt, readerSnippet, tableSlotAt,
  type NameNode,
} from '../composables/sqlCompletion';
import { VIM_SETTING, bindingLabel, commandForEvent, freedDefault, recordingKeys, registerCommand } from '../composables/commands';
import { useSettingsStore } from '../stores/settings';

// CodeMirror 6 wrapped for DBine: the language comes from the driver
// (`language` + `dialect`), `schema` feeds table/column completion.
// ⌘↵ runs the selection (or everything), ⌘⇧↵ the statement at the cursor,
// ⌘L shows the estimated plan, ⌘⇧L runs with the actual plan, ⌘S saves.
// Those keys come from the action registry (composables/commands.ts), so the
// user can change them; the editor runs its own commands (`editor` scope)
// before CodeMirror and Vim see the key.
//
// "Modo Vim en el editor" (setting `editor.vim`) turns Vim keys on in every
// editor at once; its status line shows the mode and takes `:` commands, and
// `:w` saves as ⌘S does.

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
  /** The right-click menu offers running, the plans and formatting (the
   *  parent handles `run`, `runStatement`, `plan` and `format`). */
  runActions?: boolean;
  /** More entries for the right-click menu, after the editor's own. */
  menuItems?: (() => MenuItem[]) | null;
  /** Snippets: abbreviation + Tab expands them; also offered by completion. */
  snippets?: Snippet[];
  /** The name at a position that ⌘/Ctrl+click opens (underlined while the
   *  key is down), or null. The parent handles `navigate`. */
  linkAt?: ((doc: string, pos: number) => { from: number; to: number } | null) | null;
  /** A "DataFrame SQL" cell: DuckDB's file readers in table
   *  positions, and inside their quotes "Elegir archivo…" (`pickFile`) and
   *  `grantedFiles` (composables/sqlCompletion.ts). */
  fileReaders?: boolean;
  grantedFiles?: string[];
  /** The native file dialog: the chosen path, or null. */
  pickFile?: (() => Promise<string | null>) | null;
}>(), {
  language: 'sql', dialect: '', readOnly: false, schema: () => ({}), placeholder: '', lint: null, runActions: false, menuItems: null, snippets: () => [], linkAt: null,
  fileReaders: false, grantedFiles: () => [], pickFile: null,
});

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
  /** Completion was asked for while `schema` is empty. The list reopens
   *  when `schema` brings names. */
  needNames: [];
  /** ⌘/Ctrl+click on a name `linkAt` knows. */
  navigate: [doc: string, pos: number];
}>();

const host = ref<HTMLDivElement | null>(null);
let view: EditorView | null = null;
const lang = new Compartment();
const ro = new Compartment();
const ph = new Compartment();
const lintC = new Compartment();
const vimC = new Compartment();

// -- Vim mode --------------------------------------------------------------------------------
const settings = useSettingsStore();
const vimOn = () => settings.get<boolean>(VIM_SETTING, false) === true;
/** The status line (mode, pending keys, the `:` prompt): small and quiet. */
const vimTheme = EditorView.theme({
  '.cm-panels-bottom': { borderTop: '1px solid var(--nm-border)' },
  '.cm-vim-panel': {
    padding: '1px 10px', minHeight: '18px', fontFamily: 'var(--nm-mono)', fontSize: '11px', lineHeight: '18px',
    color: 'var(--nm-text-dim)', display: 'flex', alignItems: 'center',
  },
  '.cm-vim-panel input': { fontFamily: 'var(--nm-mono)', fontSize: '12px', color: 'var(--nm-text-strong)' },
});
function vimExt(): Extension {
  return vimOn() ? [vim({ status: true }), vimTheme] : [];
}

/** Gutter marks and squiggles, a moment after typing stops. */
function lintExt(): Extension {
  const source = props.lint;
  return source ? [linter((v) => source(v.state.doc.toString()), { delay: 600 }), lintGutter()] : [];
}

const DIALECTS: Record<string, SQLDialect> = {
  postgres: PostgreSQL, mysql: MySQL, mariadb: MariaSQL, mssql: MSSQL, sybase: MSSQL,
  sqlite: SQLite, oracle: PLSQL, cql: Cassandra,
};

/** Schemas, tables and columns, with what comes after a dot resolved by
 *  name (`sales.`, `orders.`, an alias). A table whose columns aren't loaded
 *  yet asks for them, and the list reopens when they arrive; so does a list
 *  asked for before there are any names at all. */
function nameSource(dialect: SQLDialect) {
  // SQL Server takes "x" too, but [x] is what everyone writes there.
  const quotes = dialect.spec.identifierQuotes ?? '"';
  const open = quotes.includes('[') ? '[' : quotes[0];
  const close = open === '[' ? ']' : open;
  const quote = (label: string) => open + label + close;

  return (ctx: CompletionContext): CompletionResult | null => {
    const { result, needNames, needColumns } = nameCompletion(ctx, names().tree, quote);
    const n = names();
    if (needNames && !n.namesAsked) {
      // No names yet (the tab's connection isn't open): ask once; the list
      // opens when they arrive if the cursor stayed.
      n.namesAsked = true;
      reopenAt = ctx.pos;
      emit('needNames');
    }
    if (needColumns && !n.asked.has(needColumns.join('.'))) {
      n.asked.add(needColumns.join('.'));
      reopenAt = ctx.pos;
      emit('needColumns', needColumns);
    }
    return result;
  };
}

/** The tree of `schema`, built once per `schema` object. The completion
 *  sources read it when asked instead of being rebuilt with it: a new
 *  `schema` (a notebook's context recomputed, columns arriving) must not
 *  reconfigure the language, since CodeMirror then drops the open list and
 *  asks the new sources again as if nobody had asked (the list after
 *  FROM, opened by itself, closed at once). */
let namesOf: { schema: Record<string, string[]>; tree: NameNode; asked: Set<string>; namesAsked: boolean } | null = null;
function names() {
  if (namesOf?.schema !== props.schema) namesOf = { schema: props.schema, tree: nameTree(props.schema), asked: new Set(), namesAsked: false };
  return namesOf;
}

// -- DuckDB's file readers ("DataFrame SQL" cells) ------------------------------------------
/** A reader: the snippet `read_csv('|')`, then the list inside the quotes. */
function readerOption(name: string, format: string): Completion {
  const insert = snippet(readerSnippet(name));
  return {
    label: name, type: 'function', detail: format, boost: -1,
    apply: (v, c, from, to) => {
      insert(v, c, from, to);
      // Not from inside the pick's own transaction.
      setTimeout(() => startCompletion(v));
    },
  };
}

/** "Elegir archivo…": the native dialog, and its path between the quotes. */
function pickOption(at: { from: number; to: number }): Completion {
  return {
    label: t('editor:pickFile'), type: 'file', boost: 99,
    apply: (v) => {
      const pick = props.pickFile;
      if (!pick) return;
      void pick().then((path) => {
        if (!path || !view) return;
        // Where the quotes are now (the text may have moved meanwhile).
        const now = readerPathAt(new CompletionContext(v.state, v.state.selection.main.head, true)) ?? at;
        const insert = quotePath(path);
        const after = v.state.sliceDoc(now.to, now.to + 2);
        const end = now.from + insert.length + (after.startsWith("')") ? 2 : after.startsWith("'") ? 1 : 0);
        v.dispatch({ changes: { from: now.from, to: now.to, insert }, selection: { anchor: end }, userEvent: 'input.complete' });
        v.focus();
      });
    },
  };
}
const fileSource = (ctx: CompletionContext): CompletionResult | null =>
  fileCompletion(ctx, { enabled: props.fileReaders, grantedFiles: props.grantedFiles, pick: props.pickFile ? pickOption : null }, readerOption);

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
    dialect.language.data.of({ autocomplete: fileSource }),
    dialect.language.data.of({ autocomplete: (ctx: CompletionContext) => (tableSlotAt(ctx) || afterDot(ctx) ? null : keywords(ctx)) }),
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
  if (!Object.keys(props.schema).length && !props.fileReaders) return;
  const pos = u.state.selection.main.head;
  // Only right after the space (or comma) typed: not while typing a name.
  if (!/[\s,]$/.test(u.state.sliceDoc(pos - 1, pos))) return;
  // Not from inside this update: the editor refuses a dispatch mid-update.
  if (isTableSlot(u.state.sliceDoc(Math.max(0, pos - 2000), pos))) queueMicrotask(() => startCompletion(u.view));
});

// -- snippets ----------------------------------------------------------------------------------
/** The word before the cursor, when it can be an abbreviation (not after a
 *  dot or inside a longer name). */
function abbreviationBefore(state: EditorState, pos: number): { from: number; text: string } | null {
  const line = state.doc.lineAt(pos);
  const m = /[\w]+$/.exec(state.sliceDoc(line.from, pos));
  if (!m) return null;
  const from = pos - m[0].length;
  if (from > line.from && /[.\w$#@:]/.test(state.sliceDoc(from - 1, from))) return null;
  return { from, text: m[0] };
}

function inStringOrComment(state: EditorState, pos: number): boolean {
  return /String|Comment/.test(syntaxTree(state).resolveInner(pos, -1).name);
}

/** Tab after an abbreviation: the snippet replaces it, with its tab stops. */
function expandSnippet(v: EditorView): boolean {
  if (props.readOnly || !props.snippets.length) return false;
  const sel = v.state.selection.main;
  if (!sel.empty || inStringOrComment(v.state, sel.head)) return false;
  const word = abbreviationBefore(v.state, sel.head);
  if (!word) return false;
  const s = props.snippets.find((x) => x.abbr === word.text) ?? props.snippets.find((x) => x.abbr.toLowerCase() === word.text.toLowerCase());
  if (!s) return false;
  closeCompletion(v);
  snippet(s.body)(v, null, word.from, sel.head);
  return true;
}

/** The snippets as completion entries too (any language: not tied to one). */
const snippetSource = (ctx: CompletionContext): CompletionResult | null => {
  if (!props.snippets.length || props.readOnly || inStringOrComment(ctx.state, ctx.pos)) return null;
  const word = abbreviationBefore(ctx.state, ctx.pos);
  // Not after a dot (`sales.`, `sales.or`): a name goes there, not a snippet,
  // even when the list was opened explicitly.
  if (!word && (!ctx.explicit || /[.\w$#@:]$/.test(ctx.state.sliceDoc(Math.max(0, ctx.pos - 1), ctx.pos)))) return null;
  return {
    from: word?.from ?? ctx.pos,
    options: props.snippets.map((s) => snippetCompletion(s.body, { label: s.abbr, detail: s.detail, type: 'snippet', boost: -1 })),
    validFor: /^\w*$/,
  };
};

// -- ⌘/Ctrl+click on a name ---------------------------------------------------------------------
const setLink = StateEffect.define<{ from: number; to: number } | null>();
const linkMark = Decoration.mark({ class: 'cm-dbine-link' });
const linkField = StateField.define<DecorationSet>({
  create: () => Decoration.none,
  update(deco, tr) {
    for (const e of tr.effects) if (e.is(setLink)) return e.value ? Decoration.set([linkMark.range(e.value.from, e.value.to)]) : Decoration.none;
    return tr.docChanged ? Decoration.none : deco;
  },
  provide: (f) => EditorView.decorations.from(f),
});
const linkKey = (e: MouseEvent | KeyboardEvent) => (navigator.platform.toLowerCase().includes('mac') ? e.metaKey : e.ctrlKey);
let linkShown: { from: number; to: number } | null = null;
function showLink(v: EditorView, r: { from: number; to: number } | null) {
  if (r?.from === linkShown?.from && r?.to === linkShown?.to) return;
  linkShown = r;
  v.dispatch({ effects: setLink.of(r) });
}
// "Which column is this value?": with the cursor on a value of an INSERT's
// VALUES, a tooltip names its column and the column list highlights it
// (composables/insertColumns.ts). Without a list, the table's columns from
// `schema`, in order.
const insertMark = Decoration.mark({ class: 'cm-insert-col' });
/** Past this size, recomputing on each cursor move isn't worth it. */
const INSERT_HINT_MAX = 500_000;
function columnsOf(table: string): string[] | null {
  const want = table.toLowerCase();
  const last = want.split('.').pop()!;
  const entries = Object.entries(props.schema);
  return entries.find(([k]) => k.toLowerCase() === want)?.[1]
    ?? entries.find(([k]) => k.toLowerCase().split('.').pop() === last)?.[1]
    ?? null;
}
interface InsertHint { tooltip: Tooltip | null; deco: DecorationSet }
const noHint: InsertHint = { tooltip: null, deco: Decoration.none };
function insertHint(state: EditorState): InsertHint {
  const sel = state.selection.main;
  if (!sel.empty || state.doc.length > INSERT_HINT_MAX) return noHint;
  const r = insertColumnAt(state.doc.toString(), sel.head, props.language, props.dialect, columnsOf);
  if (!r) return noHint;
  const text = r.name
    ? t('editor:insertColumn', { name: r.name, n: r.index + 1, total: r.total })
    : t('editor:insertExtra', { n: r.index + 1, total: r.total });
  const tooltip: Tooltip = {
    pos: sel.head, above: true, strictSide: false, arrow: false,
    create: () => {
      const dom = document.createElement('div');
      dom.className = 'cm-insert-col-tip';
      dom.textContent = text;
      return { dom };
    },
  };
  const deco = r.from !== null && r.to !== null && r.to > r.from ? Decoration.set([insertMark.range(r.from, r.to)]) : Decoration.none;
  return { tooltip, deco };
}
const insertHints = StateField.define<InsertHint>({
  create: insertHint,
  update: (v, tr) => (tr.docChanged || tr.selection ? insertHint(tr.state) : v),
  provide: (f) => [showTooltip.from(f, (v) => v.tooltip), EditorView.decorations.from(f, (v) => v.deco)],
});
const insertTheme = EditorView.theme({
  '.cm-insert-col': { backgroundColor: 'color-mix(in srgb, var(--nm-accent) 28%, transparent)', borderRadius: '2px' },
  // Clicks go through it to the text it covers (the line above the cursor).
  '.cm-insert-col-tip': { padding: '2px 8px', fontSize: '12px', fontFamily: 'var(--nm-mono)', color: 'var(--nm-text)', pointerEvents: 'none' },
});

const links = [
  linkField,
  EditorView.domEventHandlers({
    mousemove(e, v) {
      if (!props.linkAt || !linkKey(e)) { if (linkShown) showLink(v, null); return false; }
      const pos = v.posAtCoords({ x: e.clientX, y: e.clientY });
      showLink(v, pos === null ? null : props.linkAt(v.state.doc.toString(), pos));
      return false;
    },
    mousedown(e, v) {
      if (!props.linkAt || e.button !== 0 || !linkKey(e)) return false;
      const pos = v.posAtCoords({ x: e.clientX, y: e.clientY });
      if (pos === null || !props.linkAt(v.state.doc.toString(), pos)) return false;
      e.preventDefault();
      showLink(v, null);
      emit('navigate', v.state.doc.toString(), pos);
      return true;
    },
    keyup(_e, v) { if (linkShown) showLink(v, null); return false; },
    mouseleave(_e, v) { if (linkShown) showLink(v, null); return false; },
  }),
  EditorView.baseTheme({
    '.cm-dbine-link': { textDecoration: 'underline', cursor: 'pointer', color: 'var(--nm-syn-link)' },
    '.cm-completionIcon-snippet:after': { content: "'⧉'" },
    '.cm-completionIcon-file:after': { content: "'⋯'" },
  }),
];

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
        // Before the keymaps: Vim sees the keys first (its own `Esc`, `j`…).
        vimC.of(vimExt()),
        keymap.of([
          { key: 'Tab', run: expandSnippet },
          indentWithTab,
        ]),
        basicSetup,
        // Completions and hints live in <body>, not inside the editor's
        // box: there they open upwards when the cursor is on the last
        // lines, instead of being cut off or hidden under the results.
        tooltips({ parent: document.body }),
        themeC.of(editorTheme(resolvedTheme.value)),
        lang.of(languageExt()),
        ro.of(EditorState.readOnly.of(props.readOnly)),
        ph.of(cmPlaceholder(props.placeholder)),
        lintC.of(lintExt()),
        EditorView.updateListener.of((u) => {
          if (u.docChanged) emit('update:modelValue', u.state.doc.toString());
        }),
        openTableList,
        EditorState.languageData.of(() => [{ autocomplete: snippetSource }]),
        links,
        insertHints,
        insertTheme,
      ],
    }),
  });
  saveOf.set(view, () => emit('save'));
  host.value!.addEventListener('keydown', onKeyCapture, true);
  host.value!.addEventListener('keydown', onKeyBubble);
  host.value!.addEventListener('focusin', claimCommands);
});

onBeforeUnmount(() => {
  releaseCommands();
  if (lastClaim === releaseCommands) lastClaim = null;
  view?.destroy();
});

// -- the editor's commands -----------------------------------------------------------------------
/** What each `editor` command does in this editor. */
const editorCommands: Record<string, (v: EditorView) => void> = {
  'editor.run': (v) => { const r = runnable(v); emit('run', r.text, r.from); },
  'editor.runStatement': (v) => emit('runStatement', v.state.doc.toString(), v.state.selection.main.head),
  'editor.planEstimated': (v) => { const r = runnable(v); emit('plan', r.text, false, r.from); },
  'editor.planActual': (v) => { const r = runnable(v); emit('plan', r.text, true, r.from); },
  'editor.format': () => emit('format'),
  'file.save': () => emit('save'),
  'editor.comment': (v) => { if (!props.readOnly) toggleComment(v); },
  'editor.find': (v) => { openSearchPanel(v); },
  'editor.gotoLine': (v) => { gotoLine(v); },
  'editor.autocomplete': (v) => { startCompletion(v); },
};

/** Before CodeMirror (and Vim): the registry's editor keys run here, in any
 *  Vim mode. */
function onKeyCapture(e: KeyboardEvent) {
  if (!view || recordingKeys.value) return;
  const id = commandForEvent(e, 'editor');
  const run = id ? editorCommands[id] : undefined;
  if (run) {
    e.preventDefault();
    e.stopPropagation();
    run(view);
  } else if (freedDefault(e, 'editor')) {
    // Its action moved to other keys: CodeMirror's own binding stays quiet.
    e.preventDefault();
    e.stopPropagation();
  }
}

/** In Vim mode, Esc belongs to the editor (back to NORMAL): it doesn't close
 *  the dialog or panel around it. */
function onKeyBubble(e: KeyboardEvent) {
  if (e.key === 'Escape' && vimOn()) e.stopPropagation();
}

/** The palette (and any other caller of `runCommand`) runs the editor
 *  commands on the editor that had the focus last. */
let releases: (() => void)[] = [];
function releaseCommands() {
  releases.forEach((f) => f());
  releases = [];
}
function claimCommands() {
  if (releases.length || !view) return;
  const alive = () => !!view && document.body.contains(view.dom) && view.dom.offsetParent !== null;
  releases = Object.entries(editorCommands).map(([id, run]) => registerCommand(id, () => { if (view) { run(view); view.focus(); } }, alive));
  // The newest registration wins: give way when another editor takes the focus.
  lastClaim?.();
  lastClaim = releaseCommands;
}

// Vim on or off, in every editor, as soon as the setting changes.
watch(vimOn, () => view?.dispatch({ effects: vimC.reconfigure(vimExt()) }));

// Theme and font size (Configuración › General) follow at once.
const themeC = new Compartment();
watch(resolvedTheme, (t) => view?.dispatch({ effects: themeC.reconfigure(editorTheme(t)) }));
watch(editorFontSize, () => view?.requestMeasure());

// External changes (another tab loaded the query, a script was inserted).
watch(() => props.modelValue, (v) => {
  if (view && v !== view.state.doc.toString()) {
    view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: v } });
  }
});
watch(() => [props.language, props.dialect], () => {
  view?.dispatch({ effects: lang.reconfigure(languageExt()) });
});
// New names: the sources read them when asked (names()), nothing to rebuild.
watch(() => props.schema, () => {
  if (!view) return;
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

// -- right-click menu ------------------------------------------------------------------------
const { t } = useTranslation();
const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
const isMac = navigator.platform.toLowerCase().includes('mac');
const key = (k: string) => (isMac ? k : k.replace(/⌘/g, 'Ctrl+').replace(/⇧/g, 'Shift+').replace(/⌥/g, 'Alt+'));
/** The keys the user gave the command (they may not be the default). */
const keyOf = (id: string) => bindingLabel(id);

async function copySelection(cut: boolean) {
  if (!view) return;
  const r = view.state.selection.main;
  if (r.empty) return;
  try { await navigator.clipboard.writeText(view.state.sliceDoc(r.from, r.to)); } catch { return; }
  if (cut && !props.readOnly) view.dispatch(view.state.replaceSelection(''));
  view.focus();
}
async function paste() {
  if (!view || props.readOnly) return;
  let text = '';
  try { text = await navigator.clipboard.readText(); } catch { return; }
  if (text) view.dispatch(view.state.replaceSelection(text));
  view.focus();
}
function act(fn: (v: EditorView) => boolean | void) {
  return () => { if (view) { fn(view); view.focus(); } };
}

function openMenu(e: MouseEvent) {
  if (!view) return;
  // Right-click outside the selection moves the cursor there, as editors do.
  const at = view.posAtCoords({ x: e.clientX, y: e.clientY });
  const sel = view.state.selection.main;
  if (at !== null && (sel.empty || at < sel.from || at > sel.to)) view.dispatch({ selection: { anchor: at } });
  const empty = view.state.selection.main.empty;
  const rw = !props.readOnly;
  const items: MenuItem[] = [];
  if (rw) {
    items.push({ label: t('editor:menu.undo'), shortcut: key('⌘Z'), action: act(undo) });
    items.push({ label: t('editor:menu.redo'), shortcut: key('⇧⌘Z'), action: act(redo) });
    items.push({ label: t('editor:menu.cut'), shortcut: key('⌘X'), disabled: empty, divided: true, action: () => copySelection(true) });
  }
  items.push({ label: t('editor:menu.copy'), shortcut: key('⌘C'), disabled: empty, divided: !rw, action: () => copySelection(false) });
  if (rw) items.push({ label: t('editor:menu.paste'), shortcut: key('⌘V'), action: paste });
  items.push({ label: t('editor:menu.selectAll'), shortcut: key('⌘A'), action: act(selectAll) });
  if (props.runActions) {
    items.push({ label: t(empty ? 'editor:menu.runAll' : 'editor:menu.runSelection'), shortcut: keyOf('editor.run'), divided: true, action: () => { const r = runnable(view!); emit('run', r.text, r.from); } });
    items.push({ label: t('editor:menu.runStatement'), shortcut: keyOf('editor.runStatement'), action: () => emit('runStatement', view!.state.doc.toString(), view!.state.selection.main.head) });
    items.push({ label: t('editor:menu.planEstimated'), shortcut: keyOf('editor.planEstimated'), action: () => { const r = runnable(view!); emit('plan', r.text, false, r.from); } });
    items.push({ label: t('editor:menu.planActual'), shortcut: keyOf('editor.planActual'), action: () => { const r = runnable(view!); emit('plan', r.text, true, r.from); } });
  }
  if (rw) {
    items.push({ label: t('editor:menu.comment'), shortcut: keyOf('editor.comment'), divided: true, action: act(toggleComment) });
    if (props.runActions) items.push({ label: t('editor:menu.format'), shortcut: keyOf('editor.format'), action: () => emit('format') });
  }
  items.push({ label: t('editor:menu.find'), shortcut: keyOf('editor.find'), divided: !rw, action: act(openSearchPanel) });
  items.push({ label: t('editor:menu.gotoLine'), shortcut: keyOf('editor.gotoLine'), action: act(gotoLine) });
  const extra = props.menuItems?.() ?? [];
  if (extra.length) items.push({ ...extra[0], divided: true }, ...extra.slice(1));
  menu.value = { x: e.clientX, y: e.clientY, items };
}

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
  <div ref="host" class="ce" @contextmenu.prevent="openMenu" />
  <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
</template>

<style scoped>
.ce { height: 100%; min-height: 0; overflow: hidden; }
</style>
