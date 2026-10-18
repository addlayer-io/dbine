// Tests for "Conectar Zoho Vault"'s steps. Run: `node --test
// src/composables/zohoWizard.test.mjs` from web/. Not part of the app's build.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { back, canAdvance, emptyWizard, next, stepForError, stepProblems, suggestedLabel, validClientId, ZOHO_SCOPES } from './zohoWizard.ts';

test('each step is checked before going on', () => {
  const s = emptyWizard();
  assert.deepEqual(stepProblems(s), ['client_id', 'client_secret']);
  assert.equal(next(s), false);
  s.client_id = '1000.ABC DEF';
  assert.deepEqual(stepProblems(s), ['client_id_format', 'client_secret']);
  s.client_id = ' 1000.ABCDEF ';
  s.client_secret = 'cs';
  assert.ok(next(s));
  assert.equal(s.step, 2);
  assert.deepEqual(stepProblems(s), ['code']);
  s.code = '1000.a b';
  assert.deepEqual(stepProblems(s), ['code_format']);
  s.code = '1000.abc.def';
  assert.ok(next(s));
  // Step 3: not until the code was exchanged, and with a label.
  assert.deepEqual(stepProblems(s), ['connect', 'label']);
  s.pending = 'p-1';
  s.data_center = 'eu';
  s.label = suggestedLabel('ana@example.com', '');
  assert.equal(s.label, 'Zoho Vault · ana@example.com');
  assert.ok(next(s));
  // Step 4: the passphrase (kept or asked), or the command.
  assert.deepEqual(stepProblems(s), ['passphrase']);
  s.master = 'command';
  assert.deepEqual(stepProblems(s), ['master_command']);
  s.master_command = 'pass show zoho';
  assert.ok(canAdvance(s));
  assert.equal(next(s), false, 'the last step finishes instead');
});

test('going back before the exchange drops it (the code is spent)', () => {
  const s = { ...emptyWizard(), step: 4, client_id: '1000.A', client_secret: 'cs', code: '1000.c', pending: 'p-1', data_center: 'eu', label: 'x' };
  assert.deepEqual(back(s), { dropPending: null });
  assert.equal(s.step, 3);
  assert.equal(s.pending, 'p-1', 'step 3 keeps it');
  assert.deepEqual(back(s), { dropPending: 'p-1' });
  assert.equal(s.step, 2);
  assert.equal(s.pending, null);
  assert.equal(s.code, '', 'a new code is needed');
  assert.equal(s.client_secret, 'cs', 'the client stays');
  back(s);
  assert.deepEqual(back(s), { dropPending: null });
  assert.equal(s.step, 1);
});

test('labels, IDs, scopes and where errors send the user', () => {
  assert.equal(suggestedLabel('', ''), '');
  assert.equal(suggestedLabel('ana@example.com', 'Trabajo'), 'Trabajo', "the user's own label stays");
  assert.ok(validClientId('1000.XMDEG1KBDO9SE2IFTMDFSRPYYJ5L0U'));
  assert.ok(!validClientId('1000.x/y'));
  assert.equal(ZOHO_SCOPES, 'ZohoVault.secrets.READ,ZohoVault.user.READ');
  assert.equal(stepForError('el código de Zoho venció o ya se usó: generá un código nuevo en la pestaña «Generate Code»'), 2);
  assert.equal(stepForError('Zoho rechazó el secreto del cliente: copiá de nuevo el «Client Secret» de tu Self Client'), 1);
  assert.equal(stepForError('no se pudo consultar a Zoho en com'), null);
});
