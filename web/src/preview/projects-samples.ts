// view=projects: the whole workbench (App.vue) over a fake backend with two
// linked projects, to click through Proyectos without Tauri, git or a
// database: the sidebar, the file tree, environments, file tabs (edit,
// ⌘S), the changes and their diff, commit / pull / push, "Vincular
// proyecto…" and the Explorer's "Proyectos" node.
// &sidebar=explorer starts on the Explorer.

import type {
  ChangeMark, FileChange, FileStat, FolderInspect, FsEntry, ProjectBinding, ProjectInfo, ProjectStatus, SavedConnection,
} from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers } from './monitor-samples';

const now = '2026-10-03T12:00:00Z';

const conn = (id: string, name: string, color: string | null, database: string): SavedConnection => ({
  id, name, color, folder_id: null, tags: [], save_password: true, updated_at: now,
  config: { driver: 'postgres', host: `${id}.empresa.local`, port: 5432, database, username: 'app', password: null, encrypt: true, trust_server_certificate: false, read_only: false, options: {} },
});
const CONNECTIONS = [
  conn('c1', 'Producción · ventas', '#d4443b', 'ventas'),
  conn('c2', 'QA · ventas', '#e5c07b', 'ventas_qa'),
  conn('c3', 'Local', null, 'ventas_dev'),
];
const DATABASES: Record<string, string[]> = { c1: ['ventas', 'ventas_hist'], c2: ['ventas_qa'], c3: ['ventas_dev', 'postgres'] };

// -- the repos: files in memory ------------------------------------------------------------
const head: Record<string, Record<string, string>> = {
  p1: {
    '.dbine.json': '{\n  "version": 1,\n  "name": "scripts-ventas",\n  "engine": "postgres",\n  "environments": [\n    { "name": "dev" },\n    { "name": "qa" },\n    { "name": "prod", "confirm_run": true, "description": "Producción" }\n  ],\n  "default_environment": "dev"\n}\n',
    'README.md': '# Scripts de ventas\n\nConsultas y migraciones de la base de ventas.\n',
    'consultas/clientes.sql': "select id, nombre, email\nfrom clientes\nwhere activo = true\norder by nombre;\n",
    'consultas/pedidos_mes.sql': "select date_trunc('month', fecha) as mes, count(*)\nfrom pedidos\ngroup by 1\norder by 1 desc;\n",
    'migraciones/001_clientes.sql': 'create table clientes (\n  id serial primary key,\n  nombre text not null\n);\n',
    'migraciones/002_old.sql': '-- reemplazada por 003\nalter table clientes add column legacy int;\n',
    'migraciones/003_email.sql': 'alter table clientes add column email text;\n',
  },
  p2: {
    'mensual.sql': "select * from ventas_resumen where mes = date_trunc('month', now());\n",
    'anual.sql': 'select extract(year from fecha) as anio, sum(total)\nfrom pedidos\ngroup by 1;\n',
  },
};
// The working tree: one file edited, one new, one deleted.
const work: Record<string, Record<string, string>> = {
  p1: { ...head.p1 },
  p2: { ...head.p2 },
};
work.p1['consultas/clientes.sql'] = "select id, nombre, email, telefono\nfrom clientes\nwhere activo = true\n  and pais = 'AR'\norder by nombre;\n";
work.p1['consultas/nuevo.sql'] = '-- ventas por vendedor\nselect vendedor, sum(total)\nfrom pedidos\ngroup by vendedor;\n';
delete work.p1['migraciones/002_old.sql'];
work.p1['.gitignore'] = 'tmp/\n';
work.p1['tmp/borrador.sql'] = 'select 1;\n';
head.p1['.gitignore'] = 'tmp/\n';

const git: Record<string, { ahead: number; behind: number; branch: string; remote: boolean; conflict: boolean }> = {
  p1: { ahead: 1, behind: 2, branch: 'main', remote: true, conflict: false },
  p2: { ahead: 0, behind: 0, branch: 'main', remote: false, conflict: false },
};

