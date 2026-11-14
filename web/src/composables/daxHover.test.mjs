// Tests for the DAX [column] / [measure] hover (daxHover.ts): what the
// reference under the pointer stands for, a DEFINE's own expression, and a
// model measure's definition read into its parts. Run:
// `node --test src/composables/daxHover.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { register } from 'node:module';

// daxHover.ts imports its siblings without an extension (as Vite does).
register('data:text/javascript,' + encodeURIComponent(`
export async function resolve(spec, ctx, next) {
  if (spec.startsWith('.') && !/\\.[cm]?[jt]s$/.test(spec)) return next(spec + '.ts', ctx);
  return next(spec, ctx);
}`));
const { daxItemAt, measureDetails, resolveDaxItem } = await import('./daxHover.ts');

const KINDS = [
  { id: 'table', label: 'Tablas', has_columns: true, browsable: true, has_definition: true },
  { id: 'calculation_group', label: 'Grupos de cálculo', has_columns: true, browsable: true, has_definition: true },
  { id: 'measure', label: 'Medidas', has_columns: false, browsable: true, has_definition: true },
  { id: 'hierarchy', label: 'Jerarquías', has_columns: true, browsable: true, has_definition: true },
];
const obj = (name, kind = 'table', parent = null) => ({ kind, schema: null, name, parent });
const IDX = {
  objects: [
    obj('Sales'), obj('Customer'), obj('Sales Order'), obj('Time Intelligence', 'calculation_group'),
    obj('Total Sales', 'measure', 'Sales'), obj('Total', 'measure', 'Sales Order'), obj('Margin', 'measure', 'Customer'),
    obj("'Customer'[Geography]", 'hierarchy'),
  ],
  kinds: KINDS,
};
/** Each table's loaded columns; a table missing here isn't loaded yet. */
const COLUMNS = {
  Sales: ['Amount', 'CustomerKey', 'Total'],
  Customer: ['CustomerKey', 'City'],
  'Time Intelligence': ['Name', 'Ordinal'],
};
const loaded = (t) => COLUMNS[t.name] ?? null;

/** `|` marks the pointer: what the reference resolves to. */
function hover(text, columnsOf = loaded, idx = IDX) {
  const pos = text.indexOf('|');
  const doc = text.slice(0, pos) + text.slice(pos + 1);
  const ref = daxItemAt(idx, doc, pos);
  if (!ref) return null;
  const hit = resolveDaxItem(ref, columnsOf);
  const word = doc.slice(ref.from, ref.to);
  switch (hit.kind) {
    case 'measure': return { word, measure: hit.object.name, table: hit.object.parent };
    case 'column': return { word, column: hit.column, table: hit.table.name };
    case 'local': return { word, local: hit.def.kind, table: hit.def.table, expression: hit.def.expression };
    case 'loading': return { word, loading: hit.table.name };
    default: return { word, kind: hit.kind };
  }
}

test('a measure alone, and after its own table', () => {
  assert.deepEqual(hover('EVALUATE { [Total Sa|les] }'), { word: '[Total Sales]', measure: 'Total Sales', table: 'Sales' });
  assert.deepEqual(hover("EVALUATE { 'Sales Order'[To|tal] }"), { word: '[Total]', measure: 'Total', table: 'Sales Order' });
  // Case doesn't matter.
  assert.equal(hover('EVALUATE { [total sa|les] }').measure, 'Total Sales');
});

test("written after a table: that table's column before another table's measure", () => {
  // Sales has a column [Total]; the measure [Total] is Sales Order's.
  assert.deepEqual(hover("EVALUATE { SUM('Sales'[To|tal]) }"), { word: '[Total]', column: 'Total', table: 'Sales' });
  assert.deepEqual(hover('EVALUATE { SUM(Sales[amo|unt]) }'), { word: '[amount]', column: 'Amount', table: 'Sales' });
  // Not a column of Customer: the model's measure of that name.
  assert.deepEqual(hover("EVALUATE { 'Customer'[Total Sa|les] }"), { word: '[Total Sales]', measure: 'Total Sales', table: 'Sales' });
  // A calculation group's column.
  assert.equal(hover("EVALUATE { 'Time Intelligence'[Na|me] }").column, 'Name');
});

test("a table's columns not loaded yet: it waits for them", () => {
  assert.deepEqual(hover("EVALUATE { 'Sales Order'[Qu|antity] }"), { word: '[Quantity]', loading: 'Sales Order' });
  assert.deepEqual(hover("EVALUATE { 'Sales Order'[Qu|antity] }", (t) => (t.name === 'Sales Order' ? ['Quantity'] : null)), {
    word: '[Quantity]', column: 'Quantity', table: 'Sales Order',
  });
});

