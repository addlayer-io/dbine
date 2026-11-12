// Tests for the scheduler designer's form logic. Run:
// `node --test src/composables/schedulerForm.test.mjs` from web/ (Node 23.6+
// strips the TypeScript types itself). Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  canRemoveRow, codeMode, fieldOptions, fieldProblem, formValues, hasTypedSecret, humanDuration, initialLists,
  initialValues, isVisible, moveRow, newRow, optionLabel, problemText, refChoices, renameRefs, snapshot, splitOffset,
  summaryFields, validate, whenText,
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
