import { invoke } from '@tauri-apps/api/core';

// AI assistant (src-tauri/src/commands/ai.rs, docs/asistente-ia.md).

export type AiProviderKind = 'embedded' | 'ollama' | 'claude_code' | 'codex' | 'lm_studio';

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
  action: 'start_ollama' | 'pull_model' | 'download_model' | null;
  path: string | null;
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
  include_schema: boolean;
}

export const aiApi = {
  detect: () => invoke<AiDetect>('ai_detect'),
  chat: (chatId: string, provider: AiProviderKind, model: string | null, messages: ChatMessage[], context: ChatContext) =>
    invoke<{ text: string; context_summary: string }>('ai_chat', { args: { chat_id: chatId, provider, model, messages, context } }),
  cancel: (id: string) => invoke<void>('ai_cancel', { args: { id } }),
  downloadModel: (id: string) => invoke<void>('ai_download_model', { args: { id } }),
  deleteModel: (id: string) => invoke<void>('ai_delete_model', { args: { id } }),
  startOllama: () => invoke<void>('ai_start_ollama'),
  pullOllama: (model: string) => invoke<void>('ai_pull_ollama', { args: { id: model } }),
};
