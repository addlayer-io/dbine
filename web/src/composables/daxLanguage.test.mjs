// Tests for DAX in the editor (daxLanguage.ts): highlighting, the lexical
// helpers, name completion, and DAX in the shared lexer, the names under
// the pointer and the table hover. Run: `node --test src/composables/daxLanguage.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// daxLanguage.ts imports its siblings without an extension (as Vite does).
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { EditorState } = await import('@codemirror/state');
const { CompletionContext } = await import('@codemirror/autocomplete');
const { StringStream } = await import('@codemirror/language');
const {
  daxCompletion, daxDefinedNames, daxInName, daxLanguage, daxOtherNames, daxParser, daxRefAt, daxTableName, daxTablesNamed, daxTokens,
  daxUnitAround, daxUnitStarts,
} = await import('./daxLanguage.ts');
const { lexRegions, lexSyntax, maskCode } = await import('./sqlLexer.ts');
const { nameAt } = await import('./nameRefs.ts');
const { tableAt } = await import('./tableHover.ts');
const { commentMarkers, statementHead } = await import('./resultNames.ts');
const { paramMarkers, findParams } = await import('./queryParams.ts');
const { snippetFamily, snippetsFor } = await import('./snippets.ts');

// -- highlighting ---------------------------------------------------------------------------------

/** Each line's tokens as [text, style], blanks left out. */
function tokens(doc) {
  const st = daxParser.startState(2);
  const out = [];
  for (const line of doc.split('\n')) {
    const s = new StringStream(line, 2, 2);
    while (!s.eol()) {
      const style = daxParser.token(s, st);
      const text = s.current();
      if (text.trim()) out.push([text, style]);
      s.start = s.pos;
    }
  }
  return out;
}
const styleOf = (doc, text) => tokens(doc).find(([t]) => t === text)?.[1];

test('comments: //, -- and /* */ across lines', () => {
  assert.deepEqual(tokens('// a note'), [['// a note', 'comment']]);
  assert.deepEqual(tokens('-- a note'), [['-- a note', 'comment']]);
  assert.deepEqual(tokens('EVALUATE /* a\nlong */ x'), [['EVALUATE', 'keyword'], ['/* a', 'comment'], ['long */', 'comment'], ['x', 'variableName']]);
  assert.equal(daxLanguage.name, 'dax');
  assert.deepEqual(daxParser.languageData.commentTokens, { line: '//', block: { open: '/*', close: '*/' } });
});

test('strings with doubled quotes, across lines too', () => {
  assert.deepEqual(tokens('"say ""hi"""'), [['"say ""hi"""', 'string']]);
  assert.deepEqual(tokens('"one\ntwo" & x'), [['"one', 'string'], ['two"', 'string'], ['&', 'operator'], ['x', 'variableName']]);
});

test('table names, columns and measures are names, not strings', () => {
  assert.deepEqual(tokens("'Sales Order'[Amount]"), [["'Sales Order'", 'typeName'], ['[Amount]', 'propertyName']]);
  assert.deepEqual(tokens("'Bob''s'[a]]b]"), [["'Bob''s'", 'typeName'], ['[a]]b]', 'propertyName']]);
  assert.deepEqual(tokens('[Total Sales]'), [['[Total Sales]', 'propertyName']]);
  assert.equal(styleOf('Sales[Amount]', 'Sales'), 'variableName');
});

