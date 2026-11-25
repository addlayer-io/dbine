<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { aiApi } from '../api/ai';
import { errorKind, errorMessage } from '../api/client';
import { language } from '../i18n';
import SparklesIcon from './SparklesIcon.vue';
import { useAiStore } from '../stores/ai';
import { useUiStore } from '../stores/ui';
import { buildFixPrompt, destructive, fitEnds, isChange, parseFixAnswer } from '../composables/fixError';
import { fixProposals } from '../composables/fixProposal';

// "Analizar con IA", next to a query's error. It asks only when clicked: the
// SQL that failed, the engine, the error and the tab's structure go to the
// assistant's provider (never rows), the explanation streams in here and the
// corrected query goes to the editor as a proposal the user accepts or
// rejects. Nothing runs (docs/ai-assistant.md, "Fixing a failed query").

const props = defineProps<{
  /** `tabId:runId`: where the editor reports the proposal. */
  proposalKey: string;
  connectionId: string;
  database: string;
  /** The script that ran and the error it ended with. */
  sql: string;
  error: string;
  engine: string;
  /** Puts the corrected query in the editor as a proposal: `ok`, `same` (nothing to change) or `stale` (the text moved). */
  propose: (corrected: string) => 'ok' | 'same' | 'stale';
}>();

const ai = useAiStore();
const ui = useUiStore();

const phase = ref<'idle' | 'running' | 'done'>('idle');
const noProvider = ref(false);
const text = ref('');
const seconds = ref(0);
const failed = ref('');
const outcome = ref<'ok' | 'same' | 'stale' | 'nocode' | 'cancelled' | null>(null);
const warn = ref(false);
let chatId = '';
let timer: ReturnType<typeof setInterval> | null = null;
let unlisten: UnlistenFn | null = null;

const parsed = computed(() => parseFixAnswer(text.value));
const model = computed(() => ai.model ?? ai.provider?.label ?? '');
const remote = computed(() => (ai.provider && !ai.provider.local ? ai.provider.label : ''));
const status = computed(() => fixProposals[props.proposalKey]);

function stopClock() {
  if (timer) clearInterval(timer);
  timer = null;
}
onBeforeUnmount(() => {
  stopClock();
  unlisten?.();
  if (phase.value === 'running') aiApi.cancel(chatId).catch(() => { /* over */ });
});

async function analyze() {
  await ai.init();
  const p = ai.provider;
  if (!p) {
    noProvider.value = true;
    return;
  }
  noProvider.value = false;
  text.value = '';
  failed.value = '';
  outcome.value = null;
  warn.value = false;
  delete fixProposals[props.proposalKey];
  phase.value = 'running';
  seconds.value = 0;
  const id = crypto.randomUUID();
  chatId = id;
  const started = Date.now();
  timer = setInterval(() => { seconds.value = Math.floor((Date.now() - started) / 1000); }, 500);
  try {
    unlisten = await listen<{ chat_id: string; delta: { kind: 'text' | 'thinking'; text: string } }>('ai-delta', (e) => {
      if (e.payload.chat_id === id && e.payload.delta.kind === 'text') text.value += e.payload.delta.text;
    });
  } catch { /* outside Tauri */ }
  try {
    const r = await aiApi.fixError(id, p.kind, ai.model, [{ role: 'user', content: buildFixPrompt({ sql: props.sql, error: props.error, engine: props.engine }) }], {
      connection_id: props.connectionId,
      database: props.database || null,
      object: null,
      editor_sql: props.sql,
      selection: null,
      last_error: props.error,
    }, language.value);
    if (chatId !== id) return;
    // Providers that don't stream (Codex) answer whole.
    if (!text.value) text.value = r.text;
    finish();
  } catch (e) {
    if (chatId !== id) return;
    if (errorKind(e) === 'cancelled') outcome.value = 'cancelled';
    else failed.value = errorMessage(e);
    phase.value = 'done';
  } finally {
    if (chatId === id) { stopClock(); unlisten?.(); unlisten = null; }
  }
}

function finish() {
  phase.value = 'done';
  const a = parsed.value;
  if (!isChange(props.sql, a.code)) {
    outcome.value = a.code ? 'same' : 'nocode';
    return;
  }
  const corrected = fitEnds(props.sql, a.code);
  outcome.value = props.propose(corrected);
  warn.value = destructive(corrected);
}

