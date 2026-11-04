// Tests for KDB+'s q in the editor (qLanguage.ts) and q's regions in the
// shared lexer (sqlLexer.ts). Run: `node --test src/composables/qLanguage.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { EditorState } from '@codemirror/state';
import { CompletionContext } from '@codemirror/autocomplete';
import { StringStream } from '@codemirror/language';
import { qCompletion, qLanguage, qParser, qPath, statementAround, tablesNamed } from './qLanguage.ts';
import { lexRegions, lexSyntax, maskCode } from './sqlLexer.ts';

/** Each line's tokens as [text, style], blanks left out. */
function tokens(doc) {
  const st = qParser.startState(2);
  const out = [];
  for (const line of doc.split('\n')) {
    const s = new StringStream(line, 2, 2);
    while (!s.eol()) {
      const style = qParser.token(s, st);
      const text = s.current();
      if (text.trim()) out.push([text, style]);
      s.start = s.pos;
    }
  }
  return out;
}
const styleOf = (doc, text) => tokens(doc).find(([t]) => t === text)?.[1];

test('comments: / at the start or after a blank; over elsewhere', () => {
  assert.deepEqual(tokens('/ a note'), [['/ a note', 'comment']]);
  assert.deepEqual(tokens('x:1 / one'), [['x', 'variableName'], [':', 'operator'], ['1', 'number'], ['/ one', 'comment']]);
  assert.equal(styleOf('+/ 1 2 3', '/'), 'controlKeyword');
  assert.equal(styleOf('sum each x', 'each'), 'controlKeyword');
});

test('comment blocks and the end of the script', () => {
  const t = tokens('a:1\n/\nselect from t\n\\\nb:2\n\\\nanything at all');
  assert.deepEqual(t.filter(([, s]) => s === 'comment').map(([x]) => x), ['/', 'select from t', '\\', '\\', 'anything at all']);
  assert.equal(styleOf('a:1\n/\nx\n\\\nb:2', 'b'), 'variableName');
});

test('strings, symbols and file handles', () => {
  assert.deepEqual(tokens('"a\\"b" , `sym`a`b , `:data/hdb'), [
    ['"a\\"b"', 'string'], [',', 'operator'], ['`sym', 'variableName.special'], ['`a', 'variableName.special'], ['`b', 'variableName.special'],
    [',', 'operator'], ['`:data/hdb', 'string.special'],
  ]);
});

test('numbers with type suffixes, temporals, nulls and infinities', () => {
  for (const n of ['1j', '2.5e', '0x2a', '2026.11.03', '2026.11.03D12:00:00.000', '12:00:00.000', '2026.11m', '1e10', '.5']) {
    assert.deepEqual(tokens(n), [[n, 'number']], n);
  }
  for (const n of ['0N', '0w', '0Nd', '0Wp']) assert.deepEqual(tokens(n), [[n, 'null']], n);
  assert.deepEqual(tokens('101b'), [['101b', 'bool']]);
});

test('keywords, built-ins, namespaces and system commands', () => {
  const t = Object.fromEntries(tokens('select avg price by sym from trade where date=last date, .z.p > .Q.qp'));
  assert.equal(t.select, 'keyword');
  assert.equal(t.by, 'keyword');
  assert.equal(t.where, 'keyword');
  assert.equal(t.avg, 'variableName.function');
  assert.equal(t.last, 'variableName.function');
  assert.equal(t['.z.p'], 'variableName.function');
  assert.equal(t['.Q.qp'], 'variableName.function');
  assert.equal(t.trade, 'variableName');
  assert.equal(styleOf('.u.upd[`trade;x]', '.u.upd'), 'variableName');
  assert.equal(styleOf('\\l hdb.q', '\\l'), 'moduleKeyword');
  assert.equal(styleOf('\\t select from t', '\\t'), 'moduleKeyword');
  assert.equal(styleOf('f each x', 'f'), 'variableName');
});

test('the editor parses q and toggles comments with /', () => {
  const state = EditorState.create({ doc: 'x:1', extensions: [qLanguage] });
  assert.deepEqual(state.languageDataAt('commentTokens', 0)[0], { line: '/' });
});

// -- completion --------------------------------------------------------------------------------

const NAMES = {
  tables: { trade: ['time', 'sym', 'price', 'size'], quote: [], '.u.t': ['a'] },
  others: [{ label: '.u.upd', kind: 'function' }, { label: 'cfg', kind: 'variable' }],
};
function complete(text, explicit = false) {
  const pos = text.indexOf('|');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const state = EditorState.create({ doc, extensions: [qLanguage] });
  return qCompletion(new CompletionContext(state, pos, explicit), NAMES);
}
const labels = (r) => r.result?.options.map((o) => o.label) ?? [];

test('q words, built-ins, the connection names and the statement columns', () => {
  const r = complete('select pr| from trade');
  const l = labels(r);
  for (const x of ['price', 'sym', 'trade', 'quote', '.u.upd', 'cfg', 'select', 'avg', 'xbar', '.Q.qp', '.z.p']) assert.ok(l.includes(x), x);
  assert.equal(r.result.from, 'select '.length);
  assert.ok(!l.includes('a'), 'no columns of tables the statement does not name');
  assert.deepEqual(r.needColumns, []);
});

test('columns not loaded are asked for; symbols offer names only', () => {
  assert.deepEqual(complete('select from quote where b|').needColumns, ['quote']);
  const r = complete('`trade`s| xasc trade');
  assert.equal(r.result.from, '`trade`'.length);
  const l = labels(r);
  assert.ok(l.includes('sym') && l.includes('trade'));
  assert.ok(!l.includes('select') && !l.includes('.u.upd'));
});

test('nothing in a file handle or a number, nor unasked on nothing', () => {
  assert.equal(complete('`:da|').result, null);
  assert.equal(complete('12|').result, null);
  assert.equal(complete('x: |').result, null);
  assert.ok(labels(complete('x: |', true)).includes('trade'));
});

test('a statement continues on lines that start with a blank', () => {
  const doc = 'a:1\nselect from trade\n  where sym=`A\nb:2';
  assert.equal(statementAround(doc, doc.indexOf('where')), 'select from trade\n  where sym=`A');
  assert.deepEqual(tablesNamed('select from .u.t', ['t', '.u.t']), ['.u.t']);
});

test("a namespace's name is the explorer's path to it: [namespace, name]", () => {
  assert.deepEqual(qPath('.u.t'), ['.u', 't']);
  assert.deepEqual(qPath('.ns.a.b'), ['.ns', 'a.b']);
  assert.deepEqual(qPath('trade'), ['trade']);
  assert.deepEqual(qPath('.foo'), ['.foo']);
});

// -- the shared lexer ----------------------------------------------------------------------------

test("the lexer reads q: no ' strings, symbols aren't quotes, / comments", () => {
  const syn = lexSyntax('redis', 'q');
  const doc = "f'[x] , `a`b , \"s/x\" / note\n+/ 1 2\n/\nblock\n\\\nz";
  const kinds = lexRegions(doc, syn).filter((r) => r.kind !== 'code').map((r) => [r.kind, doc.slice(r.start, r.end)]);
  assert.deepEqual(kinds, [['string', '"s/x"'], ['comment', '/ note'], ['comment', '/\nblock\n\\']]);
  assert.equal(maskCode('x:1 / c', syn), 'x:1    ');
});