test('keywords, functions with dots, numbers, booleans and operators', () => {
  const t = Object.fromEntries(tokens('DEFINE VAR x = 1.5e3 EVALUATE INFO.VIEW.TABLES() ORDER BY [a] DESC'));
  assert.equal(t.DEFINE, 'keyword');
  assert.equal(t.VAR, 'keyword');
  assert.equal(t.EVALUATE, 'keyword');
  assert.equal(t.ORDER, 'keyword');
  assert.equal(t.BY, 'keyword');
  assert.equal(t.DESC, 'keyword');
  assert.equal(t['INFO.VIEW.TABLES'], 'variableName.function');
  assert.equal(t['1.5e3'], 'number');
  assert.equal(styleOf('STDEVX.S (t, [x])', 'STDEVX.S'), 'variableName.function');
  assert.equal(styleOf('evaluate t', 'evaluate'), 'keyword');
  assert.equal(styleOf('IF(TRUE, FALSE())', 'TRUE'), 'bool');
  assert.equal(styleOf('IF(TRUE, FALSE())', 'FALSE'), 'variableName.function');
  for (const op of ['&&', '||', '==', '<>', '<=', '>=', '&', '^']) assert.equal(styleOf(`a ${op} b`, op), 'operator', op);
  assert.equal(styleOf('{ 1, 2 }', '{'), 'bracket');
  assert.equal(styleOf('{ 1, 2 }', ','), 'separator');
});

// -- tokens and units ----------------------------------------------------------------------------

test('tokens: names and refs end at the line, strings and block comments run on', () => {
  const t = daxTokens("EVALUATE 'Sales'[Amount] \"a\"\"b\" // x\n[M]");
  assert.deepEqual(t.map((x) => [x.type, x.value]), [
    ['word', 'EVALUATE'], ['name', 'Sales'], ['ref', 'Amount'], ['string', ''], ['comment', ''], ['ref', 'M'],
  ]);
  assert.deepEqual(daxTokens("'Bob''s'[a]]b]").map((x) => x.value), ["Bob's", 'a]b']);
  const open = daxTokens("'Sales\n[Amo");
  assert.deepEqual(open.map((x) => [x.type, !!x.open]), [['name', true], ['ref', true]]);
  assert.equal(daxTokens('"one\ntwo').at(-1).open, true);
  assert.equal(daxTokens('/* one\ntwo').at(-1).type, 'comment');
});

test('units: DEFINE holds its EVALUATEs; bare EVALUATEs are each a unit', () => {
  const doc = '// first\nEVALUATE a\nEVALUATE b\nDEFINE MEASURE T[m] = 1\nEVALUATE c\nEVALUATE d\nDEFINE VAR v = 2\nEVALUATE e';
  const t = daxTokens(doc);
  const starts = daxUnitStarts(t).map((i) => t[i].value);
  assert.deepEqual(starts, ['EVALUATE', 'EVALUATE', 'DEFINE', 'DEFINE']);
  const unitOf = (word) => daxUnitAround(t, t.findIndex((x) => x.value === word)).filter((x) => x.type === 'word').map((x) => x.value).join(' ');
  assert.equal(unitOf('a'), 'EVALUATE a');
  assert.equal(unitOf('d'), 'DEFINE MEASURE T EVALUATE c EVALUATE d');
  assert.equal(unitOf('e'), 'DEFINE VAR v EVALUATE e');
  // Inside strings, names, refs, comments and parentheses, the words don't cut.
  const quiet = daxTokens("EVALUATE FILTER(t, \"EVALUATE\" <> 'EVALUATE'[EVALUATE] /* EVALUATE */)");
  assert.equal(daxUnitStarts(quiet).length, 1);
});

test('names a unit defines, and the tables it names', () => {
  const unit = daxTokens("DEFINE MEASURE 'Sales'[Margin] = 1 COLUMN Sales[Flag] = 2 TABLE Top10 = TOPN(10, Sales) VAR total = 3\nEVALUATE 'Date'");
  assert.deepEqual(daxDefinedNames(unit), { vars: ['total'], refs: ['Margin', 'Flag'], tables: ['Top10'] });
  assert.deepEqual(daxTablesNamed(unit, ['Sales', 'Date', 'Customer']), ['Sales', 'Date']);
  // A function's name isn't a table of that name.
  assert.deepEqual(daxTablesNamed(daxTokens('EVALUATE DATE(2024, 1, 1)'), ['Date']), []);
});

