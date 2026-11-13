// Tests for the scheduler designer's form logic. Run:
// `node --test src/composables/schedulerForm.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  actionText, canRemoveRow, codeMode, fieldOptions, fieldProblem, fillSource, formValues, hasTypedSecret, humanDuration,
  initialLists, initialValues, isVisible, itemAbilities, listVisible, moveRow, newRow, optionLabel, problemText,
  refChoices, refOptions, refsToCheck, renameRefs, rowProblems, scriptWarnings, sectionVisible, snapshot, sourceFields, splitOffset,
  summaryFields, topContext, validate, visibleLists, whenText,
} from './schedulerForm.ts';

const f = (key, type = 'text', { kind = {}, ...extra } = {}) => ({ key, label: key, kind: { type, ...kind }, ...extra });

// A job-like form: a schedule's frequency fields hang from `freq`, the
// interval from the sub-day type (a `when` under a `when`), steps refer to
// each other by name.
const steps = {
  key: 'steps', label: 'Pasos', title_field: 'name', orderable: true, min_rows: 1,
  fields: [
    f('name', 'text', { required: true }),
    f('subsystem', 'select', { kind: { options: [['TSQL', 'T-SQL'], ['PowerShell', 'PowerShell']] }, default: 'TSQL' }),
    f('database', 'ref', { kind: { source: 'database' }, when: ['subsystem', ['TSQL']], required: true }),
    f('command', 'code', { kind: { language: 'tsql' }, when: ['subsystem', ['TSQL']], required: true }),
    f('command', 'code', { kind: { language: 'powershell' }, when: ['subsystem', ['PowerShell']], required: true }),
    f('on_success', 'select', { kind: { options: [['3', 'Ir al paso siguiente'], ['4', 'Ir al paso:']] }, default: '3' }),
    f('on_success_step', 'ref', { kind: { source: 'list:steps' }, when: ['on_success', ['4']], required: true }),
    f('retries', 'number', { kind: { min: 0, max: 10 }, default: 0 }),
  ],
  rows: [
    { step_id: 1, name: 'Backup', subsystem: 'TSQL', database: 'ventas', command: 'BACKUP DATABASE ventas', on_success: '3', retries: 0 },
    { step_id: 2, name: 'Avisar', subsystem: 'PowerShell', command: 'Send-Mail', on_success: '4', on_success_step: 'Backup', retries: 1 },
  ],
};
const form = () => ({
  sections: [
    {
      key: 'general', label: 'General',
      fields: [
        f('name', 'text', { required: true }),
        f('enabled', 'bool', { default: true }),
        f('freq', 'select', { kind: { options: [['once', 'Una vez'], ['daily', 'Diaria']] }, default: 'daily' }),
        f('sub', 'select', { kind: { options: [['once', 'Una vez'], ['every', 'Cada']] }, when: ['freq', ['daily']], default: 'every' }),
        f('every', 'number', { kind: { min: 1, max: 100 }, when: ['sub', ['every']], required: true }),
        f('notify_off', 'text', { when: ['enabled', ['false']] }),
        f('password', 'secret', { required: true }),
        f('created', 'text', { read_only: true, required: true }),
      ],
    },
    { key: 'steps', label: 'Pasos', lists: [steps] },
  ],
  values: { name: 'Backup nocturno', enabled: true, freq: 'daily', sub: 'every', every: 2, password: 'nope' },
  choices: { database: [['ventas', 'ventas'], ['crm', 'crm']] },
});

test('when compares as text, booleans as true / false, lists by any item', () => {
  assert.equal(whenText(true), 'true');
  assert.equal(whenText(false), 'false');
  assert.equal(whenText(4), '4');
  assert.equal(whenText(null), '');
  const fl = form();
  const ctx = { fields: fl.sections[0].fields, values: { enabled: false } };
  assert.equal(isVisible(fl.sections[0].fields[5], ctx), true);
  ctx.values.enabled = true;
  assert.equal(isVisible(fl.sections[0].fields[5], ctx), false);
  const multi = { key: 'x', label: 'x', kind: { type: 'text' }, when: ['days', ['mon']] };
  assert.equal(isVisible(multi, { fields: [f('days', 'multi_select')], values: { days: ['sun', 'mon'] } }), true);
  assert.equal(isVisible(multi, { fields: [f('days', 'multi_select')], values: { days: [] } }), false);
});

test('a field under a hidden field is hidden too', () => {
  const fl = form();
  const fields = fl.sections[0].fields;
  const every = fields.find((x) => x.key === 'every');
  assert.equal(isVisible(every, { fields, values: { freq: 'daily', sub: 'every' } }), true);
  // "once" hides the sub-day type, so its interval goes even though `sub` still says every.
  assert.equal(isVisible(every, { fields, values: { freq: 'once', sub: 'every' } }), false);
});