const binding = (b: Partial<ProjectBinding>): ProjectBinding => ({ direct: null, environments: {}, active_environment: null, ...b });
let projects: ProjectInfo[] = [
  {
    id: 'p1', name: 'scripts-ventas', path: '/Users/demo/DBine/Proyectos/scripts-ventas', sort_order: 0, created_at: now, updated_at: now,
    binding: binding({
      environments: { dev: { connection_id: 'c3', database: 'ventas_dev' }, qa: { connection_id: 'c2', database: 'ventas_qa' }, prod: { connection_id: 'c1', database: 'ventas' } },
      active_environment: 'dev',
    }),
    exists: true, is_repo: true, manifest_error: null, manifest_warnings: [],
    manifest: {
      version: 1, name: 'scripts-ventas', engine: 'postgres', default_environment: 'dev',
      environments: [
        { name: 'dev', engine: null, confirm_run: false, description: '' },
        { name: 'qa', engine: null, confirm_run: false, description: '' },
        { name: 'prod', engine: null, confirm_run: true, description: 'Producción' },
      ],
    },
  },
  {
    id: 'p2', name: 'reportes', path: '/Users/demo/code/reportes', sort_order: 1, created_at: now, updated_at: now,
    binding: binding({ direct: { connection_id: 'c1', database: 'ventas' } }),
    exists: true, is_repo: true, manifest: null, manifest_error: null, manifest_warnings: [],
  },
];

function hash(s: string): string {
  let h = 2166136261;
  for (let i = 0; i < s.length; i++) h = Math.imul(h ^ s.charCodeAt(i), 16777619) >>> 0;
  return h.toString(16).padStart(8, '0');
}
const mtimes: Record<string, number> = {};
const stat = (id: string, path: string): FileStat => {
  const t = work[id][path];
  return t === undefined
    ? { path, exists: false, mtime_ms: 0, size: 0, hash: '' }
    : { path, exists: true, mtime_ms: mtimes[`${id}:${path}`] ?? 1_759_000_000_000, size: t.length, hash: hash(t) };
};

function changes(id: string): FileChange[] {
  const out: FileChange[] = [];
  const paths = new Set([...Object.keys(head[id]), ...Object.keys(work[id])]);
  for (const p of [...paths].sort()) {
    if (p.startsWith('tmp/')) continue;
    const a = head[id][p];
    const b = work[id][p];
    let mark: ChangeMark | null = null;
    if (a === undefined) mark = 'U';
    else if (b === undefined) mark = 'D';
    else if (a !== b) mark = 'M';
    if (git[id].conflict && p === 'consultas/pedidos_mes.sql') mark = 'C';
    if (mark) out.push({ path: p, orig_path: null, index: mark === 'U' ? '?' : '.', worktree: mark === 'U' ? '?' : mark, mark });
  }
  return out;
}

function status(id: string): ProjectStatus {
  const g = git[id];
  return {
    git: true, exists: true, is_repo: true, branch: g.branch, detached: false, head: id === 'p1' ? 'a1b2c3d' : '9f8e7d6',
    upstream: g.remote ? `origin/${g.branch}` : null, remote: g.remote ? 'origin' : null, has_remote: g.remote,
    ahead: g.ahead, behind: g.behind, changes: changes(id), truncated: false,
    operation: g.conflict ? 'merge' : null, last_commit: 'Agrega email a clientes · Ana Pérez · 02/10/2026 18:40',
    fetch_error: null, identity_missing: false,
  };
}

function listDir(id: string, dir: string): FsEntry[] {
  const prefix = dir ? `${dir}/` : '';
  const seen = new Map<string, FsEntry>();
  for (const p of Object.keys(work[id])) {
    if (!p.startsWith(prefix)) continue;
    const rest = p.slice(prefix.length);
    const name = rest.split('/')[0];
    const isDir = rest.includes('/');
    const path = prefix + name;
    if (!seen.has(name)) seen.set(name, { name, path, is_dir: isDir, symlink: false, size: isDir ? 0 : work[id][p].length, ignored: path.startsWith('tmp') });
  }
  return [...seen.values()].sort((a, b) => (a.is_dir === b.is_dir ? a.name.localeCompare(b.name, undefined, { sensitivity: 'base' }) : a.is_dir ? -1 : 1));
}

const wait = (ms: number) => new Promise((r) => setTimeout(r, ms));
const log = (...a: unknown[]) => console.log('[projects-preview]', ...a);