test('what a position names: a table, or a reference with its table', () => {
  const doc = "EVALUATE { SUM('Sales Order'[Amount]) + Sales [Cost] + [Total] } // 'x'";
  const span = (text) => ({ from: doc.indexOf(text), to: doc.indexOf(text) + text.length });
  const at = (text) => daxRefAt(doc, doc.indexOf(text) + 1);
  assert.deepEqual(at("'Sales Order'"), { kind: 'table', ...span("'Sales Order'"), name: 'Sales Order', quoted: true });
  assert.deepEqual(at('[Amount]'), { kind: 'ref', ...span('[Amount]'), name: 'Amount', table: 'Sales Order' });
  // DAX takes blanks between a table and its column.
  assert.deepEqual(at('[Cost]'), { kind: 'ref', ...span('[Cost]'), name: 'Cost', table: 'Sales' });
  assert.deepEqual(at('[Total]'), { kind: 'ref', ...span('[Total]'), name: 'Total', table: null });
  const bare = doc.indexOf('Sales [');
  assert.deepEqual(at('Sales ['), { kind: 'table', from: bare, to: bare + 5, name: 'Sales', quoted: false });
  // Right after a word still names it (the pointer at its end).
  assert.equal(daxRefAt(doc, bare + 5).name, 'Sales');
  for (const text of ['EVALUATE', 'SUM', "'x'", '{']) assert.equal(at(text), null, text);
  // A reference on the next line has no table.
  assert.equal(daxRefAt('Sales\n[x]', 7).table, null);
});

test('quoting a table name where DAX needs it', () => {
  assert.equal(daxTableName('Sales'), 'Sales');
  assert.equal(daxTableName('Sales Order'), "'Sales Order'");
  assert.equal(daxTableName("Bob's"), "'Bob''s'");
  assert.equal(daxTableName('Año'), "'Año'");
  assert.equal(daxTableName('Table'), "'Table'");
  assert.equal(daxTableName('Date', new Set(['DATE'])), "'Date'");
  assert.equal(daxTableName('2024'), "'2024'");
});

// -- completion ----------------------------------------------------------------------------------

const NAMES = {
  tables: { Sales: ['Amount', 'CustomerKey', 'Order]Date'], 'Sales Order': [], Date: ['Year', 'Month'], Customer: [] },
  others: [
    { label: 'Total Sales', kind: 'measure', parent: 'Sales' },
    { label: 'Margin %', kind: 'measure', parent: 'Sales' },
    { label: 'Customers', kind: 'measure', parent: 'Customer' },
    { label: 'Local.Discount', kind: 'function', parent: null },
    { label: "'Date'[Calendar]", kind: 'hierarchy', parent: 'Date' },
  ],
  functions: [{ name: 'DATE', kind: 'scalar', args: '', returns: '', description: '', schema: '' }, { name: 'NOT', kind: 'scalar', args: '', returns: '', description: '', schema: '' }],
};
NAMES.tables["'Date'[Calendar]"] = ['Year', 'Month'];

/** `‸` marks the cursor. */
function complete(text, { names = NAMES, explicit = false } = {}) {
  const pos = text.indexOf('‸');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const state = EditorState.create({ doc });
  return daxCompletion(new CompletionContext(state, pos, explicit), names);
}
const labels = (r) => (r.result ? r.result.options.map((o) => o.label) : []);
const option = (r, label) => r.result.options.find((o) => o.label === label);
const applied = (text, label, opts) => {
  const r = complete(text, opts);
  const o = option(r, label);
  const pos = text.indexOf('‸');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const to = r.result.to ?? pos;
  return doc.slice(0, r.result.from) + (typeof o.apply === 'string' ? o.apply : o.label) + doc.slice(to);
};