test('a row field looks up its own row first, then the form', () => {
  const fl = form();
  const top = { fields: fl.sections[0].fields, values: { enabled: true } };
  const db = steps.fields[2];
  assert.equal(isVisible(db, { fields: steps.fields, values: { subsystem: 'TSQL' }, parent: top }), true);
  assert.equal(isVisible(db, { fields: steps.fields, values: { subsystem: 'PowerShell' }, parent: top }), false);
  const fromTop = { key: 'y', label: 'y', kind: { type: 'text' }, when: ['enabled', ['true']] };
  assert.equal(isVisible(fromTop, { fields: steps.fields, values: {}, parent: top }), true);
  // A cycle doesn't hang nor hide.
  const a = { key: 'a', label: 'a', kind: { type: 'text' }, when: ['b', ['1']] };
  const b = { key: 'b', label: 'b', kind: { type: 'text' }, when: ['a', ['1']] };
  assert.equal(isVisible(a, { fields: [a, b], values: { a: '1', b: '1' } }), true);
});

test('a new object starts from the defaults, an existing one from its values; secrets never come', () => {
  const fl = form();
  const fresh = initialValues({ ...fl, values: {} }, true);
  assert.equal(fresh.enabled, true);
  assert.equal(fresh.freq, 'daily');
  assert.equal(fresh.password, null);
  assert.equal('every' in fresh, false);
  const old = initialValues({ ...fl, values: { name: 'x' } }, false);
  assert.deepEqual(old, { name: 'x', password: null });
  // Copies: editing them leaves the form as it came.
  const lists = initialLists(fl);
  lists.steps[0].name = 'changed';
  assert.equal(fl.sections[1].lists[0].rows[0].name, 'Backup');
});

test('a new row takes the fields\' defaults', () => {
  assert.deepEqual(newRow(steps), { subsystem: 'TSQL', on_success: '3', retries: 0 });
});

test('a list source offers the rows as edited now, by number and name', () => {
  const fl = form();
  const lists = initialLists(fl);
  assert.deepEqual(refChoices('list:steps', fl, lists), [['Backup', '1 · Backup'], ['Avisar', '2 · Avisar']]);
  lists.steps = moveRow(lists.steps, 1, -1);
  lists.steps.push({ name: '' });
  assert.deepEqual(refChoices('list:steps', fl, lists), [['Avisar', '1 · Avisar'], ['Backup', '2 · Backup']]);
  assert.deepEqual(refChoices('database', fl, lists), [['ventas', 'ventas'], ['crm', 'crm']]);
  assert.deepEqual(refChoices('nope', fl, lists), []);
  assert.deepEqual(refChoices('list:nope', fl, lists), []);
});

test('a row isn\'t offered to its own list\'s references, and keeps its number', () => {
  const fl = form();
  const lists = initialLists(fl);
  lists.steps.push({ name: 'Limpiar' });
  assert.deepEqual(refChoices('list:steps', fl, lists, { list: 'steps', index: 1 }), [['Backup', '1 · Backup'], ['Limpiar', '3 · Limpiar']]);
  // A row being added isn't in the list yet: every row is offered.
  assert.equal(refChoices('list:steps', fl, lists, { list: 'steps', index: null }).length, 3);
  // Another list's row doesn't take anything out.
  assert.equal(refChoices('list:steps', fl, lists, { list: 'schedules', index: 0 }).length, 3);
});

test('an option\'s label, "" included', () => {
  const proxy = f('proxy', 'ref', { kind: { source: 'proxy:CmdExec' } });
  const agent = [['', 'Cuenta de servicio del Agente SQL Server'], ['etl', 'Proxy ETL']];
  assert.equal(optionLabel(proxy, '', agent), 'Cuenta de servicio del Agente SQL Server');
  assert.equal(optionLabel(proxy, 'etl', agent), 'Proxy ETL');
  assert.equal(optionLabel(proxy, 'gone', agent), null);
  assert.equal(optionLabel(proxy, null, agent), null);
  assert.equal(optionLabel(proxy, '', [['etl', 'Proxy ETL']]), null);
  const sel = steps.fields[5];
  assert.equal(optionLabel(sel, '4'), 'Ir al paso:');
  assert.equal(optionLabel(sel, 3), 'Ir al paso siguiente');
  assert.deepEqual(fieldOptions(sel), sel.kind.options);
  assert.deepEqual(fieldOptions(proxy, agent), agent);
  assert.deepEqual(fieldOptions(f('t', 'text'), agent), []);
});

test('renaming a row renames what refers to it, everywhere', () => {
  const fl = form();
  fl.sections[1].fields = [f('start_step', 'ref', { kind: { source: 'list:steps' } })];
  const values = { ...initialValues(fl, false), start_step: 'Backup' };
  const lists = initialLists(fl);
  lists.steps[0].name = 'Backup completo';
  lists.steps.push({ name: 'Limpiar', on_success: '3', on_success_step: 'Backup' }); // hidden: renamed too
  const out = renameRefs(fl, values, lists, 'steps', 'Backup', 'Backup completo');
  assert.equal(out.count, 3);
  assert.equal(out.values.start_step, 'Backup completo');
  assert.equal(out.lists.steps[1].on_success_step, 'Backup completo');
  assert.equal(out.lists.steps[2].on_success_step, 'Backup completo');
  // Copies: what was passed stays as it was; rows without a reference are the same rows.
  assert.equal(values.start_step, 'Backup');
  assert.equal(lists.steps[1].on_success_step, 'Backup');
  assert.equal(out.lists.steps[0], lists.steps[0]);
  // A multiple reference renames its item.
  const multi = { sections: [{ key: 'g', label: 'g', fields: [f('after', 'ref', { kind: { source: 'list:steps', multiple: true } })], lists: [steps] }] };
  const m = renameRefs(multi, { after: ['Avisar', 'Backup'] }, { steps: [{ name: 'Avisar' }, { name: 'Backup 2' }] }, 'steps', 'Backup', 'Backup 2');
  assert.deepEqual(m.values.after, ['Avisar', 'Backup 2']);
  // Nothing when another row still has the old title, or nothing changed.
  const dup = { steps: [{ name: 'Backup' }, { name: 'Nuevo' }] };
  assert.equal(renameRefs(fl, values, dup, 'steps', 'Backup', 'Nuevo').count, 0);
  assert.equal(renameRefs(fl, values, lists, 'steps', 'Backup', 'Backup').count, 0);
  assert.equal(renameRefs(fl, values, lists, 'steps', '', 'x').count, 0);
  assert.equal(renameRefs(fl, values, lists, 'nope', 'Backup', 'x').count, 0);
});

