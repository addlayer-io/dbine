import { invoke } from '@tauri-apps/api/core';

// "Calidad de código": the editor's linter. Mirrors src-tauri/src/lint and
// src-tauri/src/commands/lint.rs.

export type LintSeverity = 'error' | 'warning' | 'info';

export type LintGroup =
  | 'sql' | 'tsql' | 'postgres' | 'mysql' | 'oracle' | 'influxql' | 'cql'
  | 'mongodb' | 'couchdb' | 'search' | 'redis' | 'etcd' | 'cypher';

export interface LintRule {
  id: string;
  severity: LintSeverity;
  groups: LintGroup[];
}

/** A problem: `start`/`end` are indices into the editor's text. */
export interface LintFinding {
  rule: string;
  severity: LintSeverity;
  start: number;
  end: number;
  line: number;
  /** Values its message shows (`{{fn}}`, `{{table}}`…). */
  params: Record<string, string>;
}

export const lintApi = {
  lint: (connectionId: string, sql: string) =>
    invoke<LintFinding[]>('lint_script', { args: { connection_id: connectionId, sql } }),
  rules: () => invoke<LintRule[]>('lint_rules'),
};
