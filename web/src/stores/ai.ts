import { acceptHMRUpdate, defineStore } from 'pinia';
import { listen } from '@tauri-apps/api/event';
import { errorKind, errorMessage } from '../api/client';
import { aiApi, type AiDetect, type AiProvider, type AiProviderKind, type ChatMessage } from '../api/ai';
import { t } from '../i18n';
import { tb } from '../i18n/backend';
import { readJson, writeJson } from './storage';
import { useSettingsStore } from './settings';
import { useTabsStore } from './tabs';

// The AI assistant's state: detected providers, the chosen one, and the
// conversation. The assistant never runs anything: its code goes into the
// active query tab (appended, or replacing it when it corrects the editor)
// and the user runs it.

/** What a query tab lets the assistant see and do. */
export interface EditorBridge {
  text: () => string;
  selection: () => string;
  lastError: () => string | null;
  append: (code: string) => void;
  replace: (code: string) => void;
  /** Rename the tab's saved query (the tab strip's inline rename). */
  rename?: (name: string) => Promise<void>;
}
const bridges = new Map<string, EditorBridge>();
export function editorBridge(tabId: string): EditorBridge | null {
  return bridges.get(tabId) ?? null;
}
export function registerEditor(tabId: string, b: EditorBridge) {
  bridges.set(tabId, b);
  return () => { if (bridges.get(tabId) === b) bridges.delete(tabId); };
}

export interface UiMessage extends ChatMessage {
  id: string;
  thinking?: string;
  /** What went as context with this question. */
  context?: string;
  error?: string;
  pending?: boolean;
  provider?: string;
}

const HISTORY_KEY = 'dbine.ai.conversation';
let seq = 0;
const newId = () => `ai-${Date.now().toString(36)}-${++seq}`;

