// Tests for the editor's proposed edits (editorProposal.ts) on the editor
// state alone (no DOM): the text stays untouched until a change is accepted,
// accepting one change is one undo step, rejecting leaves the text as it
// was, and the user's own edit drops the proposal. Run: `node --test
// src/composables/editorProposal.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';
import { EditorState } from '@codemirror/state';
import { history, undo } from '@codemirror/commands';

register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { acceptAll, cancelProposal, hasProposal, propose, proposalExtension } = await import('./editorProposal.ts');
const { acceptChunks, rejectChunk } = await import('./editorProposal.ts');

const labels = { title: 't', accept: 'a', reject: 'r', acceptAll: 'A', rejectAll: 'R' };

/** The few things the functions use of a view, over a plain state. */
function fake(doc) {
  const v = {
    state: EditorState.create({ doc, extensions: [history(), proposalExtension(() => {})] }),
    dispatch(spec) { v.state = v.state.update(spec).state; },
  };
  return v;
}
const text = (v) => v.state.doc.toString();
const ORIGINAL = 'SELECT nmae, b FROM userz WHERE x = 1';
const FIXED = 'SELECT name, b FROM users WHERE x = 1';

test('proposing leaves the text untouched', () => {
  const v = fake(`-- hi\n${ORIGINAL}\n`);
  assert.equal(propose(v, { from: 6, original: ORIGINAL, corrected: FIXED, labels }), true);
  assert.ok(hasProposal(v.state));
  assert.equal(text(v), `-- hi\n${ORIGINAL}\n`);
});

test('nothing to propose when the text is not there or is the same', () => {
  const v = fake(ORIGINAL);
  assert.equal(propose(v, { from: 3, original: ORIGINAL, corrected: FIXED, labels }), false);
  assert.equal(propose(v, { from: 0, original: ORIGINAL, corrected: ORIGINAL, labels }), false);
  assert.equal(hasProposal(v.state), false);
});

test('"Rechazar todo" restores nothing because nothing changed', () => {
  const v = fake(ORIGINAL);
  propose(v, { from: 0, original: ORIGINAL, corrected: FIXED, labels });
  cancelProposal(v);
  assert.equal(hasProposal(v.state), false);
  assert.equal(text(v), ORIGINAL);
});

test('"Aceptar todo" edits the text in one undo step', () => {
  const v = fake(ORIGINAL);
  propose(v, { from: 0, original: ORIGINAL, corrected: FIXED, labels });
  acceptAll(v);
  assert.equal(text(v), FIXED);
  assert.equal(hasProposal(v.state), false);
  undo({ state: v.state, dispatch: (tr) => { v.state = tr.state; } });
  assert.equal(text(v), ORIGINAL);
});

test('the user typing drops the proposal and keeps what they typed', () => {
  const v = fake(ORIGINAL);
  propose(v, { from: 0, original: ORIGINAL, corrected: FIXED, labels });
  v.dispatch({ changes: { from: 0, insert: ' ' } });
  assert.equal(hasProposal(v.state), false);
  assert.equal(text(v), ` ${ORIGINAL}`);
});

test('changes are decided one by one, each accept its own undo step', () => {
  const v = fake(ORIGINAL);
  propose(v, { from: 0, original: ORIGINAL, corrected: FIXED, labels });
  // nmae -> name, userz -> users: two changes.
  acceptChunks(v, [0]);
  assert.equal(text(v), 'SELECT name, b FROM userz WHERE x = 1');
  assert.ok(hasProposal(v.state), 'the other change is still pending');
  acceptChunks(v, [0]);
  assert.equal(text(v), FIXED);
  assert.equal(hasProposal(v.state), false);
  undo({ state: v.state, dispatch: (tr) => { v.state = tr.state; } });
  assert.equal(text(v), 'SELECT name, b FROM userz WHERE x = 1');
});

test('a rejected change stays as it was while the other is accepted', () => {
  const v = fake(ORIGINAL);
  propose(v, { from: 0, original: ORIGINAL, corrected: FIXED, labels });
  rejectChunk(v, 0);
  assert.equal(text(v), ORIGINAL);
  acceptAll(v);
  assert.equal(text(v), 'SELECT nmae, b FROM users WHERE x = 1');
  assert.equal(hasProposal(v.state), false);
});
