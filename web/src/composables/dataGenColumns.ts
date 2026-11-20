import type { VectorMeta } from '../api/types';
import { shownVectorMeta, vectorOfType } from './vector';

// "Generar datos de prueba…" (GenerateDataDialog.vue): what each column
// offers and what the backend gets (src-tauri/src/commands/datagen.rs).
// The vector generator fills a column with random vectors; the backend picks
// it by itself ("auto") where its catalog reads the column's type as a vector
// on that engine, and says so with the column (`vector`: its dimensions and
// numbers), which the dialog goes by first. Where a backend doesn't (an older
// one, without `vector`), though the type says it, the dialog chooses it,
// with the type's dimensions. No Vue imports, so `node --test` loads it
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
  /** The vectors the column holds, as the backend reads its type on its
   *  engine; absent: not a vector there (or an older backend). */
  vector?: VectorMeta | null;
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
 *  itself, though its type says it holds them: it doesn't say the column's
 *  vector and its "auto" isn't the vector generator. Null for any other
 *  column, and for one whose vector the backend says (its "auto" is its
 *  call). */
export function unclassifiedVector(c: Pick<GenColumn, 'data_type' | 'auto' | 'vector'>): VectorMeta | null {
  if (c.vector || c.auto === 'vector') return null;
  const m = vectorOfType(c.data_type);
  return m && !m.sparse && m.element !== 'other' ? m : null;
}

/** What "Automático" fills the column with: the backend's pick, or the
 *  vector generator for a vector the backend doesn't read as one. */
export const autoOf = (c: Pick<GenColumn, 'data_type' | 'auto' | 'vector'>) => (unclassifiedVector(c) ? 'vector' : c.auto);

/** The column's vector: the backend's, else what its type's name says. */
const vectorOf = (c: Pick<GenColumn, 'data_type' | 'vector'>) => shownVectorMeta({ type_name: c.data_type, vector: c.vector });

/** The dimensions the column's type fixes (`vector(1536)`, `FLOAT[3]`):
 *  its vectors take that many numbers, nothing else. */
const fixedDims = (c: Pick<GenColumn, 'data_type' | 'vector'>) => vectorOf(c)?.dims;

/** A column's settings: a vector's dimensions only where its type doesn't
 *  fix them. */
export function paramsOf(c: Pick<GenColumn, 'data_type' | 'vector'>, s: GenSpec | undefined): string[] {
  const g = s?.generator ?? 'auto';
  if (g === 'vector' && fixedDims(c)) return [];
  return GEN_PARAMS[g] ?? [];
}

/** The generators a column can choose: "auto", the backend's list and the
 *  vector one when a column is a vector the backend doesn't read as one. */
export function generatorOptions(generators: string[], columns: Pick<GenColumn, 'data_type' | 'auto' | 'vector'>[]): string[] {
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
 *  the column's vector as the backend says it, else its type; else, for a
 *  type that doesn't say it, as dense vectors of an element nobody claims
 *  when the backend fills it with them (its "auto" or the chosen generator
 *  is the vector one). */
export function sampleVectorOf(c: Pick<GenColumn, 'data_type' | 'auto' | 'vector'> | undefined, s: GenSpec | undefined): { meta: VectorMeta | null; shown: VectorMeta | null } {
  const typed = c ? vectorOf(c) : null;
  if (typed || !c) return { meta: typed, shown: typed };
  const filled = c.auto === 'vector' || s?.generator === 'vector';
  return filled ? { meta: { element: 'f32' }, shown: null } : { meta: null, shown: null };
}
