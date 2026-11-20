import { t } from '../i18n';
import type { VectorIndex } from '../api/schema-types';
import { ELEMENT_NAMES, isStandardMetric, isStandardParam, RANGES, shortNumber, vectorIndexText, type VectorProblem } from './vector';

// The texts of vectors (composables/vector.ts) in the interface language:
// the standard metric and parameter names translated, any other as the
// engine spells it, and why an edited vector is refused.

/** A distance's label: `cosine` → «Coseno»; an engine's own name as is. */
export const metricLabel = (m: string) => (isStandardMetric(m) ? t(`vectors:metric.${m}`) : m);

/** A distance's short label, for the explorer's index tag: `l2` → «L2»,
 *  `cosine` → «Coseno»; the whole one (`metricLabel`) is on hover. */
export const metricShortLabel = (m: string) => (isStandardMetric(m) ? t(`vectors:metricShort.${m}`) : m);

/** A build parameter's label: `m` → «Conexiones por nodo (m)»; others as is. */
export const paramLabel = (p: string) => (isStandardParam(p) ? t(`vectors:param.${p}`) : p);

/** A vector index in one line, its metric translated. */
export const vectorIndexLine = (v: VectorIndex | null | undefined) => vectorIndexText(v, metricLabel);

/** Why the value typed in vector column `column` can't be written. */
export function vectorProblemText(p: VectorProblem, column: string): string {
  if (p.kind === 'dims') return t('vectors:edit.dims', { column, expected: p.expected, got: p.got });
  if (p.kind === 'range') {
    const [min, max] = RANGES[p.element] ?? [0, 1];
    return t('vectors:edit.range', { column, min, max, position: p.position + 1, value: p.value });
  }
  if (p.kind === 'overflow') {
    return t('vectors:edit.overflow', { column, element: ELEMENT_NAMES[p.element], max: shortNumber(p.max), position: p.position + 1, value: p.value });
  }
  if (p.kind === 'empty') return t('vectors:edit.empty', { column });
  return t('vectors:edit.format', { column });
}