test("after ' : the model's tables, closed with their quote", () => {
  const r = complete("EVALUATE 'Sa‸");
  assert.deepEqual(labels(r), ['Sales', 'Sales Order', 'Date', 'Customer']);
  assert.equal(applied("EVALUATE 'Sa‸", 'Sales Order'), "EVALUATE 'Sales Order'");
  // The editor closed the quote already: it isn't doubled.
  assert.equal(applied("EVALUATE '‸'", 'Sales'), "EVALUATE 'Sales'");
  assert.equal(applied("EVALUATE '‸'[Amount]", 'Sales'), "EVALUATE 'Sales'[Amount]");
  // Editing a name's middle: the rest of it goes too.
  assert.equal(applied("EVALUATE 'Cu‸stomr'[Amount]", 'Customer'), "EVALUATE 'Customer'[Amount]");
  // A `'` typed before other code isn't paired: the quote ahead opens the
  // next name, and the code up to it stays.
  assert.equal(applied("EVALUATE '‸FILTER('X', 1)", 'Customer'), "EVALUATE 'Customer'FILTER('X', 1)");
  // A query's own DEFINE TABLE too; never the hierarchies.
  assert.ok(labels(complete("DEFINE TABLE Top10 = TOPN(10, Sales)\nEVALUATE '‸")).includes('Top10'));
  assert.ok(!labels(r).includes("'Date'[Calendar]"));
});

test("after 'Table'[ or Table[ : its columns, then its measures", () => {
  for (const text of ["EVALUATE { SUM('Sales'[‸", 'EVALUATE { SUM(Sales[‸', "EVALUATE { SUM('Sales' [A‸", 'EVALUATE { SUM(sales[‸']) {
    const r = complete(text);
    assert.deepEqual(labels(r), ['Amount', 'CustomerKey', 'Order]Date', 'Total Sales', 'Margin %'], text);
    assert.deepEqual(r.needColumns, [], text);
  }
  const r = complete("EVALUATE { SUM('Sales'[‸");
  assert.equal(option(r, 'Amount').type, 'property');
  assert.ok(option(r, 'Amount').boost > option(r, 'Total Sales').boost);
  // A `]` in a name is doubled; the closing one isn't added twice.
  assert.equal(applied("EVALUATE { SUM('Sales'[‸", 'Order]Date'), "EVALUATE { SUM('Sales'[Order]]Date]");
  assert.equal(applied("EVALUATE { SUM('Sales'[‸]) }", 'Amount'), "EVALUATE { SUM('Sales'[Amount]) }");
  // Columns not loaded yet: asked for.
  assert.deepEqual(complete("EVALUATE 'Customer'[‸").needColumns, ['Customer']);
  assert.deepEqual(labels(complete("EVALUATE 'Customer'[‸")), ['Customers']);
  // A quoted name the model doesn't have: nothing.
  assert.equal(complete("EVALUATE 'Nope'[‸").result, null);
});

test('after a bare [ : measures and the unit\'s own, then the columns of the tables it names', () => {
  const r = complete("DEFINE MEASURE Sales[Margin] = 1\nEVALUATE SUMMARIZECOLUMNS('Date'[Year], \"x\", [‸");
  // `Sales[Margin]` and `'Date'[Year]` name Sales and Date.
  assert.deepEqual(labels(r), ['Margin', 'Total Sales', 'Margin %', 'Customers', 'Amount', 'CustomerKey', 'Order]Date', 'Year', 'Month']);
  assert.equal(option(r, 'Total Sales').detail, 'Sales');
  assert.equal(option(r, 'Year').detail, 'Date');
  assert.equal(applied('EVALUATE { [To‸ }', 'Total Sales'), 'EVALUATE { [Total Sales] }');
  // The closing `]` the editor added, or the rest of the name being edited,
  // goes too; a `'` inside the name is the name's own.
  assert.equal(applied('EVALUATE { [‸] }', 'Total Sales'), 'EVALUATE { [Total Sales] }');
  assert.equal(applied("EVALUATE { [Last Ye‸ar's Sales] }", 'Total Sales'), 'EVALUATE { [Total Sales] }');
  // A `[` typed before another reference isn't paired: that code stays.
  assert.equal(applied("EVALUATE { [‸'Sales'[Amount] }", 'Total Sales'), "EVALUATE { [Total Sales]'Sales'[Amount] }");
  assert.equal(applied('EVALUATE { [‸Sales[Amount] }', 'Total Sales'), 'EVALUATE { [Total Sales]Sales[Amount] }');
  // After a keyword or a VAR, a reference on its own.
  assert.ok(labels(complete('EVALUATE ROW("a", 1) ORDER BY [‸')).includes('Total Sales'));
  assert.ok(labels(complete('DEFINE VAR x = 1 EVALUATE { x [‸')).includes('Total Sales'));
  // The tables named whose columns aren't loaded are asked for.
  assert.deepEqual(complete("EVALUATE FILTER('Customer', [‸").needColumns, ['Customer']);
});

