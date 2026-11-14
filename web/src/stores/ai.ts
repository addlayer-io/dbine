import { acceptHMRUpdate, defineStore } from 'pinia';
import { listen } from '@tauri-apps/api/event';
import { errorKind, errorMessage } from '../api/client';
import { aiApi, type AiDetect, type AiProvider, type AiProviderKind, type ChatContextItem, type ChatMessage } from '../api/ai';
import { language, t } from '../i18n';
import { tb } from '../i18n/backend';
import { sealedApi } from '../api/sealed';
import { readJson } from './storage';
import { useSettingsStore } from './settings';
import { ownsSavedState, useTabsStore } from './tabs';
import { trackFeature } from '../composables/telemetry';

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
  /** A notebook: insert Markdown as a text cell after the active one. */
  appendText?: (md: string) => void;
  /** Where the editor's text runs, when it isn't the tab's connection (a
   *  notebook's active cell); null: the tab's. */
  target?: () => { connectionId: string; database: string } | null;
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
  /** What a local model read from the connection to answer. */
  tools?: { label: string; ok: boolean }[];
  /** Active contexts that didn't fit in the prompt. */
  contextsLeftOut?: string[];
  /** A read of rows waiting for the user's Aprobar / Rechazar. */
  /** A read waiting for the user: `data` (rows, `sql` is its query) or `structure` (a remote model's catalog read, `sql` is the tool and its arguments). */
  approval?: { requestId: string; label: string; connection: string; database: string; sql: string; kind: 'data' | 'structure' };
}

let seq = 0;
const newId = () => `ai-${Date.now().toString(36)}-${++seq}`;

// The conversation and the past ones ("Historial") quote database content,
// so they're kept in DBine's encrypted store (sealedApi), never in
// localStorage. "Nueva conversación" archives the current one instead of
// losing it. Every window reads the list fresh before changing it, so two
// windows don't overwrite each other's archive. Without a vault they stay in
// memory only, and the history panel says so.
const MAX_CHATS = 50;
// Where DBine 1.5 and before kept them in the clear: moved once, then removed.
const LEGACY_CONVERSATION_KEY = 'dbine.ai.conversation';
const LEGACY_CHAT_ID_KEY = 'dbine.ai.conversationId';
const LEGACY_ARCHIVE_KEY = 'dbine.ai.history';

interface SavedConversation {
  chatId: string;
  messages: UiMessage[];
}

export interface SavedChat {
  id: string;
  /** The first question, cut. */
  title: string;
  /** Last change (ms). */
  updated: number;
  messages: UiMessage[];
}

function parse<T>(v: string | null, def: T): T {
  if (v == null) return def;
  try { return JSON.parse(v) as T; } catch { return def; }
}

/** The store's writes, in order: a read waits for the ones before it. */
let writes: Promise<unknown> = Promise.resolve();
/** The archive as this window last wrote it: what it shows without a vault. */
let memArchive: SavedChat[] = [];
/** Without a vault, the chat id of the conversation DBine 1.5 left in
 *  plaintext, which moveLegacy keeps; `null` when there is none. */
let legacyChatId: string | null = null;

/** Without a vault, the plaintext copies DBine 1.5 left stay where they were
 *  (moveLegacy). So a chat the user deletes has to leave them too, or it stays
 *  readable on disk and comes back on the next start. `ids`: the chats
 *  deleted; `null`: the whole history, except `current`, the conversation
 *  open now, which isn't in it. With a vault the keys are already gone. */
function forgetLegacy(ids: Set<string> | null, current?: string) {
  try {
    const legacy = ids ? readJson<SavedChat[] | null>(LEGACY_ARCHIVE_KEY, null) : null;
    const rest = legacy ? legacy.filter((c) => !ids?.has(c.id)) : [];
    if (rest.length) {
      if (rest.length !== legacy?.length) localStorage.setItem(LEGACY_ARCHIVE_KEY, JSON.stringify(rest));
    } else localStorage.removeItem(LEGACY_ARCHIVE_KEY);
    if (legacyChatId && (ids ? ids.has(legacyChatId) : legacyChatId !== current)) {
      localStorage.removeItem(LEGACY_CONVERSATION_KEY);
      localStorage.removeItem(LEGACY_CHAT_ID_KEY);
      legacyChatId = null;
    }
  } catch { /* storage blocked */ }
}
/** The saved conversation's load (`ready`). */
let loading: Promise<void> | null = null;

