import type { NotebookTarget, ProjectEnvironment, SavedConnection } from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import { readJson, writeJson } from '../stores/storage';
import { emptyTarget } from './notebookModel';

// Where a notebook's cells run (docs/notebooks.md › Where a cell runs).
// The file names its targets — a connection's name and a database, or a
// project environment — never ids or credentials. Here those names become
// this machine's connections: by name, else by the choice made the first
// time a name didn't match (kept per machine, like a project's
// environments).

const ALIASES_KEY = 'dbine.notebook.aliases';

/** Connection name in a file → this machine's connection id. */
export function notebookAliases(): Record<string, string> {
  return readJson<Record<string, string>>(ALIASES_KEY, {});
}

export function rememberAlias(name: string, connectionId: string) {
  writeJson(ALIASES_KEY, { ...notebookAliases(), [name]: connectionId });
}

/** A target resolved to a local connection and database. */
export interface ResolvedTarget {
  connectionId: string;
  database: string;
  /** The project environment it came from (its `confirm_run`). */
  env: ProjectEnvironment | null;
  /** "Ventas prod › ventas" (or "dev · Ventas prod › ventas"). */
  label: string;
}

export type Resolution =
  | { ok: true; target: ResolvedTarget }
  /** `none`: no target at all; `connection`: no local connection by that
   *  name (`name`); `environment`: the project's environment isn't bound
   *  here (or doesn't exist). */
  | { ok: false; problem: 'none' | 'connection' | 'environment'; name: string | null };

/** The connection a file's name refers to here. */
export function connectionByName(name: string): SavedConnection | undefined {
  const conns = useConnectionsStore();
  const exact = conns.list.find((c) => c.name === name);
  if (exact) return exact;
  const alias = notebookAliases()[name];
  const aliased = alias ? conns.byId(alias) : undefined;
  if (aliased) return aliased;
  const loose = conns.list.filter((c) => c.name.trim().toLowerCase() === name.trim().toLowerCase());
  return loose.length === 1 ? loose[0] : undefined;
}

export function targetLabel(connectionId: string, database: string): string {
  const name = useConnectionsStore().byId(connectionId)?.name ?? '';
  return database ? `${name} › ${database}` : name;
}

/** The target a project's notebooks use when they name none: its active
 *  environment, or its direct base. */
export function projectDefaultTarget(projectId: string): NotebookTarget | null {
  const projects = useProjectsStore();
  const a = projects.activeTarget(projectId);
  if (a.env) return { connection: null, database: null, environment: a.env.name };
  if (a.target) {
    const name = useConnectionsStore().byId(a.target.connection_id)?.name;
    if (name) return { connection: name, database: a.target.database || null, environment: null };
  }
  return null;
}

/** Resolve a stored target (`projectId`: the notebook's project, for environments). */
export function resolveTarget(target: NotebookTarget | null, projectId?: string | null): Resolution {
  if (emptyTarget(target)) return { ok: false, problem: 'none', name: null };
  const conns = useConnectionsStore();
  if (target!.environment) {
    const env = target!.environment;
    const p = projectId ? useProjectsStore().byId(projectId) : undefined;
    const def = p?.manifest?.environments.find((e) => e.name.toLowerCase() === env.toLowerCase()) ?? null;
    const bound = p ? p.binding.environments[def?.name ?? env] ?? p.binding.environments[env] : undefined;
    if (!bound || !conns.byId(bound.connection_id)) return { ok: false, problem: 'environment', name: env };
    return {
      ok: true,
      target: { connectionId: bound.connection_id, database: bound.database, env: def, label: `${def?.name ?? env} · ${targetLabel(bound.connection_id, bound.database)}` },
    };
  }
  const c = connectionByName(target!.connection!);
  if (!c) return { ok: false, problem: 'connection', name: target!.connection };
  const database = target!.database ?? c.config.database ?? '';
  return { ok: true, target: { connectionId: c.id, database, env: null, label: targetLabel(c.id, database) } };
}

/** How a target reads on screen without resolving it ("Ventas prod › ventas", "dev"). */
export function targetText(target: NotebookTarget | null): string {
  if (emptyTarget(target)) return t('notebook:target.none');
  if (target!.environment) return target!.environment;
  return target!.database ? `${target!.connection} › ${target!.database}` : target!.connection!;
}

/** A local connection and database as a file stores them (by name). */
export function targetOf(connectionId: string, database: string): NotebookTarget {
  return { connection: useConnectionsStore().byId(connectionId)?.name ?? '', database: database || null, environment: null };
}
