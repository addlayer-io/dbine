// view=tabs: the editor tab strip with tabs of three connections mixed, to
// see the connection groups (&collapse=<connection id> starts one collapsed).

import { useConnectionsStore } from '../stores/connections';
import { sampleDrivers } from './monitor-samples';

export function installTabsPreview() {
  const conns = useConnectionsStore();
  conns.drivers = [
    sampleDrivers[0],
    { ...sampleDrivers[0], id: 'redis', name: 'Redis' },
    { ...sampleDrivers[0], id: 'sqlserver', name: 'SQL Server' },
  ];
  const conn = (id: string, name: string, driver: string, color: string | null) => ({
    id, name, color, folder_id: null, tags: [], save_password: true, updated_at: '',
    config: { driver, host: 'h', port: 0, database: '', username: null, password: null, encrypt: false, trust_server_certificate: false, read_only: false, options: {} },
  });
  conns.list = [conn('pg', 'Postgres · analytics', 'postgres', null), conn('rd', 'Redis · caché', 'redis', null), conn('ms', 'SQL Server · ventas-prod', 'sqlserver', '#d4443b')];
  const tab = (id: string, connectionId: string, name: string, kind: 'object' | 'monitor' = 'object') => kind === 'monitor'
    ? { id, kind, connectionId, database: '', preview: false }
    : { id, kind, connectionId, database: 'db', object: { kind: 'table', schema: null, name }, view: 'data', preview: false };
  // Mixed, as they would be opened one after another.
  const tabs = [
    tab('1', 'ms', 'facturas'), tab('2', 'pg', 'eventos'), tab('3', 'ms', 'clientes'), tab('4', 'rd', 'user:1:profile'),
    tab('5', 'pg', 'sesiones'), tab('6', 'ms', 'Monitor', 'monitor'), tab('7', 'rd', 'config'),
  ];
  const collapse = new URLSearchParams(location.search).get('collapse');
  try {
    localStorage.setItem('dbine.tabs', JSON.stringify({ tabs, activeId: '5', collapsed: collapse ? [collapse] : [] }));
  } catch { /* preview */ }
  (window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
    transformCallback: () => 0,
    invoke: async () => { throw { kind: 'preview', message: 'vista previa' }; },
  };
}
