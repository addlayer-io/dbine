import type { Language } from 'element-plus/es/locale';
import type { Lang } from './index';

// Element Plus's own texts (date pickers, "no data"…) per app language, each
// loaded with its language (see `loadLanguage` in ./index.ts).
const loaders: Record<Lang, () => Promise<{ default: Language }>> = {
  en: () => import('element-plus/es/locale/lang/en.mjs'),
  es: () => import('element-plus/es/locale/lang/es.mjs'),
  pt: () => import('element-plus/es/locale/lang/pt-br.mjs'),
  fr: () => import('element-plus/es/locale/lang/fr.mjs'),
  it: () => import('element-plus/es/locale/lang/it.mjs'),
  de: () => import('element-plus/es/locale/lang/de.mjs'),
  ru: () => import('element-plus/es/locale/lang/ru.mjs'),
  zh: () => import('element-plus/es/locale/lang/zh-cn.mjs'),
  ja: () => import('element-plus/es/locale/lang/ja.mjs'),
  ko: () => import('element-plus/es/locale/lang/ko.mjs'),
};

/** Element Plus's texts per app language. The current language's entry is
 *  always present: `setLanguage` switches only after its locale has loaded,
 *  and the app mounts after `i18nReady`. */
export const ELEMENT_LOCALES = {} as Record<Lang, Language>;

export async function loadElementLocale(code: Lang) {
  if (ELEMENT_LOCALES[code]) return;
  ELEMENT_LOCALES[code] = (await loaders[code]()).default;
}
