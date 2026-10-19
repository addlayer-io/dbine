import i18next from 'i18next';
import { ref } from 'vue';
import { loadBackendCatalog } from './backend';

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
// src-tauri/src/commands/updates.rs (the changelog translations): a new
// language goes in all of them.
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
] as const;
export type Lang = (typeof LANGUAGES)[number]['code'];
const info = (code: Lang) => LANGUAGES.find((l) => l.code === code)!;

const STORAGE_KEY = 'dbine.language';
/** Synced preference (Configuración › General › Idioma). */
export const SETTING_KEY = 'ui.language';

const files = import.meta.glob<{ default: Record<string, unknown> }>('../locales/*/*.json', { eager: true });
const resources: Record<string, Record<string, Record<string, unknown>>> = {};
for (const [path, mod] of Object.entries(files)) {
  const m = /locales\/([a-z]+)\/([\w-]+)\.json$/.exec(path);
  if (!m || m[2] === 'backend') continue;
  (resources[m[1]] ??= {})[m[2]] = mod.default;
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

i18next.init({
  lng: language.value,
  fallbackLng: 'es',
  resources,
  ns: Object.keys(resources.es ?? {}),
  defaultNS: 'common',
  interpolation: { escapeValue: false },
  returnNull: false,
});
document.documentElement.lang = info(language.value).tag;

/** Switch the whole app's language (texts update in place). */
export async function setLanguage(code: Lang) {
  if (code === language.value) return;
  language.value = code;
  try { localStorage.setItem(STORAGE_KEY, code); } catch { /* storage blocked */ }
  document.documentElement.lang = info(code).tag;
  await loadBackendCatalog(code);
  await i18next.changeLanguage(code);
}

/** Translate outside components (stores, composables). Inside templates use `$t`. */
export const t = i18next.t.bind(i18next);

/** Number and date formatting in the current language. */
export const locale = () => info(language.value).intl;

export default i18next;