test('a reference to a row that isn\'t there (removed, renamed or itself) is a problem', () => {
  const fl = form();
  fl.sections[1].fields = [f('start_step', 'ref', { kind: { source: 'list:steps' }, required: true })];
  const values = { ...initialValues(fl, false), start_step: 'Backup' };
  const lists = initialLists(fl);
  assert.equal(validate(fl, values, lists, false).count, 0);
  // "Backup" removed: the start step and "Avisar"'s next step name nothing.
  lists.steps = lists.steps.filter((r) => r.name !== 'Backup');
  lists.steps.push({ name: 'Otro', subsystem: 'PowerShell', command: 'x', on_success: '3' });
  const p = validate(fl, values, lists, false);
  assert.deepEqual(p.fields.start_step, { code: 'missing', value: 'Backup' });
  assert.deepEqual(p.rows.steps, [{ row: 0, field: 'on_success_step', problem: { code: 'missing', value: 'Backup' } }]);
  assert.deepEqual(p.sections, ['steps']);
  // A step that goes to itself.
  lists.steps[0].on_success_step = 'Avisar';
  assert.deepEqual(validate(fl, { ...values, start_step: 'Avisar' }, lists, false).rows.steps,
    [{ row: 0, field: 'on_success_step', problem: { code: 'missing', value: 'Avisar' } }]);
  lists.steps[0].on_success_step = 'Otro';
  assert.equal(validate(fl, { ...values, start_step: 'Avisar' }, lists, false).count, 0);
  // Other sources aren't checked: the server may have what this login can't list.
  assert.equal(fieldProblem(f('db', 'ref', { kind: { source: 'database' } }), 'hidden', false, [['ventas', 'ventas']]), null);
  assert.equal(fieldProblem(f('s', 'ref', { kind: { source: 'list:steps' } }), null, false, []), null);
});

test('a typed secret is noticed, top level or in a row', () => {
  const fl = form();
  const lists = initialLists(fl);
  assert.equal(hasTypedSecret(fl, initialValues(fl, false), lists), false);
  assert.equal(hasTypedSecret(fl, { ...initialValues(fl, false), password: '  ' }, lists), false);
  assert.equal(hasTypedSecret(fl, { ...initialValues(fl, false), password: 's3cret' }, lists), true);
  const withRowSecret = { sections: [{ key: 'g', label: 'g', lists: [{ ...steps, fields: [...steps.fields, f('pwd', 'secret')] }] }] };
  assert.equal(hasTypedSecret(withRowSecret, {}, { steps: [{ name: 'a', pwd: null }] }), false);
  assert.equal(hasTypedSecret(withRowSecret, {}, { steps: [{ name: 'a', pwd: 'x' }] }), true);
});

test('problems read through the UI\'s translate function', () => {
  const t = (k, o) => `${k}${o ? JSON.stringify(o) : ''}`;
  assert.equal(problemText({ code: 'required' }, t), 'scheduled:designer.errors.required');
  assert.equal(problemText({ code: 'max', n: 9 }, t), 'scheduled:designer.errors.max{"n":9}');
  assert.equal(problemText({ code: 'minRows', n: 2 }, t), 'scheduled:designer.errors.minRows{"count":2}');
  assert.equal(problemText({ code: 'missing', value: 'Backup' }, t), 'scheduled:designer.errors.missing{"value":"Backup"}');
});

test('validation: required fields that show, bounds, list minimum and rows', () => {
  const fl = form();
  const values = initialValues(fl, false);
  const lists = initialLists(fl);
  let p = validate(fl, values, lists, false);
  // The secret of an existing object may stay empty; a read-only field isn't checked.
  assert.equal(p.count, 0, JSON.stringify(p));

  values.name = '  ';
  values.every = 500;
  lists.steps[1].on_success_step = null;
  lists.steps[0].command = '';
  p = validate(fl, values, lists, false);
  assert.deepEqual(p.fields.name, { code: 'required' });
  assert.deepEqual(p.fields.every, { code: 'max', n: 100 });
  assert.deepEqual(p.rows.steps, [
    { row: 0, field: 'command', problem: { code: 'required' } },
    { row: 1, field: 'on_success_step', problem: { code: 'required' } },
  ]);
  assert.deepEqual(p.sections, ['general', 'steps']);
  assert.equal(p.count, 4);

  // A hidden field isn't required; an empty list under its minimum is.
  values.name = 'x';
  values.freq = 'once';
  lists.steps = [];
  p = validate(fl, values, lists, false);
  assert.equal(p.fields.every, undefined);
  assert.deepEqual(p.lists.steps, { code: 'minRows', n: 1 });
  assert.deepEqual(p.sections, ['steps']);

  // A new object needs its required secret.
  p = validate(fl, { ...values, password: '' }, initialLists(fl), true);
  assert.deepEqual(p.fields.password, { code: 'required' });
});