type Args = Record<string, unknown>;
async function projectCmd(cmd: string, a: Args): Promise<unknown> {
  const id = a.id as string;
  switch (cmd) {
    case 'list_projects': return projects;
    case 'project_default_dir': return '/Users/demo/DBine/Proyectos';
    case 'project_inspect_folder': {
      const path = String(a.path);
      const linked = projects.find((p) => p.path === path);
      const r: FolderInspect = {
        path, exists: !path.includes('noexiste'), is_repo: !path.includes('sin-git'),
        repo_root: path.endsWith('/sql') ? path.slice(0, -4) : path, already_linked: linked?.id ?? null,
        suggested_name: (path.endsWith('/sql') ? path.slice(0, -4) : path).split('/').filter(Boolean).pop() ?? 'proyecto', has_manifest: false, empty: false,
      };
      return r;
    }
    case 'project_link': {
      const path = String(a.path);
      const p: ProjectInfo = {
        id: `p${projects.length + 1}`, name: (a.name as string) || path.split('/').pop()!, path, sort_order: projects.length, created_at: now, updated_at: now,
        binding: (a.binding as ProjectBinding) ?? binding({}), exists: true, is_repo: true, manifest: null, manifest_error: null, manifest_warnings: [],
      };
      head[p.id] = { 'consulta.sql': 'select 1;\n' };
      work[p.id] = { ...head[p.id] };
      git[p.id] = { ahead: 0, behind: 0, branch: 'main', remote: false, conflict: false };
      projects = [...projects, p];
      log('link', a);
      return p;
    }
    case 'project_update': {
      const p = projects.find((x) => x.id === id)!;
      p.name = String(a.name);
      return p;
    }
    case 'project_set_binding': {
      const p = projects.find((x) => x.id === id)!;
      p.binding = a.binding as ProjectBinding;
      log('binding', JSON.stringify(p.binding));
      return { ...p };
    }
    case 'project_unlink': projects = projects.filter((x) => x.id !== id); return null;
    case 'project_write_manifest': {
      const p = projects.find((x) => x.id === id)!;
      p.manifest = a.manifest as ProjectInfo['manifest'];
      work[id]['.dbine.json'] = `${JSON.stringify(a.manifest, null, 2)}\n`;
      return { ...p };
    }
    case 'project_list_dir': await wait(60); return listDir(id, String(a.dir ?? ''));
    case 'project_read_file': {
      const path = String(a.path);
      const t = work[id][path];
      if (t === undefined) throw { kind: 'not_found', message: 'el archivo no existe' };
      const { mtime_ms, size, hash: h } = stat(id, path);
      return { path, text: t, eol: 'lf', bom: false, mtime_ms, size, hash: h };
    }
    case 'project_write_file': {
      const path = String(a.path);
      const cur = stat(id, path);
      if (a.expected_hash && cur.exists && cur.hash !== a.expected_hash) return { written: false, conflict: true, stat: cur };
      work[id][path] = String(a.text);
      mtimes[`${id}:${path}`] = Date.now();
      log('write', path);
      return { written: true, conflict: false, stat: stat(id, path) };
    }
    case 'project_stat_files': return (a.paths as string[]).map((p) => stat(id, p));
    case 'project_create_file': work[id][String(a.path)] = String(a.text ?? ''); return stat(id, String(a.path));
    case 'project_create_dir': work[id][`${a.path}/.keep`] = ''; return null;
    case 'project_rename': {
      const from = String(a.from);
      const to = String(a.to);
      for (const p of Object.keys(work[id])) {
        if (p === from || p.startsWith(`${from}/`)) { work[id][to + p.slice(from.length)] = work[id][p]; delete work[id][p]; }
      }
      return null;
    }
    case 'project_delete': {
      const path = String(a.path);
      for (const p of Object.keys(work[id])) if (p === path || p.startsWith(`${path}/`)) delete work[id][p];
      return null;
    }
    case 'project_reveal': log('reveal', a); return null;
    case 'project_set_remote': git[id].remote = true; return null;
    case 'project_set_identity': return null;
    case 'project_status': if (a.fetch) await wait(300); return status(id);
    case 'project_diff': {
      const path = String(a.path);
      const c = changes(id).find((x) => x.path === path);
      return {
        path, orig_path: null, mark: c?.mark ?? 'M',
        before: c?.mark === 'U' ? null : head[id][path] ?? null,
        after: c?.mark === 'D' ? null : work[id][path] ?? null,
        binary: false, too_large: false,
      };
    }
    case 'project_commit': {
      await wait(400);
      const msg = String(a.message).trim();
      if (!msg) throw { kind: 'bad_request', message: 'escribí un mensaje para el commit' };
      head[id] = { ...work[id] };
      git[id].ahead++;
      log('commit', msg);
      return 'e4f5a6b';
    }
    case 'project_pull': case 'project_sync': {
      await wait(900);
      const updated = git[id].behind ? ['consultas/pedidos_mes.sql', 'migraciones/004_indices.sql'] : [];
      if (git[id].behind) {
        head[id]['migraciones/004_indices.sql'] = 'create index ix_pedidos_fecha on pedidos (fecha);\n';
        work[id]['migraciones/004_indices.sql'] = head[id]['migraciones/004_indices.sql'];
      }
      git[id].behind = 0;
      const pushed = cmd === 'project_sync' && git[id].ahead > 0;
      if (pushed) git[id].ahead = 0;
      return { up_to_date: !updated.length, updated, conflicts: [], operation: null, note: null, ...(cmd === 'project_sync' ? { pushed } : {}) };
    }
    case 'project_push': await wait(700); git[id].ahead = 0; return false;
    case 'project_conflict': git[id].conflict = false; return null;
    case 'project_operation': git[id].conflict = false; return { up_to_date: true, updated: [], conflicts: [], operation: null };
    case 'project_discard': {
      for (const p of a.paths as string[]) {
        if (head[id][p] === undefined) delete work[id][p];
        else work[id][p] = head[id][p];
      }
      return null;
    }
    case 'project_git_cancel': return true;
    case 'files_report': case 'files_save_all_broadcast': return null;
    case 'files_unsaved_all': return [];
    default: return undefined;
  }
}