test('a column alone: the tables the unit names, in their order', () => {
  assert.deepEqual(hover('EVALUATE FILTER(Customer, [Ci|ty] = "Rosario")'), { word: '[City]', column: 'City', table: 'Customer' });
  // Both have CustomerKey: the first one named.
  assert.equal(hover("EVALUATE FILTER('Sales', [Customer|Key] > 1)").table, 'Sales');
  // Another unit's tables don't count.
  assert.deepEqual(hover('EVALUATE Customer\n\nEVALUATE { [Ci|ty] }'), null);
});

test('a column alone: the row context first, then its own EVALUATE or definition, then the unit', () => {
  // Sales and Customer both have CustomerKey; the FILTER iterates Customer,
  // though Sales is named before it.
  const filter = 'FILTER(Customer, [Customer|Key] > 1)';
  assert.deepEqual(hover(`DEFINE MEASURE Sales[Total] = SUM(Sales[Amount])\nEVALUATE ${filter}`), {
    word: '[CustomerKey]', column: 'CustomerKey', table: 'Customer',
  });
  assert.equal(hover(`EVALUATE SUMMARIZECOLUMNS(Sales[Amount], ${filter})`).table, 'Customer');
  assert.equal(hover(`DEFINE VAR x = 1\nEVALUATE Sales\nEVALUATE ${filter}`).table, 'Customer');
  // The closest iterator first, then the ones around it; the table's own
  // argument has no row context of it.
  assert.equal(hover('EVALUATE { SUMX(Customer, SUMX(FILTER(Sales, [Amount] > 0), [Customer|Key])) }').table, 'Sales');
  assert.equal(hover('EVALUATE { SUMX(Sales, SUMX(Customer, 1) + [Customer|Key]) }').table, 'Sales');
  assert.equal(hover('EVALUATE { SUMX(Sales, COUNTROWS(FILTER(Customer, [Customer|Key] > 1))) }').table, 'Customer');
  // TOPN's table is its second argument.
  assert.equal(hover("EVALUATE TOPN(COUNTROWS(Sales), 'Customer', [Customer|Key])").table, 'Customer');
  // The table argument may be a function of the table.
  assert.equal(hover('EVALUATE GENERATE(Sales, FILTER(ALL(Customer[City], Customer[CustomerKey]), [Customer|Key] > 1))').table, 'Customer');
  // No row context: the tables its own EVALUATE names, before an earlier one's.
  assert.equal(hover('DEFINE VAR x = 1\nEVALUATE Sales\nEVALUATE SUMMARIZECOLUMNS(Customer[City], "k", MAX([Customer|Key]))').table, 'Customer');
  // A DEFINE COLUMN's own table, before the rest of the unit's.
  assert.equal(hover('DEFINE MEASURE Sales[T] = 1\nCOLUMN Customer[Key2] = [Customer|Key] * 2\nEVALUATE Sales').table, 'Customer');
});

test('nothing known by that name', () => {
  assert.deepEqual(hover('EVALUATE FILTER(Customer, [Nowh|ere] = 1)'), { word: '[Nowhere]', kind: 'none' });
  assert.equal(hover("EVALUATE { 'Customer'[Nowh|ere] }").kind, 'none');
  // A table the model doesn't have and no measure: no popup.
  assert.equal(hover("EVALUATE { 'Elsewhere'[Nowh|ere] }"), null);
  assert.equal(hover('EVALUATE { [Nowh|ere] }'), null);
});

test('not a reference: tables, functions, strings, comments', () => {
  for (const text of ["EVALUATE 'Sa|les'", 'EVALUATE SU|M(x)', 'EVALUATE { "[To|tal Sales]" }', '// [To|tal Sales]', 'EVALUATE { 1|0 }']) {
    assert.equal(hover(text), null, text);
  }
});