test('field problems', () => {
  assert.deepEqual(fieldProblem(f('n', 'number', { kind: { min: 1 } }), 0, false), { code: 'min', n: 1 });
  assert.equal(fieldProblem(f('n', 'number', { kind: { min: 1 } }), null, false), null);
  assert.deepEqual(fieldProblem(f('d', 'duration'), -5, false), { code: 'min', n: 0 });
  assert.deepEqual(fieldProblem(f('m', 'multi_select', { required: true }), [], false), { code: 'required' });
  assert.equal(fieldProblem(f('b', 'bool', { required: true }), false, false), null);
});

test('what goes back: every key, lists in their order, empty secrets as null', () => {
  const fl = form();
  const values = initialValues(fl, false);
  const lists = initialLists(fl);
  lists.steps = moveRow(lists.steps, 0, 1);
  values.password = '';
  values.extra = 'kept';
  const out = formValues(fl, values, lists);
  assert.equal(out.values.password, null);
  assert.equal(out.values.extra, 'kept');
  assert.deepEqual(out.lists.steps.map((r) => r.step_id), [2, 1]);
  assert.equal(out.lists.steps[0].on_success_step, 'Backup');
  values.password = 's3cret';
  assert.equal(formValues(fl, values, lists).values.password, 's3cret');
});

test('the snapshot changes with the values, not with key order', () => {
  const fl = form();
  const a = snapshot(fl, { x: 1, y: 2 }, { steps: [{ a: 1, b: 2 }] });
  const b = snapshot(fl, { y: 2, x: 1 }, { steps: [{ b: 2, a: 1 }] });
  assert.equal(a, b);
  assert.notEqual(a, snapshot(fl, { x: 1, y: 3 }, { steps: [{ a: 1, b: 2 }] }));
});

test('the list\'s table shows up to three short columns besides the title', () => {
  assert.deepEqual(summaryFields(steps).map((x) => x.key), ['subsystem', 'database', 'on_success']);
});

test('rows move and go within the list\'s rules', () => {
  assert.deepEqual(moveRow([1, 2, 3], 0, -1), [1, 2, 3]);
  assert.deepEqual(moveRow([1, 2, 3], 2, 1), [1, 2, 3]);
  assert.deepEqual(moveRow([1, 2, 3], 1, 1), [1, 3, 2]);
  assert.equal(canRemoveRow(steps, 1, false), false);
  assert.equal(canRemoveRow(steps, 2, false), true);
  assert.equal(canRemoveRow(steps, 2, true), false);
  assert.equal(canRemoveRow({ ...steps, read_only: true }, 5, false), false);
});

test('code fields map to the editor\'s modes', () => {
  assert.deepEqual(codeMode('tsql', 'postgres'), { language: 'sql', dialect: 'mssql' });
  assert.deepEqual(codeMode('PLSQL', ''), { language: 'sql', dialect: 'oracle' });
  assert.deepEqual(codeMode('sql', 'mysql'), { language: 'sql', dialect: 'mysql' });
  assert.deepEqual(codeMode('json', 'mssql'), { language: 'json', dialect: '' });
  assert.notEqual(codeMode('powershell', 'mssql').language, 'sql');
});

test('a date-time keeps its offset; durations read in units', () => {
  assert.deepEqual(splitOffset('2026-11-12T03:00:00Z'), ['2026-11-12T03:00:00', 'Z']);
  assert.deepEqual(splitOffset('2026-11-12T03:00:00-03:00'), ['2026-11-12T03:00:00', '-03:00']);
  assert.deepEqual(splitOffset('2026-11-12T03:00:00'), ['2026-11-12T03:00:00', '']);
  assert.deepEqual(splitOffset('2026-11-12'), ['2026-11-12', '']);
  assert.equal(humanDuration(5400), '1 h 30 min');
  assert.equal(humanDuration(45), '45 s');
  assert.equal(humanDuration(0), '0 s');
  assert.equal(humanDuration(90061), '1 d 1 h 1 min 1 s');
});

// -- second pass: combined conditions, conditional lists and tabs, dependent choices, abilities --

