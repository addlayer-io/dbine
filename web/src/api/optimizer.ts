import { invoke } from '@tauri-apps/api/core';
import type { Plan } from './types';
import type { AiProviderKind } from './ai';

// "Optimizar consulta" (docs/optimizar-consulta.md): mirrors of
// src-tauri/src/optimizer and commands/optimizer.rs (snake_case fields).

export type CandidateSource = 'rule' | 'ai' | 'user';

export interface Candidate {
  id: string;
  source: CandidateSource;
  /** The rule that wrote it: `optimizer:rules.<rule>`. */
  rule: string | null;
  params: Record<string, string>;
  /** The AI's own title and explanation. */
  title: string | null;
  explanation: string | null;
  sql: string;
  /** Not proven equivalent in every case: compare it before using it. */
  verify: boolean;
}

export interface OptimizerNote {
  rule: string;
  params: Record<string, string>;
  sql: string | null;
}

export interface IndexHint {
  /** missing_index (the engine's own) · full_scan · scan (columns unknown). */
  reason: 'missing_index' | 'full_scan' | 'scan';
  table: string;
  columns: string[];
  est_rows: number | null;
  impact: number | null;
  script: string | null;
}

export interface PlanWarning { op: string; object: string | null; text: string }

export interface Analysis {
  language: string;
  dialect: string;
  engine: string;
  writes: boolean;
  supports_explain: boolean;
  candidates: Candidate[];
  notes: OptimizerNote[];
  hints: IndexHint[];
  warnings: PlanWarning[];
  plans: Plan[];
  cost: number | null;
  skipped: string[];
}

export interface AiAlternatives { candidates: Candidate[]; none: boolean; sent: string[] }

export interface Measure {
  id: string;
  executed: boolean;
  error: string | null;
  runs_ms: number[];
  min_ms: number | null;
  avg_ms: number | null;
  rows: number | null;
  truncated: boolean;
  checksum: string | null;
  equivalent: boolean | null;
  cost: number | null;
  plans: Plan[];
  plan_error: string | null;
}

export const optimizerApi = {
  analyze: (connectionId: string, database: string, sql: string, runId: string) =>
    invoke<Analysis>('optimizer_analyze', { args: { connection_id: connectionId, database, sql, run_id: runId } }),
  ai: (connectionId: string, database: string, sql: string, runId: string, provider: AiProviderKind, model: string | null, plans: Plan[]) =>
    invoke<AiAlternatives>('optimizer_ai', { args: { connection_id: connectionId, database, sql, run_id: runId, provider, model, plans } }),
  /** `versions`: the original first. Each measure also arrives as an `optimizer-progress` event. */
  compare: (connectionId: string, database: string, runId: string, versions: { id: string; sql: string }[], runs: number, maxRows: number) =>
    invoke<Measure[]>('optimizer_compare', { args: { connection_id: connectionId, database, run_id: runId, versions, runs, max_rows: maxRows } }),
  cancel: (runId: string) => invoke<void>('optimizer_cancel', { args: { run_id: runId } }),
};
