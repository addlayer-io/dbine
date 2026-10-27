import i18next from 'i18next';

// The phone app's texts, in DBine's languages (Spanish is the source). The
// list mirrors scripts/i18n/languages.json; `scripts/i18n/check.py` fails
// when they differ, and checks every `locales/<lang>.json` against es.json.
const LANGS = ['es', 'en', 'pt', 'fr', 'it', 'de', 'ru', 'zh', 'ja', 'ko', 'uk', 'pl', 'id'];

const files = import.meta.glob<Record<string, unknown>>('./locales/*.json', { eager: true, import: 'default' });
const resources: Record<string, { translation: Record<string, unknown> }> = {};
for (const lang of LANGS) {
  const texts = files[`./locales/${lang}.json`];
  if (texts) resources[lang] = { translation: texts };
}

/** The phone's language when it's one of ours (by its primary subtag, so
 *  every Chinese variant gets `zh`); English otherwise. */
function phoneLanguage(): string {
  for (const tag of navigator.languages?.length ? navigator.languages : [navigator.language]) {
    const code = tag?.split(/[-_]/)[0]?.toLowerCase();
    if (code && resources[code]) return code;
  }
  return 'en';
}

i18next.init({
  lng: phoneLanguage(),
  fallbackLng: 'es',
  resources,
  interpolation: { escapeValue: false },
});

export default i18next;