test('a bare word: tables (quoted where needed), VAR names, measures in brackets, keywords', () => {
  const r = complete('DEFINE VAR total = 1\nEVALUATE { t‸', { explicit: false });
  const l = labels(r);
  for (const x of ['Sales', 'Sales Order', 'Date', 'Customer', 'total', 'Total Sales', 'Local.Discount', 'EVALUATE', 'ORDER BY']) assert.ok(l.includes(x), x);
  assert.ok(!l.includes("'Date'[Calendar]"));
  assert.equal(option(r, 'Sales').apply, undefined);
  assert.equal(option(r, 'Sales Order').apply, "'Sales Order'");
  assert.equal(option(r, 'Sales Order').displayLabel, "'Sales Order'");
  // A table named like a function of the catalog goes quoted.
  assert.equal(option(r, 'Date').apply, "'Date'");
  assert.equal(option(r, 'Total Sales').apply, '[Total Sales]');
  assert.equal(option(r, 'Total Sales').displayLabel, '[Total Sales]');
  assert.equal(option(r, 'EVALUATE').type, 'keyword');
  // The catalog's NOT stands for the keyword.
  assert.ok(!l.includes('NOT'));
  assert.ok(labels(complete('EVALUATE { N‸', { names: { ...NAMES, functions: null } })).includes('NOT'));
  // The bracket shift in the match highlight.
  assert.deepEqual(r.result.getMatch(option(r, 'Total Sales'), [0, 1]), [1, 2]);
  assert.deepEqual(r.result.getMatch(option(r, 'Sales'), [0, 1]), [0, 1]);
});

test('after DEFINE MEASURE or COLUMN: only the tables, the new name\'s own comes first', () => {
  const tables = ['Sales', 'Sales Order', 'Date', 'Customer'];
  for (const text of ['DEFINE MEASURE S‸', 'DEFINE COLUMN S‸', 'define measure S‸']) assert.deepEqual(labels(complete(text)), tables, text);
  assert.deepEqual(labels(complete('DEFINE MEASURE ‸', { explicit: true })), tables);
  // Written as DAX needs them, the query's own DEFINE TABLE too.
  const r = complete('DEFINE TABLE Top10 = TOPN(10, Sales)\nMEASURE T‸');
  assert.deepEqual(labels(r), [...tables, 'Top10']);
  assert.equal(option(r, 'Sales Order').apply, "'Sales Order'");
  assert.equal(option(r, 'Date').apply, "'Date'");
  // The definition's expression is an expression again.
  assert.ok(labels(complete("DEFINE MEASURE 'Sales'[M] = T‸")).includes('Total Sales'));
});

