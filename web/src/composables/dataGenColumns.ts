import type { VectorMeta } from '../api/types';
import { shownVectorMeta, vectorOfType } from './vector';

// "Generar datos de prueba…" (GenerateDataDialog.vue): what each column
// offers and what the backend gets (src-tauri/src/commands/datagen.rs).
// The vector generator fills a column with random vectors; the backend picks
// it by itself ("auto") where its catalog reads the column's type as a vector
// on that engine. Where it doesn't, though the type says it (a Redis search
// index's `VECTOR(3, FLOAT32)` field), the dialog chooses it, with the type's
// dimensions. No Vue imports, so `node --test` loads it
// (dataGenColumns.test.mjs).

/** A table column as `datagen_preview` describes it. */
export interface GenColumn {
  name: string;
  data_type: string;
  nullable: boolean;
  primary_key: boolean;
  auto_increment: boolean;
  foreign_key: string | null;
  /** The generator the backend's "auto" picks. */
  auto: string;
}
/** A column's choice in the dialog. */
export interface GenSpec { generator: string; params: Record<string, string>; null_percent: number }
/** A column's choice as the backend takes it. */
export interface SentSpec extends GenSpec { name: string }

/** The settings each generator takes. */
export const GEN_PARAMS: Record<string, string[]> = {
  fixed: ['value'],
  sequence: ['start', 'step'],
  integer: ['min', 'max'],
  decimal: ['min', 'max', 'scale'],
  date: ['from', 'to'],
  datetime: ['from', 'to'],
  text: ['min', 'max'],
  list: ['values'],
  vector: ['dims'],
};

/** A dense vector column of numbers the backend doesn't fill with vectors by
 *  itself, though its type says it holds them: its "auto" isn't the vector
 *  generator. Null for any other column. */
export function unclassifiedVector(c: Pick<GenColumn, 'data_type' | 'auto'>): VectorMeta | null {
  if (c.auto === 'vector') return null;
  const m = vectorOfType(c.data_type);
  return m && !m.sparse && m.element !== 'other' ? m : null;
}

/** What "Automático" fills the column with: the backend's pick, or the
 *  vector generator for a vector the backend doesn't read as one. */
export const autoOf = (c: Pick<GenColumn, 'data_type' | 'auto'>) => (unclassifiedVector(c) ? 'vector' : c.auto);

/** The dimensions the column's type fixes (`vector(1536)`, `FLOAT[3]`):
 *  its vectors take that many numbers, nothing else. */
const fixedDims = (c: Pick<GenColumn, 'data_type'>) => shownVectorMeta({ type_name: c.data_type })?.dims;

/** A column's settings: a vector's dimensions only where its type doesn't
 *  fix them. */
export function paramsOf(c: Pick<GenColumn, 'data_type'>, s: GenSpec | undefined): string[] {
  const g = s?.generator ?? 'auto';
  if (g === 'vector' && fixedDims(c)) return [];
  return GEN_PARAMS[g] ?? [];
}

/** The generators a column can choose: "auto", the backend's list and the
 *  vector one when a column is a vector the backend doesn't read as one. */
export function generatorOptions(generators: string[], columns: Pick<GenColumn, 'data_type' | 'auto'>[]): string[] {
  const out = ['auto', ...generators];
  if (!out.includes('vector') && columns.some((c) => unclassifiedVector(c))) out.push('vector');
  return out;
}

/**
 * The columns' choices as the backend gets them: those not left to "auto"
 * (or with a share of NULLs), plus the vector generator for a vector the
 * backend doesn't read as one. A vector generator on a type that fixes the
 * dimensions sends them: the backend knows them only where its catalog
 * reads the type.
 */
export function sentSpecs(columns: GenColumn[], specs: Record<string, GenSpec>): SentSpec[] {
  const out: SentSpec[] = [];
  for (const c of columns) {
    const s = specs[c.name];
    if (!s) continue;
    const generator = s.generator === 'auto' && unclassifiedVector(c) ? 'vector' : s.generator;
    if (generator === 'auto' && s.null_percent <= 0) continue;
    const dims = generator === 'vector' ? fixedDims(c) : undefined;
    const params = dims ? { ...s.params, dims: String(dims) } : s.params;
    out.push({ name: c.name, generator, params, null_percent: s.null_percent });
  }
  return out;
}

/** How a sample column shows its cells: `meta` reads them as vectors (null:
 *  as text), `shown` is what the badge says (null: only the dimensions). By
 *  the column's type; else, for a type that doesn't say it, as dense
 *  vectors of an element nobody claims when the backend fills it with them
 *  (its "auto" or the chosen generator is the vector one). */
export function sampleVectorOf(c: Pick<GenColumn, 'data_type' | 'auto'> | undefined, s: GenSpec | undefined): { meta: VectorMeta | null; shown: VectorMeta | null } {
  const typed = shownVectorMeta({ type_name: c?.data_type });
  if (typed || !c) return { meta: typed, shown: typed };
  const filled = c.auto === 'vector' || s?.generator === 'vector';
  return filled ? { meta: { element: 'f32' }, shown: null } : { meta: null, shown: null };
}
