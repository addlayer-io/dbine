import i18next from 'i18next';
import { ref } from 'vue';
import { loadBackendCatalog } from './backend';
import { loadElementLocale } from './element';

// The app's languages (i18next). Spanish is the source: every text is written
// in Spanish first (web/src/locales/es/*.json) and translated to the others.
// One JSON file per area and language (`locales/<lang>/<area>.json`); a key
// is `<area>:<key>` (`common:save`, `explorer:connect`). Missing keys fall
// back to Spanish, so a new text shows up (in Spanish) before it's translated;
// `npm run i18n:check` lists what's missing.
//
// Messages that come from the backend (Rust) are Spanish sentences, not keys:
// `tb()` in ./backend.ts translates them.

//
// The language list is also in scripts/i18n/languages.json (the checks,
// the changelog), in crates/dbine-ai (the AI prompts) and in
// src-tauri/src/commands/updates.rs (the changelog translations), in
// web/src/i18n/element.ts and in mobile/src/i18n.ts (the phone app): a new
// language goes in all of them (scripts/i18n/check.py compares them).
//
// One variant per language: `zh` is Simplified Chinese (zh-CN) and also what
// zh-TW / zh-HK systems get; `pt` is Brazilian Portuguese.
// `tag` is the BCP 47 tag (`<html lang>`, which picks the CJK glyph variant
// in global.scss) and `intl` the locale for number and date formatting.
export const LANGUAGES = [
  { code: 'en', name: 'English', tag: 'en', intl: 'en-US' },
  { code: 'es', name: 'Español', tag: 'es', intl: 'es-AR' },
  { code: 'pt', name: 'Português', tag: 'pt-BR', intl: 'pt-BR' },
  { code: 'fr', name: 'Français', tag: 'fr', intl: 'fr-FR' },
  { code: 'it', name: 'Italiano', tag: 'it', intl: 'it-IT' },
  { code: 'de', name: 'Deutsch', tag: 'de', intl: 'de-DE' },
  { code: 'ru', name: 'Русский', tag: 'ru', intl: 'ru-RU' },
  { code: 'zh', name: '中文（简体）', tag: 'zh-CN', intl: 'zh-CN' },
  { code: 'ja', name: '日本語', tag: 'ja', intl: 'ja-JP' },
  { code: 'ko', name: '한국어', tag: 'ko', intl: 'ko-KR' },
  { code: 'uk', name: 'Українська', tag: 'uk', intl: 'uk-UA' },
  { code: 'pl', name: 'Polski', tag: 'pl', intl: 'pl-PL' },
  { code: 'id', name: 'Bahasa Indonesia', tag: 'id', intl: 'id-ID' },
] as const;
export type Lang = (typeof LANGUAGES)[number]['code'];
const info = (code: Lang) => LANGUAGES.find((l) => l.code === code)!;

const STORAGE_KEY = 'dbine.language';
/** Synced preference (Configuración › General › Idioma). */
export const SETTING_KEY = 'ui.language';

// Spanish is the fallback (and the source of the key list), so it's in the
// main chunk. Every other language is a chunk of its own (vite.config.ts),
// loaded at start only when it's the startup language and otherwise on the
// first switch to it: startup doesn't pay for languages it doesn't show.
type Bundle = Record<string, unknown>;
const spanish = import.meta.glob<{ default: Bundle }>(
  ['../locales/es/*.json', '!../locales/es/backend.json'],
  { eager: true },
);
const others = import.meta.glob<{ default: Bundle }>(
  ['../locales/*/*.json', '!../locales/es/*.json', '!../locales/*/backend.json'],
);
const fileRe = /locales\/([a-z]+)\/([\w-]+)\.json$/;
const resources: Record<string, Record<string, Bundle>> = { es: {} };
for (const [path, mod] of Object.entries(spanish)) {
  const m = fileRe.exec(path);
  if (m) resources.es[m[2]] = mod.default;
}

export function isLang(v: unknown): v is Lang {
  return typeof v === 'string' && LANGUAGES.some((l) => l.code === v);
}

/** Ours for a BCP 47 tag (`pt-BR`, `zh-Hant-TW`, `de_AT`): its primary
 *  subtag, so every Chinese variant gets `zh`. */
export function langOfTag(tag: string | null | undefined): Lang | null {
  const code = tag?.split(/[-_]/)[0]?.toLowerCase();
  return isLang(code) ? code : null;
}

/** The system's language when it's one of ours; English otherwise. */
export function systemLanguage(): Lang {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const code = langOfTag(tag);
    if (code) return code;
  }
  return 'en';
}

function initialLanguage(): Lang {
  try {
    const saved = localStorage.getItem(STORAGE_KEY);
    if (isLang(saved)) return saved;
  } catch { /* storage blocked */ }
  return systemLanguage();
}

/** The current language, reactive (for computed labels and Element Plus). */
export const language = ref<Lang>(initialLanguage());

const loadedUi = new Set<string>(['es']);

/** A language's UI namespaces, into i18next (once). */
async function loadUi(code: Lang) {
  if (loadedUi.has(code)) return;
  const prefix = `../locales/${code}/`;
  const bundles = await Promise.all(
    Object.entries(others)
      .filter(([path]) => path.startsWith(prefix))
      .map(async ([path, load]) => [fileRe.exec(path)![2], (await load()).default] as const),
  );
  for (const [ns, bundle] of bundles) i18next.addResourceBundle(code, ns, bundle, true, true);
  loadedUi.add(code);
}

/** Everything a language shows: UI texts, Element Plus's, backend messages. */
function loadLanguage(code: Lang) {
  return Promise.all([loadUi(code), loadElementLocale(code), loadBackendCatalog(code)]);
}

i18next.init({
  lng: language.value,
  fallbackLng: 'es',
  resources,
  ns: Object.keys(resources.es),
  defaultNS: 'common',
  interpolation: { escapeValue: false },
  returnNull: false,
});
document.documentElement.lang = info(language.value).tag;

/** Resolves once the startup language is loaded: mount the app after it, so
 *  no raw key is ever rendered. If loading fails, the texts fall back to
 *  Spanish. */
export const i18nReady: Promise<void> = loadLanguage(language.value).then(
  () => {},
  (e) => console.error('i18n: could not load', language.value, e),
);

let requested: Lang = language.value;

/** Switch the whole app's language (texts update in place). The language's
 *  texts load first, so nothing is shown untranslated in between. */
export async function setLanguage(code: Lang) {
  requested = code;
  if (code === language.value) return;
  try {
    await loadLanguage(code);
  } catch (e) {
    console.error('i18n: could not load', code, e);
    return;
  }
  if (requested !== code) return; // another switch came while this one loaded
  try { localStorage.setItem(STORAGE_KEY, code); } catch { /* storage blocked */ }
  await i18next.changeLanguage(code);
  language.value = code;
  document.documentElement.lang = info(code).tag;
}

/** Translate outside components (stores, composables). Inside templates use `$t`. */
export const t = i18next.t.bind(i18next);

/** Number and date formatting in the current language. */
export const locale = () => info(language.value).intl;

/** "a, b y c" in the current language (its own separators and conjunction). */
export const listOf = (items: string[]) => new Intl.ListFormat(locale(), { type: 'conjunction' }).format(items);

export default i18next;