test('nothing in strings, comments, numbers, dotted names or where a name is given', () => {
  for (const text of [
    'EVALUATE { "Sa‸', '// Sa‸', '-- Sa‸', '/* Sa‸', 'EVALUATE { 12‸', 'EVALUATE INFO.V‸', 'DEFINE VAR to‸', 'DEFINE TABLE to‸',
    'DEFINE FUNCTION f‸', 'EVALUATE ‸', "EVALUATE 'Sales'‸",
    // A user-defined function's parameters and their types.
    'DEFINE FUNCTION f = (S‸', 'DEFINE FUNCTION f = (a : SCALAR, S‸', 'DEFINE FUNCTION f = (x : S‸', 'DEFINE FUNCTION Local.Add = (\n\ta,\n\tS‸',
  ]) {
    assert.equal(complete(text).result, null, text);
  }
  assert.equal(complete('DEFINE FUNCTION f = (‸', { explicit: true }).result, null);
  assert.ok(complete('EVALUATE ‸', { explicit: true }).result);
  // The function's body names the model again.
  assert.ok(labels(complete('DEFINE FUNCTION f = (a : SCALAR) => S‸')).includes('Sales'));
  assert.ok(labels(complete('DEFINE FUNCTION f = (a : SCALAR) => CALCULATE(S‸')).includes('Sales'));
});

test('snippets and parameters stay out of table names and references', () => {
  const at = (text, extensions = [daxLanguage]) => {
    const pos = text.indexOf('‸');
    const state = EditorState.create({ doc: text.slice(0, pos) + text.slice(pos + 1), extensions });
    return daxInName(state, pos);
  };
  for (const text of ['EVALUATE { [ca‸', "EVALUATE 'ev‸", "EVALUATE 'Sa‸les'[Amount]", 'EVALUATE { [To‸tal Sales] }', "EVALUATE { 'Sales'[‸ }", 'EVALUATE { [x]‸ }']) {
    assert.equal(at(text), true, text);
  }
  for (const text of ['EVALUATE ev‸', 'EVALUATE { ca‸ }', "EVALUATE { 'Sales'[x] + ca‸ }", "‸'Sales'", 'EVALUATE { [x] ‸}']) {
    assert.equal(at(text), false, text);
  }
  // Another language's editor: never.
  assert.equal(at("EVALUATE 'ev‸", []), false);
});

test("the explorer's objects as completion reads them", () => {
  const objs = [
    { kind: 'table', schema: null, name: 'Sales', parent: null },
    { kind: 'measure', schema: null, name: 'Total Sales', parent: 'Sales' },
    { kind: 'relationship', schema: null, name: "'Sales'[k] → 'Customer'[k]", parent: null },
    { kind: 'function', schema: null, name: 'Local.Discount', parent: null },
  ];
  assert.deepEqual(daxOtherNames(objs), [
    { label: 'Total Sales', kind: 'measure', parent: 'Sales' },
    { label: 'Local.Discount', kind: 'function', parent: null },
  ]);
});

// -- the shared lexer, names under the pointer, hover --------------------------------------------

test('the shared lexer reads DAX: names and refs are names, "" and // -- comments', () => {
  const syn = lexSyntax('dax', 'powerbi');
  const doc = "EVALUATE 'It''s'[a]]b] \"x\"\"y\" // c\n-- d\n:p";
  const kinds = lexRegions(doc, syn).filter((r) => r.kind !== 'code').map((r) => [r.kind, doc.slice(r.start, r.end)]);
  assert.deepEqual(kinds, [['ident', "'It''s'"], ['ident', '[a]]b]'], ['string', '"x""y"'], ['comment', '// c'], ['comment', '-- d']]);
  assert.equal(maskCode('"a" & "b"', syn), '" " & " "');
});

const KINDS = [
  { id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true },
  { id: 'measure', label: 'Medidas', has_columns: false, browsable: true, has_definition: true },
  { id: 'hierarchy', label: 'Jerarquías', has_columns: true, browsable: true, has_definition: true },
];
const OBJECTS = [
  { kind: 'table', schema: null, name: 'Sales', parent: null },
  { kind: 'table', schema: null, name: 'Sales Order', parent: null },
  { kind: 'measure', schema: null, name: 'Total', parent: 'Sales' },
  { kind: 'measure', schema: null, name: 'Total', parent: 'Sales Order' },
  { kind: 'hierarchy', schema: null, name: "'Sales'[Dates]", parent: 'Sales' },
];
const IDX = { language: 'dax', dialect: 'powerbi', database: 'Model', objects: OBJECTS, kinds: KINDS, schemas: [], columns: () => null };