test("the query's own measures and columns hide the model's", () => {
  const doc = [
    'DEFINE',
    "  MEASURE 'Sales'[Total Sales] =",
    '      VAR x = SUM ( Sales[Amount] )',
    '      RETURN x * 2',
    '  VAR Limit = 10',
    "  COLUMN 'Customer'[Upper City] = UPPER ( 'Customer'[City] )",
    "EVALUATE TOPN ( Limit, Customer, [Total Sa|les] )",
  ].join('\n');
  assert.deepEqual(hover(doc), {
    word: '[Total Sales]', local: 'measure', table: 'Sales', expression: 'VAR x = SUM ( Sales[Amount] )\nRETURN x * 2',
  });
  assert.deepEqual(hover(doc.replace('[Total Sa|les] )', "'Customer'[Upper Ci|ty] )")), {
    word: '[Upper City]', local: 'column', table: 'Customer', expression: "UPPER ( 'Customer'[City] )",
  });
  // A DEFINE ends at the next definition, a top VAR or the EVALUATE.
  const two = "DEFINE MEASURE Sales[A] = 1 + 2 MEASURE Sales[B] = [A] VAR v = 1\nEVALUATE { [|A], [B] }";
  assert.equal(hover(two).expression, '1 + 2');
  assert.equal(hover(two.replace('[|A], [B]', '[A], [|B]')).expression, '[A]');
  // A VAR after a RETURN is the expression's own nested block, not the
  // DEFINE's next VAR.
  const nested = [
    'DEFINE',
    '  MEASURE Sales[A] =',
    '      VAR a = 1',
    '      RETURN',
    '          VAR b = 2',
    '          RETURN a + b',
    '  VAR v = 3',
    '  FUNCTION f = ( x ) => VAR y = x RETURN y',
    '  MEASURE Sales[B] = 1',
    'EVALUATE { [|A] }',
  ].join('\n');
  assert.equal(hover(nested).expression, 'VAR a = 1\nRETURN\n    VAR b = 2\n    RETURN a + b');
  assert.equal(hover(nested.replace('[|A]', '[|B]')).expression, '1');
  // A DEFINE holds for every EVALUATE after it, not for the ones before.
  assert.equal(hover('DEFINE MEASURE Sales[Total Sales] = 1\nEVALUATE { 1 }\nEVALUATE { [Total Sa|les] }').local, 'measure');
  assert.equal(hover('EVALUATE { [Total Sa|les] }\n\nDEFINE MEASURE Sales[Total Sales] = 1\nEVALUATE { [Total Sales] }').measure, 'Total Sales');
});

test('a string of the unit names a column the query makes', () => {
  const doc = 'EVALUATE ADDCOLUMNS ( Customer, "Sales ""YTD""", [Total Sales] )\nORDER BY [Sales "YTD|"]';
  assert.deepEqual(hover(doc), { word: '[Sales "YTD"]', kind: 'query-column' });
  for (const f of ['SUMMARIZECOLUMNS ( Customer[City], "Rosario", 1 )', 'ROW ( "Rosario", 1 )', 'SELECTCOLUMNS ( Customer, "Rosario", [City] )']) {
    assert.equal(hover(`EVALUATE ${f}\nORDER BY [Ros|ario]`).kind, 'query-column', f);
  }
  // A filter's value names no column.
  assert.deepEqual(hover('EVALUATE FILTER ( Customer, Customer[City] = "Rosario" && [Ros|ario] > 0 )'), { word: '[Rosario]', kind: 'none' });
  assert.equal(hover('EVALUATE SUMMARIZECOLUMNS ( Customer[City], TREATAS ( { "Rosario" }, Customer[City] ), "n", 1 )\nORDER BY [Ros|ario]').kind, 'none');
});

test("a measure's definition: description, expression, type and format", () => {
  assert.deepEqual(measureDetails("// Suma de importes\nMEASURE 'Sales'[Total Sales] = SUM ( Sales[Amount] )\n// Tipo de dato: Decimal\n// Formato: $ #,0\n"), {
    description: 'Suma de importes', expression: 'SUM ( Sales[Amount] )', dataType: 'Decimal', format: '$ #,0',
  });
  // An expression of several lines, indented under its head; a name with `=`.
  assert.deepEqual(measureDetails("// Uno\n//\n// Dos\nMEASURE 'Sales'[A = B] =\n    VAR prev = 1\n    \n    RETURN\n        prev\n"), {
    description: 'Uno\n\nDos', expression: 'VAR prev = 1\n\nRETURN\n    prev', dataType: null, format: null,
  });
  // The user may not see the expression.
  assert.deepEqual(measureDetails("// MEASURE 'Sales'[Total Sales]: sin permiso para ver la expresión\n// Tipo de dato: Decimal\n"), {
    description: null, expression: null, dataType: 'Decimal', format: null,
  });
  // A description line that starts with MEASURE isn't the head.
  assert.deepEqual(measureDetails("// MEASURE of revenue\nMEASURE 'Sales'[Total Sales] = SUM ( Sales[Amount] )\n"), {
    description: 'MEASURE of revenue', expression: 'SUM ( Sales[Amount] )', dataType: null, format: null,
  });
  assert.deepEqual(measureDetails("// MEASURE of revenue\n// MEASURE 'Sales'[Total Sales]: sin permiso para ver la expresión\n"), {
    description: 'MEASURE of revenue', expression: null, dataType: null, format: null,
  });
});
