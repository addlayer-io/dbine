import type { NotebookTarget, ProjectEnvironment, SavedConnection } from '../api/types';
import { t } from '../i18n';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import { readJson, writeJson } from '../stores/storage';
import { emptyTarget, matchConnection } from './notebookModel';

// Where a notebook's cells run (docs/notebooks.md › Where a cell runs).
// The file names its targets — a connection's name and a database, or a
// project environment — never ids or credentials. Here those names become
// this machine's connections: by name, else by the choice made the first
// time a name didn't match (kept per machine, like a project's
// environments). A name that several local connections share is asked
// once per file (and remembered for that file on this machine).

const ALIASES_KEY = 'dbine.notebook.aliases';
const FILE_ALIASES_KEY = 'dbine.notebook.fileAliases';

/** Connection name in a file → this machine's connection id. */
export function notebookAliases(): Record<string, string> {
  return readJson<Record<string, string>>(ALIASES_KEY, {});
}

export function rememberAlias(name: string, connectionId: string) {
  writeJson(ALIASES_KEY, { ...notebookAliases(), [name]: connectionId });
}

/** A file's identity on this machine: `p:<project>:<path>`, `f:<path>` or `u:<tab>`. */
export type FileKey = string;

function fileAliasesAll(): Record<FileKey, Record<string, string>> {
  return readJson<Record<FileKey, Record<string, string>>>(FILE_ALIASES_KEY, {});
}
/** Connection name → id, chosen for this file (names several connections share). */
export function fileAlias(file: FileKey | null | undefined, name: string): string | null {
  return file ? fileAliasesAll()[file]?.[name] ?? null : null;
}
export function rememberFileAlias(file: FileKey, name: string, connectionId: string) {
  const all = fileAliasesAll();
  if (all[file]?.[name] === connectionId) return;
  writeJson(FILE_ALIASES_KEY, { ...all, [file]: { ...all[file], [name]: connectionId } });
}
/** An untitled notebook was saved: its choices follow it to its file. */
export function moveFileAliases(from: FileKey, to: FileKey) {
  const all = fileAliasesAll();
  if (!all[from] || from === to) return;
  all[to] = { ...all[from], ...all[to] };
  delete all[from];
  writeJson(FILE_ALIASES_KEY, all);
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
   *  name (`name`), or several (`candidates`: their ids); `environment`:
   *  the project's environment isn't bound here (or doesn't exist). */
  | { ok: false; problem: 'none' | 'connection' | 'environment'; name: string | null; candidates?: string[] };

/** The connection a file's name refers to here (`file`: its per-file choices). */
export function matchName(name: string, file?: FileKey | null) {
  const conns = useConnectionsStore();
  return matchConnection(conns.list, name, fileAlias(file, name), notebookAliases()[name] ?? null);
}
export function connectionByName(name: string, file?: FileKey | null): SavedConnection | undefined {
  const m = matchName(name, file);
  return m.kind === 'one' ? useConnectionsStore().byId(m.id) : undefined;
}

export function targetLabel(connectionId: string, database: string): string {
  const name = useConnectionsStore().byId(connectionId)?.name ?? '';
  return database ? `${name} › ${database}` : name;
}

/** The target a project's notebooks use when they name none: its active
 *  environment, or its direct base (`file`: a name other connections share
 *  is remembered for it). */
export function projectDefaultTarget(projectId: string, file?: FileKey | null): NotebookTarget | null {
  const projects = useProjectsStore();
  const a = projects.activeTarget(projectId);
  if (a.env) return { connection: null, database: null, environment: a.env.name };
  if (a.target && useConnectionsStore().byId(a.target.connection_id)) return targetOf(a.target.connection_id, a.target.database, file);
  return null;
}

/** Resolve a stored target (`projectId`: the notebook's project, for
 *  environments; `file`: the notebook, for names several connections share). */
export function resolveTarget(target: NotebookTarget | null, projectId?: string | null, file?: FileKey | null): Resolution {
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
  const m = matchName(target!.connection!, file);
  const c = m.kind === 'one' ? conns.byId(m.id) : undefined;
  if (!c) return { ok: false, problem: 'connection', name: target!.connection, candidates: m.kind === 'ambiguous' ? m.ids : undefined };
  const database = target!.database ?? c.config.database ?? '';
  return { ok: true, target: { connectionId: c.id, database, env: null, label: targetLabel(c.id, database) } };
}

/** How a target reads on screen without resolving it ("Ventas prod › ventas", "dev"). */
export function targetText(target: NotebookTarget | null): string {
  if (emptyTarget(target)) return t('notebook:target.none');
  if (target!.environment) return target!.environment;
  return target!.database ? `${target!.connection} › ${target!.database}` : target!.connection!;
}

/** A local connection and database as a file stores them (by name). A
 *  name other local connections share: the choice is remembered for `file`. */
export function targetOf(connectionId: string, database: string, file?: FileKey | null): NotebookTarget {
  const conns = useConnectionsStore();
  const name = conns.byId(connectionId)?.name ?? '';
  if (file && name && conns.list.filter((c) => c.name === name).length > 1) rememberFileAlias(file, name, connectionId);
  return { connection: name, database: database || null, environment: null };
}