function cancel() {
  if (phase.value !== 'running') return;
  const id = chatId;
  chatId = '';
  aiApi.cancel(id).catch(() => { /* over */ });
  stopClock();
  unlisten?.();
  unlisten = null;
  outcome.value = 'cancelled';
  phase.value = 'done';
}

function openSetup() {
  ui.aiOpen = true;
}
</script>

<template>
  <div class="afe">
    <button v-if="phase === 'idle' || (phase === 'done' && !text)" class="afe-btn" :title="$t('ai:fix.tip')" @click="analyze">
      <SparklesIcon class="afe-icon" />{{ phase === 'done' ? $t('ai:fix.again') : $t('ai:fix.button') }}
    </button>
    <div v-if="noProvider" class="afe-note" role="status">
      {{ $t('ai:fix.noProvider') }}
      <button class="afe-link" @click="openSetup">{{ $t('ai:fix.openSetup') }}</button>
    </div>
    <div v-if="phase === 'running'" class="afe-run" role="status">
      <el-icon class="is-loading"><ei-loading /></el-icon>
      <span>{{ $t('ai:fix.running', { model, seconds }) }}</span>
      <button class="afe-link" @click="cancel">{{ $t('ai:fix.cancel') }}</button>
    </div>
    <div v-if="phase === 'running' && remote" class="afe-note">{{ $t('ai:fix.remote', { provider: remote }) }}</div>
    <div v-if="text && parsed.explanation" class="afe-explain nm-selectable">{{ parsed.explanation }}</div>
    <template v-if="phase === 'done'">
      <div v-if="failed" class="afe-note afe-bad" role="alert">{{ $t('ai:fix.failed', { error: failed }) }}</div>
      <div v-if="warn && outcome === 'ok'" class="afe-warn">{{ $t('ai:code.destructive') }}</div>
      <div v-if="outcome === 'ok' && status === 'pending'" class="afe-note">{{ $t('ai:fix.proposed') }}</div>
      <div v-else-if="status === 'stale'" class="afe-note">{{ $t('ai:fix.dropped') }}</div>
      <div v-else-if="status === 'done'" class="afe-note">{{ $t('ai:fix.resolved') }}</div>
      <div v-else-if="outcome === 'stale'" class="afe-note">{{ $t('ai:fix.stale') }}</div>
      <div v-else-if="outcome === 'same'" class="afe-note">{{ $t('ai:fix.noChange') }}</div>
      <div v-else-if="outcome === 'nocode' && text" class="afe-note">{{ $t('ai:fix.noCode') }}</div>
      <div v-else-if="outcome === 'cancelled'" class="afe-note">{{ $t('ai:fix.cancelled') }}</div>
      <button v-if="text" class="afe-link" @click="analyze">{{ $t('ai:fix.again') }}</button>
    </template>
  </div>
</template>

<style scoped>
.afe { margin-top: 10px; display: flex; flex-direction: column; gap: 6px; align-items: flex-start; font-size: 12px; }
.afe-btn { display: inline-flex; align-items: center; gap: 6px; padding: 3px 10px; font: inherit; cursor: pointer; color: var(--nm-text); background: var(--ide-button-2); border: 1px solid color-mix(in srgb, var(--nm-text) 25%, transparent); border-radius: 3px; }
.afe-btn:hover { background: color-mix(in srgb, var(--nm-text) 12%, var(--ide-button-2)); }
.afe-icon { width: 14px; height: 14px; }
.afe-run { display: flex; align-items: center; gap: 8px; color: var(--nm-text-dim); }
.afe-note { color: var(--nm-text-dim); }
.afe-bad { color: var(--nm-danger); }
.afe-warn { padding: 4px 8px; border-radius: 3px; color: var(--nm-text); border: 1px solid color-mix(in srgb, var(--nm-warning) 60%, transparent); background: color-mix(in srgb, var(--nm-warning) 12%, transparent); }
.afe-explain { white-space: pre-wrap; color: var(--nm-text); max-width: 80ch; }
.afe-link { padding: 0; font: inherit; cursor: pointer; color: var(--nm-link, var(--nm-accent)); background: none; border: 0; text-decoration: underline; }
</style>
