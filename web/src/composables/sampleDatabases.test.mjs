// Tests for sampleDatabases.ts. Run: `node --test src/composables/sampleDatabases.test.mjs` from web/.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync } from 'node:fs';
import { join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { SAMPLE_CONNECTION_IDS, SAMPLES_FOLDER_ID, SAMPLES_INSTALLED, isSampleConnection, sampleTexts } from './sampleDatabases.ts';

const here = fileURLToPath(new URL('.', import.meta.url));
const locales = join(here, '../locales');

/** A `t` over one language's samples.json, failing on a missing key. */
function tOf(lang) {
  const bundle = JSON.parse(readFileSync(join(locales, lang, 'samples.json'), 'utf8'));
  return (key, values = {}) => {
    const path = key.replace(/^samples:/, '').split('.');
    let v = bundle;
    for (const p of path) v = v?.[p];
    assert.equal(typeof v, 'string', `${lang}: ${key}`);
    return v.replace(/\{\{(\w+)\}\}/g, (_, k) => {
      assert.ok(k in values, `${lang}: ${key} uses {{${k}}}`);
      return values[k];
    });
  };
}

test('every language has every text, with the connections named as they are', () => {
  const langs = readdirSync(locales, { withFileTypes: true }).filter((d) => d.isDirectory()).map((d) => d.name);
  assert.equal(langs.length, 13);
  for (const lang of langs) {
    const texts = sampleTexts(tOf(lang));
    for (const v of [texts.folder, texts.tienda, texts.analitica, texts.archivos, texts.tour_file, ...Object.values(texts.tour)]) {
      assert.ok(v.trim(), lang);
      assert.ok(!v.includes('{{'), `${lang}: ${v}`);
    }
    // The tour names the connections exactly as the explorer shows them.
    assert.ok(texts.tour.intro.includes(texts.tienda) && texts.tour.intro.includes(texts.analitica), lang);
    assert.ok(texts.tour.only_in_analitica.includes(texts.analitica), lang);
    assert.ok(texts.tour.targets.includes(texts.archivos), lang);
  }
});

test('the ids are the backend\'s', () => {
  const rs = readFileSync(join(here, '../../../src-tauri/src/samples.rs'), 'utf8');
  const constant = (name) => rs.match(new RegExp(`pub const ${name}: &str = "([^"]+)"`))?.[1];
  assert.equal(constant('FOLDER_ID'), SAMPLES_FOLDER_ID);
  assert.equal(constant('INSTALLED'), SAMPLES_INSTALLED);
  assert.deepEqual([constant('TIENDA_ID'), constant('ANALITICA_ID'), constant('ARCHIVOS_ID')], [...SAMPLE_CONNECTION_IDS]);
});

test('the sample connections are told apart, by their fixed ids only', () => {
  for (const id of SAMPLE_CONNECTION_IDS) assert.ok(isSampleConnection(id), id);
  assert.ok(!isSampleConnection('dbine-samples'), 'the folder is not a connection');
  assert.ok(!isSampleConnection('my-tienda'));
});