export function installProjectsPreview() {
  const params = new URLSearchParams(location.search);
  try {
    localStorage.setItem('dbine.sidebarView', JSON.stringify(params.get('sidebar') ?? 'projects'));
    localStorage.setItem('dbine.sidebarOpen', 'true');
    localStorage.setItem('dbine.sidebarWidth', '330');
    localStorage.setItem('dbine.aiOpen', 'false');
    localStorage.setItem('dbine.tabs', JSON.stringify({ tabs: [], activeId: null, collapsed: [] }));
    localStorage.setItem('dbine.openFolders', JSON.stringify(['c:c1', 'd:c1:ventas', 'ps:c1:ventas']));
    localStorage.removeItem('dbine.projects.expanded');
  } catch { /* preview */ }
  const conns = useConnectionsStore();
  for (const c of CONNECTIONS) {
    conns.live[c.id] = { status: 'connected', serverVersion: 'PostgreSQL 16.4', databases: DATABASES[c.id], defaultDatabase: DATABASES[c.id][0], error: null };
  }
  // Another editor changes a file on disk (the click-through calls it).
  (window as unknown as Record<string, unknown>).__previewTouch = (id: string, path: string) => {
    work[id][path] = `-- cambiado afuera\n${work[id][path] ?? ''}`;
    mtimes[`${id}:${path}`] = Date.now();
  };
  let cb = 0;
  (window as unknown as Record<string, unknown>).__TAURI_EVENT_PLUGIN_INTERNALS__ = { unregisterListener: () => {} };
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => ++cb,
    unregisterCallback: () => {},
    convertFileSrc: (p: string) => p,
    invoke: async (cmd: string, payload?: { args?: Args }) => {
      const a = payload?.args ?? {};
      const p = await projectCmd(cmd, a);
      if (p !== undefined) return p;
      switch (cmd) {
        case 'plugin:event|listen': return ++cb;
        case 'plugin:event|unlisten': return null;
        case 'plugin:dialog|open': return '/Users/demo/code/inventario';
        case 'list_drivers': return sampleDrivers;
        case 'list_connections': return CONNECTIONS;
        case 'list_folders': case 'list_queries': case 'list_saved_migrations': case 'list_library': case 'list_history': return [];
        case 'list_database_objects': return { objects: [{ kind: 'table', schema: 'public', name: 'clientes' }, { kind: 'table', schema: 'public', name: 'pedidos' }], schemas: null };
        case 'list_settings': case 'get_settings': return { 'telemetry.noticeSeen': true, 'telemetry.consent': false, 'import.suggested': true, 'support.next': '2099-01-01T00:00:00Z' };
        case 'set_setting': return null;
        case 'get_cached': return null;
        case 'execute_query': {
          await wait(300);
          return {
            results: [{ columns: [{ name: 'id', type_name: 'int4' }, { name: 'nombre', type_name: 'text' }], rows: [[1, 'Ana'], [2, 'Bruno']], total_rows: 2, truncated: false, rows_affected: null }],
            messages: [], error: null, elapsed_ms: 12, plans: [], log: [], errors: [], transaction: null, database: null,
          };
        }
        default: throw { kind: 'preview', message: `"${cmd}" no está disponible en la vista previa` };
      }
    },
  };
}
