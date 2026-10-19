import elEn from 'element-plus/es/locale/lang/en.mjs';
import elEs from 'element-plus/es/locale/lang/es.mjs';
import elPt from 'element-plus/es/locale/lang/pt-br.mjs';
import elFr from 'element-plus/es/locale/lang/fr.mjs';
import elIt from 'element-plus/es/locale/lang/it.mjs';
import elDe from 'element-plus/es/locale/lang/de.mjs';
import elRu from 'element-plus/es/locale/lang/ru.mjs';
import elZh from 'element-plus/es/locale/lang/zh-cn.mjs';
import elJa from 'element-plus/es/locale/lang/ja.mjs';
import elKo from 'element-plus/es/locale/lang/ko.mjs';
import type { Language } from 'element-plus/es/locale';
import type { Lang } from './index';

/** Element Plus's own texts (date pickers, "no data"…) per app language. */
export const ELEMENT_LOCALES: Record<Lang, Language> = {
  en: elEn, es: elEs, pt: elPt, fr: elFr, it: elIt, de: elDe, ru: elRu, zh: elZh, ja: elJa, ko: elKo,
};