/** Run `write` after the writes before it. `true`: kept; `false`: memory only. */
function enqueue(write: () => Promise<boolean>): Promise<boolean> {
  const p = writes.then(write, write);
  writes = p.catch(() => undefined);
  return p;
}

async function sealArchive(list: SavedChat[]): Promise<boolean> {
  // Too big for the store: drop the oldest chats until it fits.
  let keep = list;
  while (true) {
    try {
      return await sealedApi.set('ai_history', JSON.stringify(keep));
    } catch {
      if (!keep.length) return false;
      keep = keep.slice(0, Math.floor(keep.length / 2));
    }
  }
}

async function readArchive(): Promise<SavedChat[]> {
  await writes;
  try {
    const r = await sealedApi.get('ai_history');
    return r.available ? parse<SavedChat[]>(r.value, []) : memArchive;
  } catch {
    return memArchive;
  }
}

/** `false`: there's no vault, it's kept in this window's memory only. */
function writeArchive(list: SavedChat[]): Promise<boolean> {
  memArchive = list.slice(0, MAX_CHATS);
  const keep = memArchive;
  return enqueue(() => sealArchive(keep));
}

/** The "what was sent" line ("PostgreSQL · ventas · 42 tables · editor"), in the UI language. */
function contextLine(items: ChatContextItem[]): string {
  return items
    .filter((c) => c.kind !== 'contexts_left_out')
    .map((c) => {
      switch (c.kind) {
        case 'contexts': return t('ai:context.contexts', { count: c.count });
        case 'engine':
        case 'database': return c.name;
        case 'objects': return t('ai:context.objects', { count: c.count });
        case 'tables': return c.shown === c.total ? t('ai:context.tables', { count: c.total }) : t('ai:context.tablesOf', { shown: c.shown, count: c.total });
        case 'editor': return t('ai:context.editor');
        case 'selection': return t('ai:context.selection');
        case 'last_error': return t('ai:context.lastError');
      }
    })
    .join(' · ');
}

function titleOf(messages: UiMessage[]): string {
  const first = messages.find((m) => m.role === 'user')?.content.trim().split('\n')[0] ?? '';
  return first.length > 80 ? `${first.slice(0, 79)}…` : first;
}