test('when_all: every condition holds besides when, and a hidden owner hides', () => {
  const fields = [
    f('freq', 'select', { kind: { options: [['once', 'Una vez'], ['daily', 'Diaria']] } }),
    f('sub', 'select', { kind: { options: [['once', 'Una vez'], ['every', 'Cada']] }, when: ['freq', ['daily']] }),
    f('until', 'time', { when: ['sub', ['every']], when_all: [['freq', ['daily']]] }),
    // when_all alone (no `when`) is a condition too.
    f('only_all', 'text', { when_all: [['freq', ['once']], ['sub', ['once', 'every']]] }),
  ];
  const until = fields[2];
  const ctx = (values) => ({ fields, values });
  assert.equal(isVisible(until, ctx({ freq: 'daily', sub: 'every' })), true);
  assert.equal(isVisible(until, ctx({ freq: 'daily', sub: 'once' })), false);
  // `sub` still says every, but freq once both hides `sub` and fails when_all.
  assert.equal(isVisible(until, ctx({ freq: 'once', sub: 'every' })), false);
  // A when_all naming a hidden field fails even when its value matches.
  assert.equal(isVisible(fields[3], ctx({ freq: 'once', sub: 'every' })), false);
  const free = [f('a', 'text'), f('b', 'text'), f('c', 'text', { when_all: [['a', ['1']], ['b', ['2']]] })];
  assert.equal(isVisible(free[2], { fields: free, values: { a: '1', b: '2' } }), true);
  assert.equal(isVisible(free[2], { fields: free, values: { a: '1', b: '3' } }), false);
  // In a row: one condition on the row, another on the form's top level.
  const top = { fields: [f('type', 'select')], values: { type: 'CHAIN' } };
  const rowField = f('x', 'text', { when: ['kind', ['step']], when_all: [['type', ['CHAIN']]] });
  const row = (kind, type) => ({ fields: [f('kind', 'text'), rowField], values: { kind }, parent: { ...top, values: { type } } });
  assert.equal(isVisible(rowField, row('step', 'CHAIN')), true);
  assert.equal(isVisible(rowField, row('step', 'PLSQL_BLOCK')), false);
  assert.equal(isVisible(rowField, row('rule', 'CHAIN')), false);
});

// An Oracle-like job: its type picks a tab (arguments) and a list (remote destinations).
const typed = () => ({
  sections: [
    {
      key: 'general', label: 'General',
      fields: [
        f('name', 'text', { required: true }),
        f('job_type', 'select', { kind: { options: [['PLSQL_BLOCK', 'Bloque'], ['STORED_PROCEDURE', 'Procedimiento'], ['EXECUTABLE', 'Ejecutable']] } }),
        f('advanced', 'bool'),
        f('dest_type', 'select', { when: ['advanced', ['true']] }),
      ],
    },
    {
      key: 'args', label: 'Argumentos', when: ['job_type', ['STORED_PROCEDURE', 'EXECUTABLE']],
      fields: [f('arg_count', 'number', { required: true })],
      lists: [{ key: 'arguments', label: 'Argumentos', title_field: 'value', min_rows: 1, fields: [f('value', 'text', { required: true })] }],
    },
    {
      key: 'options', label: 'Opciones',
      lists: [
        { key: 'destinations', label: 'Destinos', title_field: 'host', min_rows: 1, when: ['job_type', ['EXECUTABLE']], fields: [f('host', 'text', { required: true }), f('pwd', 'secret')] },
        { key: 'remote', label: 'Remotos', title_field: 'host', when: ['dest_type', ['remote']], fields: [f('host', 'text')] },
      ],
    },
  ],
});

test('a list or a tab shows while the top-level field it names has one of the values', () => {
  const fl = typed();
  const top = (values) => topContext(fl, values);
  const [general, args, options] = fl.sections;
  assert.equal(sectionVisible(general, top({ job_type: 'PLSQL_BLOCK' })), true);
  assert.equal(sectionVisible(args, top({ job_type: 'PLSQL_BLOCK' })), false);
  assert.equal(sectionVisible(args, top({ job_type: 'EXECUTABLE' })), true);
  assert.equal(listVisible(options.lists[0], top({ job_type: 'STORED_PROCEDURE' })), false);
  assert.equal(listVisible(options.lists[0], top({ job_type: 'EXECUTABLE' })), true);
  // A list hanging from a hidden field is hidden too.
  assert.equal(listVisible(options.lists[1], top({ advanced: false, dest_type: 'remote' })), false);
  assert.equal(listVisible(options.lists[1], top({ advanced: true, dest_type: 'remote' })), true);
  // A list in a hidden tab doesn't show either.
  assert.deepEqual(visibleLists(fl, top({ job_type: 'PLSQL_BLOCK' })).map((l) => l.key), []);
  assert.deepEqual(visibleLists(fl, top({ job_type: 'EXECUTABLE' })).map((l) => l.key), ['arguments', 'destinations']);
});

