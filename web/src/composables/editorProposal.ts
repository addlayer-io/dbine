// An edit the editor only proposes (the AI's corrected query): the text stays
// as it is and the proposal shows over it, removed text in red, added text in
// green, with Aceptar / Rechazar on each change and for all of them. Accepting
// edits the text (one undo step per click); rejecting leaves it untouched. Any
// edit the user makes while a proposal is pending drops it (it was made for
// the text as it was).
//
// Not `@codemirror/merge`'s unified view: that one diffs the *document*
// against an original, so the proposed text would have to sit in the
// document (and in the saved query's autosave) before being accepted.
import { Annotation, StateEffect, StateField, type EditorState, type Extension, type Range } from '@codemirror/state';
import { Decoration, EditorView, WidgetType, showPanel, type DecorationSet, type Panel } from '@codemirror/view';
import { diffChunks, type Chunk } from './textDiff';

export interface ProposalLabels {
  accept: string;
  reject: string;
  acceptAll: string;
  rejectAll: string;
  /** The bar's text ("Cambios propuestos por la IA"). */
  title: string;
}

interface Proposal {
  /** The text the proposal was made for, and where it starts in the document. */
  original: string;
  base: number;
  /** What is still undecided, in the document's positions. */
  chunks: Chunk[];
  labels: ProposalLabels;
}

/** Why a proposal ended: every change was decided (`done`), the user edited
 *  the text (`stale`) or the caller dropped it (`cancelled`). */
export type ProposalEnd = 'done' | 'stale' | 'cancelled';

const setProposal = StateEffect.define<Proposal>();
const endProposal = StateEffect.define<ProposalEnd>();
const dropChunk = StateEffect.define<number>();
/** On the transaction that accepts changes: their indexes. */
const accepting = Annotation.define<number[]>();

const field = StateField.define<Proposal | null>({
  create: () => null,
  update(value, tr) {
    for (const e of tr.effects) {
      if (e.is(setProposal)) return e.value;
      if (e.is(endProposal)) return null;
      if (e.is(dropChunk) && value) {
        const chunks = value.chunks.filter((_, k) => k !== e.value);
        return chunks.length ? { ...value, chunks } : null;
      }
    }
    if (!value || !tr.docChanged) return value;
    const taken = tr.annotation(accepting);
    // The user's own edit: the proposal was for other text.
    if (!taken) return null;
    const chunks = value.chunks
      .filter((_, k) => !taken.includes(k))
      .map((c) => ({ ...c, from: tr.changes.mapPos(c.from, 1), to: tr.changes.mapPos(c.to, 1) }));
    return chunks.length ? { ...value, chunks } : null;
  },
});

export function hasProposal(state: EditorState): boolean {
  return state.field(field, false) != null;
}

class ChunkWidget extends WidgetType {
  readonly index: number;
  readonly insert: string;
  readonly labels: ProposalLabels;
  constructor(index: number, insert: string, labels: ProposalLabels) {
    super();
    this.index = index;
    this.insert = insert;
    this.labels = labels;
  }
  eq(o: ChunkWidget) { return o.index === this.index && o.insert === this.insert && o.labels === this.labels; }
  toDOM(view: EditorView) {
    const wrap = document.createElement('span');
    wrap.className = 'cm-prop-chunk';
    if (this.insert) {
      const add = document.createElement('span');
      add.className = 'cm-prop-add';
      add.textContent = this.insert;
      wrap.append(add);
    }
    const mk = (cls: string, label: string, run: () => void) => {
      const b = document.createElement('button');
      b.type = 'button';
      b.className = `cm-prop-btn ${cls}`;
      b.textContent = label;
      // Keep the editor's focus and cursor.
      b.addEventListener('mousedown', (e) => e.preventDefault());
      b.addEventListener('click', run);
      return b;
    };
    wrap.append(
      mk('cm-prop-accept', this.labels.accept, () => acceptChunks(view, [this.index])),
      mk('cm-prop-reject', this.labels.reject, () => rejectChunk(view, this.index)),
    );
    return wrap;
  }
  ignoreEvent() { return true; }
}

function decorations(p: Proposal | null): DecorationSet {
  if (!p) return Decoration.none;
  const out: Range<Decoration>[] = [];
  p.chunks.forEach((c, k) => {
    if (c.to > c.from) out.push(Decoration.mark({ class: 'cm-prop-del' }).range(c.from, c.to));
    out.push(Decoration.widget({ widget: new ChunkWidget(k, c.insert, p.labels), side: 1 }).range(c.to));
  });
  return Decoration.set(out, true);
}