export const useAiStore = defineStore('ai', {
  state: () => ({
    detect: null as AiDetect | null,
    detecting: false,
    // Only the primary window resumes the saved conversation (`loadSaved`,
    // from the encrypted store); others start empty.
    messages: [] as UiMessage[],
    /** The current conversation's id in the archive (restoring one keeps its id). */
    chatId: newId(),
    /** The archive as last read (refreshed when the "Historial" panel opens). */
    history: [] as SavedChat[],
    /** `false`: this system has no vault, so conversations stay in memory only. */
    kept: true,
    /** "Aprobar lecturas en esta conversación", for `approveScope` (connection
     *  and database): another conversation or tab target asks again. */
    approveReads: false,
    approveScope: null as string | null,
    /** Contexts turned off for this conversation (`global`: the Settings text). */
    disabledContexts: [] as string[],
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
    activeBridge(): EditorBridge | null {
      const id = useTabsStore().activeId;
      return id ? bridges.get(id) ?? null : null;
    },
    /** Where a question would go: the editor's own target (a notebook's
     *  active cell), else the tab's. */
    target(): { connectionId: string; database: string } | null {
      const tab = useTabsStore().active;
      return this.activeBridge?.target?.() ?? (tab ? { connectionId: tab.connectionId, database: tab.database } : null);
    },
  },
  actions: {
    async init() {
      if (!this.listening) {
        this.listening = true;
        try {
          await listen<{ chat_id: string; delta: { kind: 'text' | 'thinking'; text: string } }>('ai-delta', (e) => {
            const m = this.messages.find((x) => x.id === e.payload.chat_id);
            if (!m?.pending) return;
            if (e.payload.delta.kind === 'thinking') m.thinking = (m.thinking ?? '') + e.payload.delta.text;
            else m.content += e.payload.delta.text;
            this.phase = 'writing';
          });
          await listen<{ chat_id: string; request_id: string; label: string; connection: string; database: string; sql: string; kind?: string }>('ai-approval', (e) => {
            const m = this.messages.find((x) => x.id === e.payload.chat_id);
            if (m?.pending) m.approval = { requestId: e.payload.request_id, label: e.payload.label, connection: e.payload.connection, database: e.payload.database, sql: e.payload.sql, kind: e.payload.kind === 'structure' ? 'structure' : 'data' };
          });
          await listen<{ chat_id: string; label: string; ok: boolean }>('ai-tool', (e) => {
            const m = this.messages.find((x) => x.id === e.payload.chat_id);
            if (m?.pending) (m.tools ??= []).push({ label: e.payload.label, ok: e.payload.ok });
          });
          await listen<{ chat_id: string; phase: string; note: string | null }>('ai-status', (e) => {
            if (e.payload.chat_id === this.running) {
              // The backend asks again after a refusal: drop what it showed.
              if (e.payload.phase === 'retry' || e.payload.phase === 'tool') {
                const m = this.messages.find((x) => x.id === e.payload.chat_id);
                if (m) { m.content = ''; m.thinking = undefined; }
              }
              this.phase = e.payload.phase;
              this.phaseNote = e.payload.note === null ? null : tb(e.payload.note);
            }
          });
          await listen<{ id: string; status: string; done: number; total: number }>('ai-download', (e) => {
            this.downloads[e.payload.id] = { done: e.payload.done, total: e.payload.total, status: tb(e.payload.status) };
          });
        } catch { /* outside Tauri */ }
      }
      await this.ready();
      if (!this.detect) await this.refresh();
    },
    /** The saved conversation, loaded once: what changes the conversation
     *  (a question, "Nueva conversación", reopening one) waits for it, so it
     *  never overwrites the saved one before it's read. */
    ready(): Promise<void> {
      return (loading ??= this.loadSaved());
    },
    /** At start: move what DBine 1.5 and before left in localStorage into the
     *  encrypted store, then resume the saved conversation (primary window). */
    async loadSaved() {
      await this.moveLegacy();
      if (!ownsSavedState()) return;
      await writes;
      let saved: SavedConversation | null = null;
      try {
        const r = await sealedApi.get('ai_conversation');
        this.kept = r.available;
        saved = parse<SavedConversation | null>(r.value, null);
      } catch { return; }
      if (!saved || this.messages.length) return;
      this.messages = (saved.messages ?? []).filter((m) => !m.pending);
      if (saved.chatId) this.chatId = saved.chatId;
    },
    /** The plaintext copies are removed only once the store has kept them;
     *  without a vault they stay as they were (nothing new is written there). */
    async moveLegacy() {
      const legacy = readJson<SavedChat[] | null>(LEGACY_ARCHIVE_KEY, null);
      if (legacy) {
        const current = await readArchive();
        const ids = new Set(current.map((c) => c.id));
        const merged = [...current, ...legacy.filter((c) => !ids.has(c.id))].sort((a, b) => b.updated - a.updated);
        const kept = await writeArchive(merged);
        this.kept = kept;
        if (kept) localStorage.removeItem(LEGACY_ARCHIVE_KEY);
      }
      if (!ownsSavedState()) return;
      const messages = readJson<UiMessage[] | null>(LEGACY_CONVERSATION_KEY, null);
      if (!messages) return;
      const chatId = readJson<string | null>(LEGACY_CHAT_ID_KEY, null) ?? this.chatId;
      const conversation: SavedConversation = { chatId, messages: messages.filter((m) => !m.pending).slice(-60) };
      const kept = await enqueue(() => sealedApi.set('ai_conversation', JSON.stringify(conversation)).catch(() => false));
      this.kept = kept;
      if (kept) {
        localStorage.removeItem(LEGACY_CONVERSATION_KEY);
        localStorage.removeItem(LEGACY_CHAT_ID_KEY);
      } else {
        // No vault: the old copy stays until that chat is deleted.
        legacyChatId = chatId;
        if (!this.messages.length) {
          // Shown from the old copy, kept in memory from here on.
          this.messages = conversation.messages;
          this.chatId = chatId;
        }
      }
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
    /** Answer a read the assistant asked for. */
    async decide(m: UiMessage, decision: 'approve' | 'approve_all' | 'reject') {
      const a = m.approval;
      if (!a) return;
      m.approval = undefined;
      if (decision === 'approve_all') this.approveReads = true;
      try { await aiApi.approve(a.requestId, decision); } catch { /* the answer already ended */ }
    },
    persist() {
      if (!ownsSavedState()) return;
      const conversation: SavedConversation = { chatId: this.chatId, messages: this.messages.filter((m) => !m.pending).slice(-60) };
      void enqueue(() => sealedApi.set('ai_conversation', JSON.stringify(conversation)).catch(() => false))
        .then((kept) => { this.kept = kept; });
    },
    /** The current conversation as an archive entry; `null` when it's empty. */
    archiveEntry(): SavedChat | null {
      const messages = this.messages.filter((m) => !m.pending).slice(-60);
      return messages.length ? { id: this.chatId, title: titleOf(messages), updated: Date.now(), messages } : null;
    },
    /** Change the archive as it is now (another window may have changed it). */
    async changeArchive(change: (list: SavedChat[]) => SavedChat[]) {
      this.kept = await writeArchive(change(await readArchive()));
      await this.loadHistory();
    },
    /** "Nueva conversación": the current one goes to the history. */
    async clear() {
      await this.ready();
      // Always available: a running answer is stopped first.
      this.stop();
      const entry = this.archiveEntry();
      this.messages = [];
      this.chatId = newId();
      this.approveReads = false;
      this.disabledContexts = [];
      this.persist();
      void this.changeArchive((list) => (entry ? [entry, ...list.filter((c) => c.id !== entry.id)] : list));
    },
    async loadHistory() {
      this.history = await readArchive();
    },
    /** Reopen a past conversation; the current one goes to the history. */
    async restore(id: string) {
      await this.ready();
      if (id === this.chatId) return;
      this.stop();
      const chat = (await readArchive()).find((c) => c.id === id);
      if (!chat) return;
      const entry = this.archiveEntry();
      this.messages = chat.messages;
      this.chatId = chat.id;
      this.approveReads = false;
      this.disabledContexts = [];
      this.persist();
      await this.changeArchive((list) => {
        const rest = list.filter((c) => c.id !== id && c.id !== entry?.id);
        return entry ? [entry, ...rest] : rest;
      });
    },
    async deleteChat(id: string) {
      forgetLegacy(new Set([id]));
      await this.changeArchive((list) => list.filter((c) => c.id !== id));
    },
    async clearHistory() {
      forgetLegacy(null, this.chatId);
      memArchive = [];
      await enqueue(() => sealedApi.delete('ai_history').then(() => true, () => false));
      await this.loadHistory();
    },

    async send(text: string) {
      await this.ready();
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
      const where = this.target;
      // "Aprobar lecturas en esta conversación" holds for one connection and database.
      const scope = `${where?.connectionId ?? ''}|${where?.database ?? ''}`;
      if (this.approveScope !== scope) { this.approveReads = false; this.approveScope = scope; }
      try {
        trackFeature('ai_message', p.kind);
        const r = await aiApi.chat(answer.id, p.kind, this.model, history, {
          connection_id: where?.connectionId ?? null,
          database: where?.database ?? null,
          object,
          editor_sql: b?.text() ?? null,
          selection: b?.selection() || null,
          last_error: b?.lastError() ?? null,
          approve_reads: this.approveReads,
          disabled_contexts: this.disabledContexts,
        }, language.value);
        // Gone (a new conversation) or stopped: what comes back is dropped.
        const m = this.messages.find((x) => x.id === answer.id);
        if (m?.pending && !m.content) m.content = r.text;
        if (m?.pending) {
          user.context = contextLine(r.context);
          user.contextsLeftOut = r.context.find((c) => c.kind === 'contexts_left_out')?.names;
        }
      } catch (e) {
        const m = this.messages.find((x) => x.id === answer.id);
        if (m?.pending) m.error = errorKind(e) === 'cancelled' ? t('core:ai.stopped') : errorMessage(e);
      } finally {
        const m = this.messages.find((x) => x.id === answer.id);
        if (m) { m.pending = false; m.approval = undefined; }
        // A newer turn may be running already (stopped, then asked again).
        if (this.running === answer.id) {
          this.running = null;
          this.phase = null;
          this.phaseNote = null;
        }
        this.persist();
      }
    },
    toggleContextForChat(id: string) {
      const off = this.disabledContexts;
      this.disabledContexts = off.includes(id) ? off.filter((x) => x !== id) : [...off, id];
    },
    /** Stop the running answer now: the UI doesn't wait for the backend
     *  (it stops streaming, reading or waiting for an approval on its own). */
    stop() {
      const id = this.running;
      if (!id) return;
      aiApi.cancel(id).catch(() => { /* already over */ });
      const m = this.messages.find((x) => x.id === id);
      if (m?.pending) {
        m.pending = false;
        m.approval = undefined;
        if (!m.content) m.error = t('core:ai.stopped');
      }
      this.running = null;
      this.phase = null;
      this.phaseNote = null;
      this.persist();
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
