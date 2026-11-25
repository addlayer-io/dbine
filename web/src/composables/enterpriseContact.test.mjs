// Tests for enterpriseContact.ts. Run: `node --test src/composables/enterpriseContact.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { ENTERPRISE_CONTACT, contactUrl, opensOnSignIn } from './enterpriseContact.ts';

test('the contact is an e-mail to the public address, subject encoded', () => {
  assert.equal(contactUrl('DBine Enterprise'), `mailto:${ENTERPRISE_CONTACT}?subject=DBine%20Enterprise`);
  assert.equal(contactUrl('Équipe & co'), `mailto:${ENTERPRISE_CONTACT}?subject=%C3%89quipe%20%26%20co`);
  // What the backend opens: http(s) and mailto only (safeHref).
  assert.match(contactUrl('x'), /^mailto:/);
});

test('existing customers land on the sign-in form, everyone else on the pitch', () => {
  assert.equal(opensOnSignIn('dbine.empresa.com'), true);
  assert.equal(opensOnSignIn(''), false);
  assert.equal(opensOnSignIn('   '), false);
  assert.equal(opensOnSignIn(null), false);
  assert.equal(opensOnSignIn(undefined), false);
});