test('a field on a tab that hid holds no condition elsewhere', () => {
  // `dest_type` lives on the "Avanzado" tab, shown only for executables; the
  // "Remotos" list and the "Puerto" field hang from it.
  const fl = {
    sections: [
      {
        key: 'general', label: 'General',
        fields: [
          f('job_type', 'select', { kind: { options: [['PLSQL_BLOCK', 'PL/SQL'], ['EXECUTABLE', 'Ejecutable']] } }),
          f('port', 'number', { when: ['dest_type', ['remote']], required: true }),
        ],
      },
      { key: 'advanced', label: 'Avanzado', when: ['job_type', ['EXECUTABLE']], fields: [f('dest_type', 'select', { kind: { options: [['local', 'Local'], ['remote', 'Remoto']] } })] },
      {
        key: 'targets', label: 'Destinos',
        lists: [{ key: 'remote', label: 'Remotos', title_field: 'host', min_rows: 1, when: ['dest_type', ['remote']], fields: [f('host', 'text')] }],
      },
      // A tab whose own field picks it (a cycle) doesn't hide over it.
      { key: 'self', label: 'Propio', when: ['mode', ['on']], fields: [f('mode', 'select', { kind: { options: [['on', 'Sí']] } })] },
    ],
  };
  const [general, advanced, targets, self] = fl.sections;
  const list = targets.lists[0];
  const port = general.fields[1];
  // An executable: the tab shows, so its value counts.
  let top = topContext(fl, { job_type: 'EXECUTABLE', dest_type: 'remote' });
  assert.equal(sectionVisible(advanced, top), true);
  assert.equal(listVisible(list, top), true);
  assert.equal(isVisible(port, top), true);
  assert.deepEqual(visibleLists(fl, top).map((l) => l.key), ['remote']);
  // Back to PL/SQL: the tab hides with "remote" still on it; nothing hangs from it.
  top = topContext(fl, { job_type: 'PLSQL_BLOCK', dest_type: 'remote' });
  assert.equal(sectionVisible(advanced, top), false);
  assert.equal(listVisible(list, top), false);
  assert.equal(isVisible(port, top), false);
  assert.deepEqual(visibleLists(fl, top).map((l) => l.key), []);
  // Neither checked nor sent.
  const p = validate(fl, { job_type: 'PLSQL_BLOCK', dest_type: 'remote' }, { remote: [] }, true);
  assert.equal(p.count, 0, JSON.stringify(p));
  assert.deepEqual(Object.keys(formValues(fl, { job_type: 'PLSQL_BLOCK', dest_type: 'remote' }, { remote: [] }).lists), []);
  assert.deepEqual(validate(fl, { job_type: 'EXECUTABLE', dest_type: 'remote' }, { remote: [] }, true).lists, { remote: { code: 'minRows', n: 1 } });
  // A row field hanging from it hides too.
  const host = f('host', 'text', { when: ['dest_type', ['remote']] });
  const row = (values) => ({ fields: [host], values: {}, parent: topContext(fl, values) });
  assert.equal(isVisible(host, row({ job_type: 'EXECUTABLE', dest_type: 'remote' })), true);
  assert.equal(isVisible(host, row({ job_type: 'PLSQL_BLOCK', dest_type: 'remote' })), false);
  // The cycle: the tab shows as its own field's value says.
  assert.equal(sectionVisible(self, topContext(fl, { mode: 'on' })), true);
  assert.equal(sectionVisible(self, topContext(fl, { mode: 'off' })), false);
});

test('what doesn\'t show isn\'t checked nor sent: hidden lists leave their key out', () => {
  const fl = typed();
  const lists = { arguments: [], destinations: [], remote: [{ host: 'x' }] };
  // A PL/SQL block: no arguments tab, no destinations, nothing missing.
  let p = validate(fl, { name: 'j', job_type: 'PLSQL_BLOCK' }, lists, true);
  assert.equal(p.count, 0, JSON.stringify(p));
  let out = formValues(fl, { name: 'j', job_type: 'PLSQL_BLOCK' }, lists);
  assert.deepEqual(Object.keys(out.lists), []);
  // An executable: its tab's field and both lists are checked and sent.
  p = validate(fl, { name: 'j', job_type: 'EXECUTABLE' }, lists, true);
  assert.deepEqual(p.fields.arg_count, { code: 'required' });
  assert.deepEqual(p.lists, { arguments: { code: 'minRows', n: 1 }, destinations: { code: 'minRows', n: 1 } });
  assert.deepEqual(p.sections, ['args', 'options']);
  out = formValues(fl, { name: 'j', job_type: 'EXECUTABLE' }, { ...lists, destinations: [{ host: 'h1' }] });
  assert.deepEqual(Object.keys(out.lists).sort(), ['arguments', 'destinations']);
  assert.deepEqual(out.lists.destinations, [{ host: 'h1' }]);
  // The snapshot follows: the type changed, so did what's sent.
  assert.notEqual(snapshot(fl, { job_type: 'PLSQL_BLOCK' }, lists), snapshot(fl, { job_type: 'EXECUTABLE' }, lists));
  // A typed secret in a list that isn't sent doesn't make the script masked.
  const withPwd = { ...lists, destinations: [{ host: 'h1', pwd: 's3cret' }] };
  assert.equal(hasTypedSecret(fl, { job_type: 'PLSQL_BLOCK' }, withPwd), false);
  assert.equal(hasTypedSecret(fl, { job_type: 'EXECUTABLE' }, withPwd), true);
});

// A SQL Server performance-condition alert: counters by object, instances by counter.
const perf = () => ({
  sections: [{
    key: 'general', label: 'General',
    fields: [
      f('object', 'ref', { kind: { source: 'perf_object' }, required: true }),
      f('counter', 'ref', { kind: { source: 'perf_counter:{object}' }, required: true }),
      f('instance', 'ref', { kind: { source: 'perf_instance:{counter}' } }),
    ],
    lists: [{
      key: 'watch', label: 'Contadores', title_field: 'counter',
      fields: [f('object', 'ref', { kind: { source: 'perf_object' } }), f('counter', 'ref', { kind: { source: 'perf_counter:{object}' } }), f('note', 'text')],
    }],
  }],
  choices: {
    perf_object: [['SQLServer:Databases', 'Databases'], ['SQLServer:Locks', 'Locks'], ['SQLServer:User Settable', 'User Settable']],
    'perf_counter:SQLServer:Databases': [['Percent Log Used', 'Percent Log Used'], ['Log Growths', 'Log Growths']],
    'perf_counter:SQLServer:Locks': [['Lock Waits/sec', 'Lock Waits/sec']],
    'perf_instance:Percent Log Used': [['ventas', 'ventas'], ['crm', 'crm'], ['_Total', '_Total']],
    'perf_counter:': [],
  },
});

