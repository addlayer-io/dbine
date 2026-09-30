import { ref } from 'vue';
import i18next, { t } from '../i18n';

// Labels that exported constants (FAMILY_LABELS, COPY_FORMATS…) hand to
// components. They're getters, so every read returns the current language;
// reading them also tracks this counter, so computeds and templates that use
// them update when the language changes.

const rev = ref(0);
i18next.on('languageChanged', () => { rev.value++; });

/** `t()` that makes the computed / render reading it follow the language. */
export function rt(key: string, options?: Record<string, unknown>): string {
  void rev.value;
  return t(key, options ?? {}) as string;
}

/** An object with the same keys whose values are `rt(keys[k])` getters. */
export function labelMap<K extends string>(keys: Record<K, string>): Record<K, string> {
  const out = {} as Record<K, string>;
  for (const k of Object.keys(keys) as K[]) {
    Object.defineProperty(out, k, { enumerable: true, get: () => rt(keys[k]) });
  }
  return out;
}
