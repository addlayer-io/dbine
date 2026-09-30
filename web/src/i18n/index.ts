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

export const LANGUAGES = [
  { code: 'en', name: 'English' },
  { code: 'es', name: 'Español' },
  { code: 'pt', name: 'Português' },
  { code: 'fr', name: 'Français' },
  { code: 'it', name: 'Italiano' },
] as const;
export type Lang = (typeof LANGUAGES)[number]['code'];

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

/** The system's language when it's one of ours; English otherwise. */
export function systemLanguage(): Lang {
  for (const tag of navigator.languages ?? [navigator.language]) {
    const code = tag?.slice(0, 2).toLowerCase();
    if (isLang(code)) return code;
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
document.documentElement.lang = language.value;

/** Switch the whole app's language (texts update in place). */
export async function setLanguage(code: Lang) {
  if (code === language.value) return;
  language.value = code;
  try { localStorage.setItem(STORAGE_KEY, code); } catch { /* storage blocked */ }
  document.documentElement.lang = code;
  await loadBackendCatalog(code);
  await i18next.changeLanguage(code);
}

/** Translate outside components (stores, composables). Inside templates use `$t`. */
export const t = i18next.t.bind(i18next);

/** Number and date formatting in the current language. */
export const locale = () => ({ en: 'en-US', es: 'es-AR', pt: 'pt-BR', fr: 'fr-FR', it: 'it-IT' })[language.value];

export default i18next;