test('a source with {field}s is filled from the row first, then the form', () => {
  assert.deepEqual(sourceFields('perf_instance:{object}|{counter}'), ['object', 'counter']);
  assert.deepEqual(sourceFields('database'), []);
  const fl = perf();
  const top = topContext(fl, { object: 'SQLServer:Databases', counter: 'Percent Log Used' });
  assert.equal(fillSource('perf_counter:{object}', top), 'perf_counter:SQLServer:Databases');
  assert.equal(fillSource('x:{object}|{counter}', top), 'x:SQLServer:Databases|Percent Log Used');
  assert.equal(fillSource('x:{nope}', top), 'x:');
  const list = fl.sections[0].lists[0];
  // The row's own `object`, even before it has a value; else the form's.
  const row = { fields: list.fields, values: { object: 'SQLServer:Locks' }, parent: top };
  assert.equal(fillSource('perf_counter:{object}', row), 'perf_counter:SQLServer:Locks');
  assert.equal(fillSource('perf_counter:{object}', { fields: list.fields, values: {}, parent: top }), 'perf_counter:');
  assert.equal(fillSource('perf_instance:{instance}', { fields: list.fields, values: {}, parent: { ...top, values: { instance: 'crm' } } }), 'perf_instance:crm');
});

test('dependent choices: what the filled source lists, or free text when nothing is', () => {
  const fl = perf();
  const lists = { watch: [] };
  const at = (values) => topContext(fl, values);
  assert.deepEqual(refOptions('perf_counter:{object}', fl, lists, at({ object: 'SQLServer:Locks' })), [['Lock Waits/sec', 'Lock Waits/sec']]);
  assert.equal(refOptions('perf_instance:{counter}', fl, lists, at({ counter: 'Percent Log Used' })).length, 3);
  // Nothing listed (an object the driver has no counters for, nothing chosen yet): free text.
  assert.equal(refOptions('perf_counter:{object}', fl, lists, at({ object: 'SQLServer:User Settable' })), null);
  assert.equal(refOptions('perf_counter:{object}', fl, lists, at({})), null);
  assert.equal(refOptions('perf_instance:{counter}', fl, lists, at({ counter: 'Lock Waits/sec' })), null);
  // Sources without fields stay as they were.
  assert.deepEqual(refOptions('perf_object', fl, lists, at({})), fl.choices.perf_object);
  assert.deepEqual(refOptions('nope', fl, lists, at({})), []);
});

test('a dependent reference is checked against its choices only when there are some', () => {
  const fl = perf();
  const ok = { object: 'SQLServer:Databases', counter: 'Percent Log Used', instance: 'ventas' };
  assert.equal(validate(fl, ok, { watch: [] }, true).count, 0);
  // Another object: the counter chosen for the first one isn't among its counters.
  let p = validate(fl, { ...ok, object: 'SQLServer:Locks' }, { watch: [] }, true);
  assert.deepEqual(p.fields.counter, { code: 'missing', value: 'Percent Log Used' });
  // The instance hangs from the counter: Lock Waits/sec lists none, so any text goes.
  p = validate(fl, { object: 'SQLServer:Locks', counter: 'Lock Waits/sec', instance: 'whatever' }, { watch: [] }, true);
  assert.equal(p.count, 0, JSON.stringify(p));
  // An object with nothing listed: free text, not checked.
  assert.equal(validate(fl, { object: 'SQLServer:User Settable', counter: 'Query 1' }, { watch: [] }, true).count, 0);
  // Inside a row, the row's object decides.
  const rows = [{ object: 'SQLServer:Locks', counter: 'Lock Waits/sec' }, { object: 'SQLServer:Locks', counter: 'Log Growths' }];
  p = validate(fl, ok, { watch: rows }, true);
  assert.deepEqual(p.rows.watch, [{ row: 1, field: 'counter', problem: { code: 'missing', value: 'Log Growths' } }]);
  // The same check the row editor runs.
  const list = fl.sections[0].lists[0];
  assert.deepEqual(rowProblems(list, rows[1], topContext(fl, ok), true, refsToCheck(fl, { watch: rows })),
    [{ field: 'counter', problem: { code: 'missing', value: 'Log Growths' } }]);
  // Plain sources still aren't checked, `list:` ones still are.
  const check = refsToCheck(fl, { watch: rows });
  assert.equal(check(fl.sections[0].fields[0], topContext(fl, ok)), undefined);
  assert.equal(check(f('t', 'text'), topContext(fl, ok)), undefined);
  assert.deepEqual(check(f('s', 'ref', { kind: { source: 'list:watch' } }), topContext(fl, ok)), [['Lock Waits/sec', '1 · Lock Waits/sec'], ['Log Growths', '2 · Log Growths']]);
});