/** Accept the changes with these indexes: one transaction, one undo step. */
export function acceptChunks(view: EditorView, indexes: number[]) {
  const p = view.state.field(field, false);
  if (!p) return;
  const changes = indexes.map((k) => p.chunks[k]).filter(Boolean).map((c) => ({ from: c.from, to: c.to, insert: c.insert }));
  if (!changes.length) return;
  view.dispatch({ changes, annotations: accepting.of(indexes), scrollIntoView: true });
}

/** Reject one change: it goes away and the text stays as it is. */
export function rejectChunk(view: EditorView, index: number) {
  if (view.state.field(field, false)) view.dispatch({ effects: dropChunk.of(index) });
}

function bar(view: EditorView, p: Proposal): Panel {
  const dom = document.createElement('div');
  dom.className = 'cm-prop-bar';
  const title = document.createElement('span');
  title.className = 'cm-prop-title';
  title.textContent = p.labels.title;
  const mk = (cls: string, label: string, run: () => void) => {
    const b = document.createElement('button');
    b.type = 'button';
    b.className = `cm-prop-btn ${cls}`;
    b.textContent = label;
    b.addEventListener('mousedown', (e) => e.preventDefault());
    b.addEventListener('click', run);
    return b;
  };
  dom.append(
    title,
    mk('cm-prop-accept', p.labels.acceptAll, () => acceptAll(view)),
    mk('cm-prop-reject', p.labels.rejectAll, () => cancelProposal(view)),
  );
  return { dom, top: true };
}

const theme = EditorView.baseTheme({
  '.cm-prop-del': {
    background: 'color-mix(in srgb, var(--nm-danger) 22%, transparent)',
    textDecoration: 'line-through',
    textDecorationColor: 'color-mix(in srgb, var(--nm-danger) 70%, transparent)',
  },
  '.cm-prop-chunk': { display: 'inline' },
  '.cm-prop-add': {
    background: 'color-mix(in srgb, var(--nm-success) 25%, transparent)',
    whiteSpace: 'pre-wrap',
  },
  '.cm-prop-btn': {
    margin: '0 0 0 4px',
    padding: '0 6px',
    font: 'inherit',
    fontSize: '11px',
    lineHeight: '16px',
    cursor: 'pointer',
    borderRadius: '3px',
    color: 'var(--nm-text)',
    background: 'var(--ide-button-2, transparent)',
    border: '1px solid color-mix(in srgb, var(--nm-text) 25%, transparent)',
  },
  '.cm-prop-accept': { borderColor: 'color-mix(in srgb, var(--nm-success) 70%, transparent)' },
  '.cm-prop-reject': { borderColor: 'color-mix(in srgb, var(--nm-danger) 70%, transparent)' },
  '.cm-prop-bar': { display: 'flex', alignItems: 'center', gap: '4px', padding: '3px 8px', fontSize: '12px', borderBottom: '1px solid color-mix(in srgb, var(--nm-text) 15%, transparent)' },
  '.cm-prop-title': { flex: '1', color: 'var(--nm-text-dim)' },
});

/** The editor's extension. `onEnd` hears how a proposal ended. */
export function proposalExtension(onEnd: (why: ProposalEnd) => void): Extension {
  return [
    field,
    EditorView.decorations.from(field, decorations),
    showPanel.compute([field], (state) => {
      const p = state.field(field);
      return p ? (view) => bar(view, p) : null;
    }),
    theme,
    EditorView.updateListener.of((u) => {
      const before = u.startState.field(field);
      if (!before || u.state.field(field)) return;
      let why: ProposalEnd = 'stale';
      for (const tr of u.transactions) {
        for (const e of tr.effects) if (e.is(endProposal)) why = e.value;
        if (tr.annotation(accepting) || tr.effects.some((e) => e.is(dropChunk))) why = 'done';
      }
      onEnd(why);
    }),
  ];
}

/** Propose turning `original` (at `from` in the document) into `corrected`.
 *  False: nothing to propose (the same text) or the document no longer has
 *  `original` there. */
export function propose(view: EditorView, o: { from: number; original: string; corrected: string; labels: ProposalLabels }): boolean {
  if (view.state.sliceDoc(o.from, o.from + o.original.length) !== o.original) return false;
  const chunks = diffChunks(o.original, o.corrected).map((c) => ({ ...c, from: c.from + o.from, to: c.to + o.from }));
  if (!chunks.length) return false;
  view.dispatch({
    effects: [setProposal.of({ original: o.original, base: o.from, chunks, labels: o.labels }), EditorView.scrollIntoView(chunks[0].from, { y: 'center' })],
  });
  return true;
}

export function acceptAll(view: EditorView) {
  const p = view.state.field(field, false);
  if (p) acceptChunks(view, p.chunks.map((_, k) => k));
}

/** "Rechazar todo": the text was never touched, so it only goes away. */
export function cancelProposal(view: EditorView) {
  if (view.state.field(field, false)) view.dispatch({ effects: endProposal.of('cancelled') });
}