export const useAiStore = defineStore('ai', {
  state: () => ({
    detect: null as AiDetect | null,
    detecting: false,
    messages: readJson<UiMessage[]>(HISTORY_KEY, []).filter((m) => !m.pending),
    running: null as string | null,
    phase: null as string | null,
    /** Detail of the phase (the engine download's percentage). */
    phaseNote: null as string | null,
    /** Downloads in progress: id → {done, total, status}. */
    downloads: {} as Record<string, { done: number; total: number; status: string }>,
    listening: false,
  }),
  getters: {
    providers: (s): AiProvider[] => s.detect?.providers ?? [],
    usable(): AiProvider[] { return this.providers.filter((p) => p.available); },
    provider(): AiProvider | null {
      const want = useSettingsStore().get<AiProviderKind | null>('ai.provider', null);
      return this.usable.find((p) => p.kind === want) ?? this.usable[0] ?? null;
    },
    model(): string | null {
      const p = this.provider;
      if (!p) return null;
      const want = useSettingsStore().get<string | null>(`ai.model.${p.kind}`, null);
      return p.models.find((m) => m.id === want)?.id ?? p.models[0]?.id ?? null;
    },
    includeSchema: () => useSettingsStore().get<boolean>('ai.includeSchema', true),
    activeBridge(): EditorBridge | null {
      const id = useTabsStore().activeId;
      return id ? bridges.get(id) ?? null : null;
    },
  },
  actions: {
    async init() {
      if (!this.listening) {
        this.listening = true;
        try {
          await listen<{ chat_id: string; delta: { kind: 'text' | 'thinking'; text: string } }>('ai-delta', (e) => {
            const m = this.messages.find((x) => x.id === e.payload.chat_id);
            if (!m) return;
            if (e.payload.delta.kind === 'thinking') m.thinking = (m.thinking ?? '') + e.payload.delta.text;
            else m.content += e.payload.delta.text;
            this.phase = 'writing';
          });
          await listen<{ chat_id: string; phase: string; note: string | null }>('ai-status', (e) => {
            if (e.payload.chat_id === this.running) {
              this.phase = e.payload.phase;
              this.phaseNote = e.payload.note === null ? null : tb(e.payload.note);
            }
          });
          await listen<{ id: string; status: string; done: number; total: number }>('ai-download', (e) => {
            this.downloads[e.payload.id] = { done: e.payload.done, total: e.payload.total, status: tb(e.payload.status) };
          });
        } catch { /* outside Tauri */ }
      }
      if (!this.detect) await this.refresh();
    },
    async refresh() {
      this.detecting = true;
      try { this.detect = await aiApi.detect(); } catch { /* outside Tauri */ } finally { this.detecting = false; }
    },
    choose(kind: AiProviderKind, model?: string) {
      const s = useSettingsStore();
      s.set('ai.provider', kind);
      if (model !== undefined) s.set(`ai.model.${kind}`, model);
    },
    setIncludeSchema(v: boolean) { useSettingsStore().set('ai.includeSchema', v); },
    persist() {
      writeJson(HISTORY_KEY, this.messages.filter((m) => !m.pending).slice(-60));
    },
    clear() {
      if (this.running) return;
      this.messages = [];
      this.persist();
    },

    async send(text: string) {
      const p = this.provider;
      if (!p || !text.trim() || this.running) return;
      const tabs = useTabsStore();
      const tab = tabs.active;
      const b = this.activeBridge;
      const user: UiMessage = { id: newId(), role: 'user', content: text.trim() };
      const answer: UiMessage = { id: newId(), role: 'assistant', content: '', pending: true, provider: p.label };
      this.messages.push(user, answer);
      const history: ChatMessage[] = this.messages
        .filter((m) => m !== answer && !m.error && (m.role === 'user' || m.content))
        .slice(-12)
        .map((m) => ({ role: m.role, content: m.content }));
      this.running = answer.id;
      this.phase = 'thinking';
      const object = tab?.kind === 'object' ? [tab.object.schema, tab.object.name].filter(Boolean).join('.') : null;
      try {
        const r = await aiApi.chat(answer.id, p.kind, this.model, history, {
          connection_id: tab?.connectionId ?? null,
          database: tab?.database ?? null,
          object,
          editor_sql: b?.text() ?? null,
          selection: b?.selection() || null,
          last_error: b?.lastError() ?? null,
          include_schema: this.includeSchema,
        });
        const m = this.messages.find((x) => x.id === answer.id)!;
        if (!m.content) m.content = r.text;
        user.context = tb(r.context_summary);
      } catch (e) {
        const m = this.messages.find((x) => x.id === answer.id)!;
        m.error = errorKind(e) === 'cancelled' ? t('core:ai.stopped') : errorMessage(e);
      } finally {
        const m = this.messages.find((x) => x.id === answer.id);
        if (m) m.pending = false;
        this.running = null;
        this.phase = null;
        this.phaseNote = null;
        this.persist();
      }
    },
    stop() {
      if (this.running) aiApi.cancel(this.running);
    },

    async downloadModel(id: string) {
      this.downloads[id] = { done: 0, total: 0, status: t('core:ai.downloading') };
      try {
        await aiApi.downloadModel(id);
        await this.refresh();
        this.choose('embedded', id);
      } catch (e) {
        if (errorKind(e) !== 'cancelled') throw e;
      } finally {
        delete this.downloads[id];
      }
    },
    cancelDownload(id: string) { aiApi.cancel(id); },
    async pullOllama(model: string) {
      const key = `ollama:${model}`;
      this.downloads[key] = { done: 0, total: 0, status: t('core:ai.starting') };
      try {
        await aiApi.pullOllama(model);
        await this.refresh();
        this.choose('ollama', model);
      } finally {
        delete this.downloads[key];
      }
    },
  },
});

// Development: swap the store's code in place when it changes (no reload).
if (import.meta.hot) import.meta.hot.accept(acceptHMRUpdate(useAiStore, import.meta.hot));