test('an object offers what it says, else its kind, system ones included', () => {
  const kind = {
    key: 'window', label: 'Ventanas', singular: 'ventana', can_create: true, can_edit: true, can_delete: true,
    actions: [{ key: 'open_window', label: 'Abrir ventana' }, { key: 'close_window', label: 'Cerrar ventana' }],
  };
  assert.deepEqual(itemAbilities(kind, {}), { edit: true, delete: true, actions: kind.actions });
  assert.deepEqual(itemAbilities(kind, { can_delete: false }).delete, false);
  assert.deepEqual(itemAbilities(kind, { can_edit: false }).edit, false);
  // System: as the kind says unless the object says otherwise (the contract:
  // absent is the kind's; a driver that protects one sends false).
  assert.deepEqual(itemAbilities(kind, { system: true }), { edit: true, delete: true, actions: kind.actions });
  assert.equal(itemAbilities(kind, { system: true, can_delete: null }).delete, true);
  assert.equal(itemAbilities(kind, { system: true, can_delete: false }).delete, false);
  assert.equal(itemAbilities({ ...kind, can_delete: false }, { system: true, can_delete: true }).delete, true);
  // The actions it lists, in the kind's order; none listed: none.
  assert.deepEqual(itemAbilities(kind, { actions: ['close_window'] }).actions.map((a) => a.key), ['close_window']);
  assert.deepEqual(itemAbilities(kind, { actions: ['close_window', 'open_window', 'gone'] }).actions.map((a) => a.key), ['open_window', 'close_window']);
  assert.deepEqual(itemAbilities(kind, { actions: [] }).actions, []);
  // A kind that can't, or no kind at all (its info failed): nothing.
  assert.deepEqual(itemAbilities({ ...kind, can_edit: false, can_delete: false, actions: undefined }, {}), { edit: false, delete: false, actions: [] });
  assert.deepEqual(itemAbilities(undefined, { system: false }), { edit: false, delete: false, actions: [] });
});

test('script warnings are the ⚠ lines of the leading comment block', () => {
  const script = [
    '-- Edita el job «x».',
    '-- ⚠ Doris no tiene ALTER para este job: se borra y se crea de nuevo.',
    '-- Un comentario común.',
    '',
    '# ⚠ Se pierde el historial de ejecuciones.',
    '/* ⚠ Las propiedades que no ves acá no se envían. */',
    '/*',
    ' * ⚠ Dentro de un bloque.',
    ' */',
    '--⚠',
    "DROP JOB x;",
    '-- ⚠ Después de una sentencia: es del cuerpo, no un aviso.',
    "CREATE JOB x DO INSERT INTO t SELECT '⚠ no es un aviso';",
  ].join('\n');
  assert.deepEqual(scriptWarnings(script), [
    'Doris no tiene ALTER para este job: se borra y se crea de nuevo.',
    'Se pierde el historial de ejecuciones.',
    'Las propiedades que no ves acá no se envían.',
    'Dentro de un bloque.',
  ]);
  assert.deepEqual(scriptWarnings('BEGIN\n  NULL;\nEND;'), []);
  // A body line the user wrote can't pose as one.
  assert.deepEqual(scriptWarnings("-- Paso 1\nEXEC sp_add_jobstep @command = N'SELECT 1\n-- ⚠ falso aviso\nSELECT 2';"), []);
  // A bare "⚠" line (no comment marker) isn't one either.
  assert.deepEqual(scriptWarnings('⚠ no es comentario\nSELECT 1;'), []);
});

test('an action that asks for values first shows "…" after its label, once', () => {
  assert.equal(actionText('Ejecutar ahora'), 'Ejecutar ahora');
  assert.equal(actionText('Ejecutar ahora', false), 'Ejecutar ahora');
  assert.equal(actionText('Rellenar un período', true), 'Rellenar un período…');
  // The driver's label may already have one (either form): not twice.
  assert.equal(actionText('Recalcular…', true), 'Recalcular…');
  assert.equal(actionText('Recalcular...', true), 'Recalcular...');
  assert.equal(actionText('Recalcular ', true), 'Recalcular…');
  assert.equal(actionText('', true), '');
});

test("an action's form starts from its values and its fields' defaults, checked and sent like the designer's", () => {
  // A BigQuery-like backfill: a range or a single run time.
  const form = {
    sections: [{
      key: 'range', label: 'Período',
      fields: [
        f('run_type', 'select', { kind: { options: [['range', 'Período'], ['time', 'Una']] }, required: true, default: 'range' }),
        f('start_time', 'date_time', { required: true, when: ['run_type', ['range']], default: '2026-11-06T03:00:00Z' }),
        f('end_time', 'date_time', { required: true, when: ['run_type', ['range']] }),
        f('run_time', 'date_time', { required: true, when: ['run_type', ['time']] }),
      ],
    }],
    values: { end_time: '2026-11-13T03:00:00Z' },
  };
  const values = initialValues(form, true);
  assert.deepEqual(values, { run_type: 'range', start_time: '2026-11-06T03:00:00Z', end_time: '2026-11-13T03:00:00Z' });
  // The single run time hides: it isn't required while the range shows.
  assert.equal(validate(form, values, initialLists(form), true).count, 0);
  const one = { ...values, run_type: 'time' };
  assert.deepEqual(Object.keys(validate(form, one, {}, true).fields), ['run_time']);
  assert.deepEqual(formValues(form, values, initialLists(form)), { values, lists: {} });
});