/** `|` marks the pointer. */
function nameHere(text) {
  const pos = text.indexOf('|');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const hit = nameAt(IDX, doc, pos);
  return hit && { name: hit.object.name, kind: hit.object.kind, parent: hit.object.parent, view: hit.view, word: doc.slice(hit.from, hit.to) };
}

test('⌘-click: a table opens its structure, a measure its definition', () => {
  assert.deepEqual(nameHere("EVALUATE 'Sales Or|der'"), { name: 'Sales Order', kind: 'table', parent: null, view: 'structure', word: "'Sales Order'" });
  assert.deepEqual(nameHere('EVALUATE Sal|es'), { name: 'Sales', kind: 'table', parent: null, view: 'structure', word: 'Sales' });
  assert.equal(nameHere('EVALUATE sales|').name, 'Sales');
  assert.deepEqual(nameHere('EVALUATE { [To|tal] }'), { name: 'Total', kind: 'measure', parent: 'Sales', view: 'definition', word: '[Total]' });
  // Written after its table, the table's own measure of that name.
  assert.equal(nameHere("EVALUATE { 'Sales Order'[To|tal] }").parent, 'Sales Order');
  // A column: its table's structure.
  assert.deepEqual(nameHere("EVALUATE { SUM('Sales'[Amo|unt]) }"), { name: 'Sales', kind: 'table', parent: null, view: 'structure', word: '[Amount]' });
  for (const text of ['EVALUATE { [Amo|unt] }', 'EVALU|ATE Sales', 'EVALUATE SU|M(x)', 'EVALUATE { "Sa|les" }', '// Sa|les', "EVALUATE 'Dates|'"]) {
    assert.equal(nameHere(text), null, text);
  }
});

test('hover: only a table name shows its columns', () => {
  const at = (text) => {
    const pos = text.indexOf('|');
    const doc = text.slice(0, pos) + text.slice(pos + 1);
    const r = tableAt(IDX, doc, pos);
    return r && { name: r.object.name, word: doc.slice(r.from, r.to) };
  };
  assert.deepEqual(at("EVALUATE 'Sa|les Order'"), { name: 'Sales Order', word: "'Sales Order'" });
  assert.deepEqual(at('EVALUATE Sa|les'), { name: 'Sales', word: 'Sales' });
  assert.equal(at("EVALUATE { 'Sales'[Am|ount] }"), null);
  assert.equal(at('EVALUATE { [To|tal] }'), null);
});

// -- the rest of the editor: names of results, parameters, snippets ------------------------------

test('the comment above a unit names its result, with // and --', () => {
  const markers = commentMarkers('dax', 'powerbi');
  const doc = '// Totales\nEVALUATE x\n\n-- Por año\nEVALUATE y';
  assert.deepEqual(statementHead(doc, 0, 22, markers), { start: 11, name: 'Totales' });
  assert.deepEqual(statementHead(doc, 22, doc.length, markers), { start: 34, name: 'Por año' });
});

test('no :name parameters, and its own snippets', () => {
  const target = { language: 'dax', dialect: 'powerbi', driverId: 'powerbi' };
  assert.equal(paramMarkers(target), null);
  assert.deepEqual(findParams("EVALUATE FILTER(t, t[a] = :x)", target), []);
  const powerbi = { id: 'powerbi', language: 'dax', dialect: 'powerbi' };
  assert.equal(snippetFamily(powerbi), 'dax');
  const abbrs = snippetsFor(powerbi, []).map((s) => s.abbr);
  assert.deepEqual(abbrs, ['ev', 'def', 'sc', 'topn', 'calc', 'var', 'info', 'cmp']);
  // Every snippet body parses as CodeMirror's template (no nested fields).
  for (const s of snippetsFor(powerbi, [])) assert.ok(!/\$\{\d+:[^}]*\$\{/.test(s.body), s.abbr);
});
