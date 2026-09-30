// Development only: the AI assistant sidebar with a mocked backend.
// http://localhost:<port>/ai-preview.html?state=setup|chat|empty
import './lang';
import I18NextVue from 'i18next-vue';
import i18next from '../i18n';
import { loadBackendCatalog } from '../i18n/backend';
import { createApp, h } from 'vue';
import { createPinia } from 'pinia';
import ElementPlus from 'element-plus';
import * as Icons from '@element-plus/icons-vue';
import 'element-plus/dist/index.css';
import 'element-plus/theme-chalk/dark/css-vars.css';
import '../styles/global.scss';
import AiSidebar from '../components/AiSidebar.vue';
import { useAiStore } from '../stores/ai';

document.documentElement.classList.add('dark');
const state = new URLSearchParams(location.search).get('state') ?? 'chat';

const callbacks = new Map<number, (e: unknown) => void>();
const listeners = new Map<string, number[]>();
let n = 0;
function emit(event: string, payload: unknown) {
  for (const id of listeners.get(event) ?? []) callbacks.get(id)?.({ event, id: 0, payload });
}
const ANSWER = 'Esta query devuelve los clientes con más de 5 pedidos en 2025:\n\n```sql\nSELECT c.id, c.nombre, COUNT(*) AS pedidos\nFROM dbo.clientes c\nJOIN dbo.pedidos p ON p.cliente_id = c.id\nWHERE p.fecha >= \'2025-01-01\' AND p.fecha < \'2026-01-01\'\nGROUP BY c.id, c.nombre\nHAVING COUNT(*) > 5\nORDER BY pedidos DESC;\n```\n\nUsé un rango de fechas en lugar de `YEAR(p.fecha)` para que **pueda usar el índice** sobre `fecha`.';
const providers = state === 'setup'
  ? [
    { kind: 'embedded', label: 'Integrado en DBine', installed: true, available: false, local: true, status: 'sin instalar nada: descargá un modelo', models: [], action: 'download_model', path: null },
    { kind: 'ollama', label: 'Ollama', installed: true, available: false, local: true, status: 'instalado, pero no está abierto', models: [], action: 'start_ollama', path: '/usr/local/bin/ollama' },
    { kind: 'claude_code', label: 'Claude Code', installed: false, available: false, local: false, status: 'no instalado', models: [], action: null, path: null },
    { kind: 'codex', label: 'Codex', installed: false, available: false, local: false, status: 'no instalado', models: [], action: null, path: null },
    { kind: 'lm_studio', label: 'LM Studio', installed: false, available: false, local: true, status: 'no instalado', models: [], action: null, path: null },
  ]
  : [
    { kind: 'embedded', label: 'Integrado en DBine', installed: true, available: true, local: true, status: '1 modelo(s)', models: [{ id: 'qwen2.5-coder-3b', detail: 'rápido y liviano · 2,1 GB' }], action: null, path: null },
    { kind: 'claude_code', label: 'Claude Code', installed: true, available: true, local: false, status: 'instalado · usa tu cuenta de Claude', models: [{ id: 'sonnet', detail: 'Sonnet: equilibrado' }, { id: 'haiku', detail: 'Haiku: el más rápido' }], action: null, path: '/x/claude' },
  ];

(window as unknown as Record<string, unknown>).__TAURI_INTERNALS__ = {
  transformCallback: (cb: (e: unknown) => void) => { const id = ++n; callbacks.set(id, cb); return id; },
  unregisterCallback: (id: number) => callbacks.delete(id),
  invoke: async (cmd: string, args: Record<string, any>) => {
    switch (cmd) {
      case 'plugin:event|listen': {
        const l = listeners.get(args.event) ?? [];
        l.push(args.handler);
        listeners.set(args.event, l);
        return args.handler;
      }
      case 'list_settings': return {};
      case 'set_setting': return null;
      case 'ai_detect': return {
        providers,
        catalog: [
          { id: 'qwen2.5-coder-3b', label: 'Qwen2.5-Coder 3B', detail: 'rápido y liviano · 2,1 GB', size: 2104932800, installed: state !== 'setup', downloading: false, recommended: false },
          { id: 'qwen2.5-coder-7b', label: 'Qwen2.5-Coder 7B', detail: 'mejores respuestas en SQL · 4,7 GB', size: 4683073536, installed: false, downloading: false, recommended: true },
        ],
        embedded_enabled: true,
        ollama_recommended_model: 'qwen2.5-coder:7b',
      };
      case 'ai_chat': {
        const id = args.args.chat_id;
        emit('ai-status', { chat_id: id, phase: 'schema' });
        await new Promise((r) => setTimeout(r, 300));
        for (const piece of ANSWER.match(/[\s\S]{1,12}/g)!) {
          emit('ai-delta', { chat_id: id, delta: { kind: 'text', text: piece } });
          await new Promise((r) => setTimeout(r, 15));
        }
        return { text: ANSWER, context_summary: 'SQL Server · ventas · 42 tablas · editor' };
      }
      default: return null;
    }
  },
};

const app = createApp({ render: () => h('div', { style: 'height: 100vh; width: 420px; margin-left: auto;' }, [h(AiSidebar)]) });
app.use(createPinia());
app.use(I18NextVue, { i18next });
loadBackendCatalog();
app.use(ElementPlus, { size: 'small' });
for (const [name, comp] of Object.entries(Icons)) app.component(`Ei${name}`, comp as never);
app.mount('#app');
if (state === 'chat') {
  localStorage.removeItem('dbine.ai.conversation');
  setTimeout(() => useAiStore().send('Clientes con más de 5 pedidos en 2025'), 600);
}
