import { invoke } from '@tauri-apps/api/core';

// AI assistant (src-tauri/src/commands/ai.rs, docs/ai-assistant.md).

export type AiProviderKind = 'embedded' | 'ollama' | 'claude_code' | 'codex' | 'lm_studio'
  | 'openai' | 'anthropic' | 'azure_openai' | 'gemini' | 'xai' | 'openai_compatible';

/** The providers used with the user's own API key (src-tauri commands/ai.rs, dbine_ai::api). */
export const API_PROVIDERS: AiProviderKind[] = ['openai', 'anthropic', 'azure_openai', 'gemini', 'xai', 'openai_compatible'];
export const isApiProvider = (k: AiProviderKind) => API_PROVIDERS.includes(k);

/** Who gets the question and bills the user; null: the user's own server. */
export function vendorOf(k: AiProviderKind): string | null {
  const v: Partial<Record<AiProviderKind, string>> = {
    claude_code: 'Anthropic', codex: 'OpenAI', openai: 'OpenAI', anthropic: 'Anthropic',
    azure_openai: 'Microsoft Azure', gemini: 'Google', xai: 'xAI',
  };
  return v[k] ?? null;
}

/** What a provider's API-key form keeps in the settings (never the key, which goes to the vault). */
export interface ApiSetting { url?: string; model?: string }
export const apiSettingKey = (k: AiProviderKind) => `ai.api.${k}`;

export interface AiModel { id: string; detail: string | null }

export interface AiProvider {
  kind: AiProviderKind;
  label: string;
  installed: boolean;
  available: boolean;
  /** Runs entirely on this machine. */
  local: boolean;
  status: string;
  models: AiModel[];
  action: 'start_ollama' | 'pull_model' | 'download_model' | 'configure_key' | null;
  path: string | null;
  /** API-key providers: a key is saved on this machine. */
  key_saved: boolean;
  /** API-key providers: the base URL in use. */
  base_url: string | null;
  /** API-key providers: the model typed by hand. */
  model: string | null;
}

export interface CatalogModel {
  id: string; label: string; detail: string; file: string; url: string; size: number; sha256: string; min_ram_gb: number;
  installed: boolean; downloading: boolean; recommended: boolean;
}

export interface AiDetect {
  providers: AiProvider[];
  catalog: CatalogModel[];
  embedded_enabled: boolean;
  ollama_recommended_model: string;
}

export interface ChatMessage { role: 'user' | 'assistant'; content: string }

export interface ChatContext {
  connection_id: string | null;
  database: string | null;
  object: string | null;
  editor_sql: string | null;
  selection: string | null;
  last_error: string | null;
  /** "Aprobar lecturas en esta conversación": a local model's reads of rows run without asking. */
  approve_reads?: boolean;
  /** Contexts turned off for this conversation (`global`: the Settings text). */
  disabled_contexts?: string[];
}

/** One part of what went as context with a question (the UI words it). */
export type ChatContextItem =
  | { kind: 'engine' | 'database'; name: string }
  | { kind: 'objects'; count: number }
  | { kind: 'tables'; shown: number; total: number }
  | { kind: 'editor' | 'selection' | 'last_error' }
  | { kind: 'contexts'; count: number }
  | { kind: 'contexts_left_out'; names: string[] };

export const aiApi = {
  detect: () => invoke<AiDetect>('ai_detect'),
  /** `uiLanguage`: the instructions to the model go in it. */
  chat: (chatId: string, provider: AiProviderKind, model: string | null, messages: ChatMessage[], context: ChatContext, uiLanguage: string) =>
    invoke<{ text: string; context: ChatContextItem[] }>('ai_chat', { args: { chat_id: chatId, provider, model, messages, context, ui_language: uiLanguage } }),
  /** "Analizar con IA" on a failed query: one answer without tools, streamed as `ai-delta` events under `chatId`. */
  fixError: (chatId: string, provider: AiProviderKind, model: string | null, messages: ChatMessage[], context: ChatContext, uiLanguage: string) =>
    invoke<{ text: string; context: ChatContextItem[] }>('ai_fix_error', { args: { chat_id: chatId, provider, model, messages, context, ui_language: uiLanguage, with_structure: true } }),
  cancel: (id: string) => invoke<void>('ai_cancel', { args: { id } }),
  /** The user's answer to a read the assistant asked for. */
  approve: (requestId: string, decision: 'approve' | 'approve_all' | 'reject') =>
    invoke<void>('ai_approve', { args: { request_id: requestId, decision } }),
  downloadModel: (id: string) => invoke<void>('ai_download_model', { args: { id } }),
  deleteModel: (id: string) => invoke<void>('ai_delete_model', { args: { id } }),
  startOllama: () => invoke<void>('ai_start_ollama'),
  pullOllama: (model: string) => invoke<void>('ai_pull_ollama', { args: { id: model } }),
  /** The key goes to the vault (never with the cloud backup); `key` null or empty keeps the saved one. */
  apiSave: (provider: AiProviderKind, key: string | null, url: string | null, model: string | null) =>
    invoke<void>('ai_api_save', { args: { provider, key, url, model } }),
  apiRemove: (provider: AiProviderKind) => invoke<void>('ai_api_remove', { args: { provider } }),
  /** "Probar": what's in the form, else what's saved. */
  apiTest: (provider: AiProviderKind, key: string | null, url: string | null, model: string | null) =>
    invoke<{ message: string; models: string[] }>('ai_api_test', { args: { provider, key, url, model } }),
};
