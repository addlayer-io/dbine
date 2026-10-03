<script setup lang="ts">
import { computed, nextTick, onMounted, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { tb } from '../i18n/backend';
import { aiApi, type AiModel, type AiProvider, type AiProviderKind } from '../api/ai';
import { newQuery } from '../composables/actions';
import { useAiStore, type UiMessage } from '../stores/ai';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import { locale } from '../i18n';

// The AI assistant (right sidebar, ⌘I). It writes code; it never runs it:
// its code goes into the open query (appended, or replacing it when it
// corrects the editor) and the user runs it (docs/asistente-ia.md).

defineEmits<{ close: [] }>();

const ai = useAiStore();
const { t } = useTranslation();
const tabs = useTabsStore();
const conns = useConnectionsStore();

onMounted(() => ai.init());

const input = ref('');
const list = ref<HTMLElement | null>(null);
const showSetup = ref(false);
// "Historial": past conversations ("Nueva conversación" archives the current one).
const showHistory = ref(false);
function toggleHistory() {
  showHistory.value = !showHistory.value;
  if (showHistory.value) { showSetup.value = false; ai.loadHistory(); }
}
function openChat(id: string) {
  ai.restore(id);
  showHistory.value = false;
}
function when(ms: number) {
  return new Date(ms).toLocaleString(locale(), { dateStyle: 'short', timeStyle: 'short' });
}
async function clearHistory() {
  try {
    await ElMessageBox.confirm(t('ai:history.clearConfirm'), t('ai:history.clear'), { type: 'warning', confirmButtonText: t('ai:history.clear'), cancelButtonText: t('common:cancel') });
  } catch { return; }
  ai.clearHistory();
}

const provider = computed(() => ai.provider);
const needsSetup = computed(() => !!ai.detect && !provider.value);
const tab = computed(() => tabs.active);
const bridge = computed(() => ai.activeBridge);
const contextLine = computed(() => {
  const cur = tab.value;
  if (!cur) return t('ai:context.noDatabase');
  const c = conns.byId(cur.connectionId);
  const d = c ? conns.driverOf(cur.connectionId) : null;
  return [d?.name, c?.name, cur.database || null, bridge.value ? t('ai:context.openQuery') : null].filter(Boolean).join(' · ');
});

/** What the model selector shows: the model's own name (the built-in
 *  catalog's label, "Qwen2.5-Coder 32B"), or the provider's when it has no
 *  model to choose (Claude Code, Codex). The provider is the group header. */
function modelName(p: AiProvider, m: AiModel): string {
  if (p.kind === 'embedded') {
    const c = ai.detect?.catalog.find((x) => x.id === m.id);
    if (c) return tb(c.label);
  }
  return m.id || tb(p.label);
}

const selectValue = computed({
  get: () => (provider.value ? `${provider.value.kind}|${ai.model ?? ''}` : ''),
  set: (v: string) => {
    const [kind, ...rest] = v.split('|');
    ai.choose(kind as AiProviderKind, rest.join('|'));
  },
});

// -- conversation ------------------------------------------------------------------
function scrollDown() {
  nextTick(() => { if (list.value) list.value.scrollTop = list.value.scrollHeight; });
}
watch(() => ai.messages.map((m) => m.content.length + (m.thinking?.length ?? 0)).join(), scrollDown);
onMounted(scrollDown);

function send(text = input.value) {
  if (!text.trim() || ai.running) return;
  ai.send(text);
  input.value = '';
  scrollDown();
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
    e.preventDefault();
    send();
  }
}

const suggestions = computed(() => {
  const out: string[] = [];
  const b = bridge.value;
  if (b?.lastError()) out.push(t('ai:suggest.whyFails'));
  if (b?.text().trim()) out.push(t('ai:suggest.explain'), t('ai:suggest.optimize'));
  if (tab.value) out.push(t('ai:suggest.largestTables'), t('ai:suggest.relatedTables'));
  else out.push(t('ai:suggest.joinAggregate'));
  return out.slice(0, 4);
});

// -- rendering (a small Markdown subset; everything is escaped first) ----------------
type Part = { kind: 'text'; html: string } | { kind: 'code'; lang: string; code: string; open: boolean };

function esc(s: string) {
  return s.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;');
}
function inline(s: string) {
  return esc(s)
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[\s(])\*([^*\s][^*]*)\*(?=[\s).,;:!?]|$)/g, '$1<em>$2</em>');
}
function textHtml(block: string) {
  const out: string[] = [];
  let list: 'ul' | 'ol' | null = null;
  const close = () => { if (list) { out.push(`</${list}>`); list = null; } };
  for (const raw of block.split('\n')) {
    const line = raw.trimEnd();
    const ul = /^\s*[-*]\s+(.*)$/.exec(line);
    const ol = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    const h = /^(#{1,4})\s+(.*)$/.exec(line);
    if (ul || ol) {
      const want = ul ? 'ul' : 'ol';
      if (list !== want) { close(); out.push(`<${want}>`); list = want; }
      out.push(`<li>${inline((ul ?? ol)![1])}</li>`);
    } else {
      close();
      if (h) out.push(`<p class="ai-h">${inline(h[2])}</p>`);
      else if (line.trim()) out.push(`<p>${inline(line)}</p>`);
    }
  }
  close();
  return out.join('');
}
function parts(md: string): Part[] {
  const out: Part[] = [];
  const re = /```([\w+#.-]*)[^\n]*\n([\s\S]*?)(```|$)/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(md))) {
    if (m.index > last) out.push({ kind: 'text', html: textHtml(md.slice(last, m.index)) });
    out.push({ kind: 'code', lang: m[1] || '', code: m[2].replace(/\n$/, ''), open: m[3] !== '```' });
    last = re.lastIndex;
    if (m[3] !== '```') break;
  }
  if (last < md.length) out.push({ kind: 'text', html: textHtml(md.slice(last)) });
  return out;
}
/** Hide a model's <think>…</think> (some local models inline it). */
function visible(m: UiMessage) {
  return m.content.replace(/<think>[\s\S]*?(<\/think>|$)/g, '').trimStart();
}
function thinkingOf(m: UiMessage) {
  const inlined = /<think>([\s\S]*?)(<\/think>|$)/.exec(m.content)?.[1] ?? '';
  return (m.thinking ?? '') + inlined;
}

/** Statements that change data or structure: flagged whatever the model says. */
const DESTRUCTIVE = /\b(drop|delete|truncate|alter|update|insert|merge|create|rename|grant|revoke|flushall|flushdb|del|dropdatabase|deletemany|remove)\b/i;
function destructive(code: string) {
  // Ignore comments and string literals.
  const bare = code.replace(/--[^\n]*|\/\*[\s\S]*?\*\/|'(?:[^']|'')*'/g, ' ');
  return DESTRUCTIVE.test(bare);
}

// -- code actions (never run) -----------------------------------------------------------
function addToQuery(code: string) {
  const b = bridge.value;
  if (b) {
    b.append(code);
    ElMessage.success(t('ai:code.appended'));
  } else if (tab.value) {
    newQuery(tab.value.connectionId, tab.value.database, code, t('ai:code.newQueryTitle'));
  }
}
function replaceQuery(code: string) {
  bridge.value?.replace(code);
  ElMessage.success(t('ai:code.replaced'));
}
async function copy(code: string) {
  try { await navigator.clipboard.writeText(code); ElMessage.success(t('common:copied')); } catch { /* ignore */ }
}

// -- setup -------------------------------------------------------------------------------
const busy = ref<string | null>(null);
async function download(id: string) {
  try { await ai.downloadModel(id); } catch (e) { ElMessage.error(errorMessage(e)); }
}
// A bigger built-in model this machine handles comfortably than the one in
// use: offered once in the chat (the setup panel always marks it).
const BETTER_DISMISSED = 'dbine.ai.betterModelDismissed';
const betterDismissed = ref<string | null>((() => { try { return localStorage.getItem(BETTER_DISMISSED); } catch { return null; } })());
const betterModel = computed(() => {
  const cat = ai.detect?.catalog;
  if (provider.value?.kind !== 'embedded' || !cat) return null;
  const rec = cat.findIndex((m) => m.recommended);
  const cur = cat.findIndex((m) => m.id === ai.model);
  if (rec < 0 || cur < 0 || rec <= cur || cat[rec].installed || betterDismissed.value === cat[rec].id) return null;
  return cat[rec];
});
function dismissBetter(id: string) {
  betterDismissed.value = id;
  try { localStorage.setItem(BETTER_DISMISSED, id); } catch { /* not kept */ }
}
async function startOllama() {
  busy.value = 'ollama';
  try { await aiApi.startOllama(); await ai.refresh(); } catch (e) { ElMessage.error(errorMessage(e)); } finally { busy.value = null; }
}
async function pullOllama() {
  try { await ai.pullOllama(ai.detect?.ollama_recommended_model ?? 'qwen2.5-coder:7b'); } catch (e) { ElMessage.error(errorMessage(e)); }
}
function pct(id: string) {
  const d = ai.downloads[id];
  return d && d.total ? Math.floor((d.done / d.total) * 100) : 0;
}
function gb(n: number) {
  return `${(n / 1e9).toLocaleString(locale(), { minimumFractionDigits: 1, maximumFractionDigits: 1 })} GB`;
}
const phaseText = computed(() => ({
  schema: t('ai:phase.schema'),
  engine: `${t('ai:phase.engine')} ${tb(ai.phaseNote)}`,
  thinking: t('ai:phase.thinking'),
  retry: t('ai:phase.retry'),
  writing: '',
} as Record<string, string>)[ai.phase ?? ''] ?? '');
</script>

<template>
  <aside class="ai">
    <header class="ai-head">
      <el-icon class="ai-logo"><ei-magic-stick /></el-icon>
      <strong>{{ $t('ai:title') }}</strong>
      <el-select v-if="provider" v-model="selectValue" size="small" class="ai-model" :title="`${tb(provider.label)} · ${tb(provider.status)}`">
        <el-option-group v-for="p in ai.usable" :key="p.kind" :label="tb(p.label)">
          <el-option v-for="m in p.models" :key="p.kind + m.id" :value="`${p.kind}|${m.id}`" :label="modelName(p, m)">
            <span>{{ modelName(p, m) }}</span>
            <span v-if="m.detail" class="ai-opt-detail">{{ tb(m.detail) }}</span>
          </el-option>
        </el-option-group>
      </el-select>
      <div style="flex: 1" />
      <button class="ai-icon" :title="$t('ai:head.history')" :class="{ on: showHistory }" @click="toggleHistory"><el-icon><ei-clock /></el-icon></button>
      <button class="ai-icon" :title="$t('ai:head.setup')" :class="{ on: showSetup }" @click="showSetup = !showSetup; showHistory = false"><el-icon><ei-setting /></el-icon></button>
      <button class="ai-icon" :title="$t('ai:head.newChat')" :disabled="!!ai.running || !ai.messages.length" @click="ai.clear()"><el-icon><ei-document-add /></el-icon></button>
      <button class="ai-icon" :title="$t('ai:head.close')" @click="$emit('close')"><el-icon><ei-close /></el-icon></button>
    </header>

    <!-- Setup: no provider, or asked for -->
    <section v-if="showHistory" class="ai-history">
      <div class="ai-history-head">
        <strong>{{ $t('ai:history.title') }}</strong>
        <el-button v-if="ai.history.length" size="small" text @click="clearHistory">{{ $t('ai:history.clear') }}</el-button>
      </div>
      <p v-if="!ai.history.length" class="ai-muted">{{ $t('ai:history.empty') }}</p>
      <div v-for="c in ai.history" :key="c.id" class="ai-chat-row" :class="{ disabled: !!ai.running }" @click="!ai.running && openChat(c.id)">
        <div class="ai-chat-main">
          <span class="ai-chat-title">{{ c.title || $t('ai:history.untitled') }}</span>
          <span class="ai-muted ai-small">{{ when(c.updated) }} · {{ $t('ai:history.messages', { count: c.messages.length }) }}</span>
        </div>
        <button class="ai-icon" :title="$t('ai:history.delete')" @click.stop="ai.deleteChat(c.id)"><el-icon><ei-delete /></el-icon></button>
      </div>
    </section>

    <section v-else-if="needsSetup || showSetup || !ai.detect" class="ai-setup">
      <div v-if="!ai.detect" class="ai-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('ai:setup.detecting') }}</div>
      <template v-else>
        <p v-if="needsSetup" class="ai-lead">
          <i18next :translation="$t('ai:setup.lead')">
            <template #model><b>{{ $t('ai:setup.leadModel') }}</b></template>
          </i18next>
        </p>
        <h4>{{ $t('ai:setup.embedded') }}</h4>
        <p v-if="!ai.detect.embedded_enabled" class="ai-muted">{{ $t('ai:setup.embeddedUnavailable') }}</p>
        <div v-for="m in ai.detect.catalog" v-else :key="m.id" class="ai-model-row">
          <div>
            <strong>{{ tb(m.label) }}</strong>
            <span v-if="m.recommended" class="ai-tag">{{ $t('ai:setup.recommended') }}</span>
            <span class="ai-muted">{{ tb(m.detail) }}</span>
            <el-progress v-if="ai.downloads[m.id]" :percentage="pct(m.id)" :stroke-width="4" style="margin-top: 4px" />
          </div>
          <el-button v-if="m.installed" size="small" @click="ai.choose('embedded', m.id); showSetup = false">{{ $t('ai:setup.use') }}</el-button>
          <el-button v-else-if="ai.downloads[m.id]" size="small" :title="$t('ai:setup.pauseHint')" @click="ai.cancelDownload(m.id)">{{ $t('ai:setup.pause') }}</el-button>
          <el-button v-else size="small" type="primary" plain @click="download(m.id)">{{ $t('ai:setup.download', { size: gb(m.size) }) }}</el-button>
        </div>

        <h4>{{ $t('ai:setup.detected') }}</h4>
        <div v-for="p in ai.providers.filter((x) => x.kind !== 'embedded')" :key="p.kind" class="ai-prov">
          <span class="ai-dot" :class="{ ok: p.available, warn: p.installed && !p.available }" />
          <div>
            <strong>{{ tb(p.label) }}</strong>
            <span class="ai-muted">{{ tb(p.status) }}</span>
          </div>
          <el-button v-if="p.action === 'start_ollama'" size="small" :loading="busy === 'ollama'" @click="startOllama">{{ $t('common:open') }}</el-button>
          <el-button v-else-if="p.action === 'pull_model' && !ai.downloads[`ollama:${ai.detect.ollama_recommended_model}`]" size="small" @click="pullOllama">
            {{ $t('ai:setup.pull', { model: ai.detect.ollama_recommended_model }) }}
          </el-button>
          <span v-else-if="p.action === 'pull_model'" class="ai-muted">{{ tb(ai.downloads[`ollama:${ai.detect.ollama_recommended_model}`]?.status) }}</span>
          <el-button v-else-if="p.available" size="small" text @click="ai.choose(p.kind); showSetup = false">{{ $t('ai:setup.use') }}</el-button>
        </div>
        <p class="ai-muted ai-small">
          {{ $t('ai:setup.cliNote') }}
        </p>
        <el-button size="small" :loading="ai.detecting" @click="ai.refresh()"><el-icon><ei-refresh /></el-icon>&nbsp;{{ $t('ai:setup.redetect') }}</el-button>
      </template>
    </section>

    <!-- Conversation -->
    <template v-if="provider && !showSetup && !showHistory">
      <div class="ai-context">
        <span :title="contextLine">{{ contextLine }}</span>
        <el-checkbox :model-value="ai.includeSchema" size="small" @update:model-value="(v: unknown) => ai.setIncludeSchema(!!v)">{{ $t('ai:context.schema') }}</el-checkbox>
      </div>
      <div class="ai-privacy" :class="{ local: provider.local }">
        <el-icon><ei-lock /></el-icon>
        <span v-if="provider.local">{{ $t('ai:privacy.local') }}</span>
        <span v-else>{{ $t('ai:privacy.remote', { vendor: provider.kind === 'claude_code' ? 'Anthropic' : 'OpenAI' }) }}</span>
      </div>
      <div v-if="betterModel" class="ai-better">
        <span>{{ $t('ai:better.text', { model: tb(betterModel.label) }) }}</span>
        <el-progress v-if="ai.downloads[betterModel.id]" :percentage="pct(betterModel.id)" :stroke-width="4" />
        <div v-else class="ai-better-acts">
          <el-button size="small" type="primary" plain @click="download(betterModel.id)">{{ $t('ai:setup.download', { size: gb(betterModel.size) }) }}</el-button>
          <el-button size="small" text @click="dismissBetter(betterModel.id)">{{ $t('ai:better.dismiss') }}</el-button>
        </div>
      </div>

      <div ref="list" class="ai-list">
        <div v-if="!ai.messages.length" class="ai-empty">
          <p>{{ $t('ai:empty.lead') }}</p>
          <p class="ai-muted ai-small">{{ $t('ai:empty.neverRuns') }}</p>
          <button v-for="s in suggestions" :key="s" class="ai-sug" @click="send(s)">{{ s }}</button>
        </div>
        <div v-for="m in ai.messages" :key="m.id" class="ai-msg" :class="m.role">
          <template v-if="m.role === 'user'">
            <div class="ai-bubble nm-selectable">{{ m.content }}</div>
            <div v-if="m.context" class="ai-sent">{{ $t('ai:context.sent', { context: tb(m.context) }) }}</div>
          </template>
          <template v-else>
            <details v-if="thinkingOf(m)" class="ai-think">
              <summary>{{ $t('ai:reasoning') }}</summary>
              <pre class="nm-selectable">{{ thinkingOf(m) }}</pre>
            </details>
            <template v-for="(p, i) in parts(visible(m))" :key="i">
              <div v-if="p.kind === 'text'" class="ai-md nm-selectable" v-html="p.html" />
              <div v-else class="ai-code">
                <div class="ai-code-bar">
                  <span>{{ p.lang || $t('ai:code.code') }}</span>
                  <template v-if="!p.open || !m.pending">
                    <button v-if="bridge || tab" class="ai-act primary" :title="bridge ? $t('ai:code.appendHint') : $t('ai:code.newQueryHint')" @click="addToQuery(p.code)">
                      <el-icon><ei-bottom /></el-icon>{{ bridge ? $t('ai:code.append') : $t('ai:code.newQuery') }}
                    </button>
                    <button v-if="bridge && bridge.text().trim()" class="ai-act" :title="$t('ai:code.replaceHint')" @click="replaceQuery(p.code)">
                      <el-icon><ei-refresh-right /></el-icon>{{ $t('ai:code.replace') }}
                    </button>
                    <button class="ai-act" :title="$t('common:copy')" @click="copy(p.code)"><el-icon><ei-copy-document /></el-icon></button>
                  </template>
                </div>
                <pre class="nm-selectable"><code>{{ p.code }}</code></pre>
                <div v-if="destructive(p.code)" class="ai-warn">
                  <el-icon><ei-warning-filled /></el-icon>
                  {{ $t('ai:code.destructive') }}
                </div>
              </div>
            </template>
            <div v-if="m.pending && !m.content" class="ai-muted ai-phase"><el-icon class="is-loading"><ei-loading /></el-icon> {{ phaseText || $t('ai:phase.thinking') }}</div>
            <div v-if="m.error" class="ai-error">{{ m.error }}</div>
            <div v-if="!m.pending && m.provider" class="ai-sent">{{ tb(m.provider) }}</div>
          </template>
        </div>
      </div>

      <div class="ai-compose">
        <el-input
          v-model="input"
          type="textarea"
          :autosize="{ minRows: 2, maxRows: 8 }"
          resize="none"
          :placeholder="$t('ai:compose.placeholder')"
          @keydown="onKey"
        />
        <el-button v-if="ai.running" class="ai-send" type="danger" plain @click="ai.stop()"><el-icon><ei-video-pause /></el-icon>&nbsp;{{ $t('common:stop') }}</el-button>
        <el-button v-else class="ai-send" type="primary" :disabled="!input.trim()" @click="send()"><el-icon><ei-promotion /></el-icon>&nbsp;{{ $t('ai:compose.send') }}</el-button>
      </div>
    </template>
  </aside>
</template>

<style scoped>
.ai { display: flex; flex-direction: column; height: 100%; min-width: 0; background: var(--ide-sidebar); border-left: 1px solid var(--nm-border); font-size: 13px; }
.ai-head { display: flex; align-items: center; gap: 6px; padding: 6px 8px; border-bottom: 1px solid var(--nm-border); min-width: 0; }
.ai-logo { color: var(--nm-accent); }
.ai-head strong { font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-strong); }
.ai-model { width: 170px; }
.ai-opt-detail { float: right; margin-left: 12px; color: var(--nm-text-dim); font-size: 11px; }
.ai-icon { display: inline-flex; align-items: center; justify-content: center; width: 24px; height: 24px; border: none; border-radius: 4px; background: transparent; color: var(--nm-text-dim); cursor: pointer; }
.ai-icon:hover:not(:disabled), .ai-icon.on { background: var(--ide-hover); color: var(--nm-text-strong); }
.ai-icon:disabled { opacity: 0.4; cursor: default; }

.ai-setup { padding: 10px 12px; overflow: auto; }
.ai-setup h4 { margin: 14px 0 6px; font-size: 12px; color: var(--nm-text-strong); }
.ai-lead { margin: 0 0 4px; line-height: 1.5; }
.ai-muted { color: var(--nm-text-dim); font-size: 12px; }
.ai-small { font-size: 11.5px; line-height: 1.45; }
.ai-model-row, .ai-prov { display: flex; align-items: center; gap: 8px; padding: 6px 0; border-bottom: 1px solid var(--nm-border-soft, var(--nm-border)); }
.ai-model-row > div, .ai-prov > div { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 1px; }
.ai-tag { align-self: flex-start; font-size: 10.5px; padding: 0 6px; border-radius: 8px; background: color-mix(in srgb, var(--nm-success) 18%, transparent); color: var(--nm-success); }
.ai-dot { width: 8px; height: 8px; border-radius: 50%; background: var(--nm-text-muted, #666); flex: none; }
.ai-dot.ok { background: var(--nm-success); }
.ai-dot.warn { background: var(--nm-warning); }

.ai-context { display: flex; align-items: center; gap: 8px; padding: 5px 10px; font-size: 11.5px; color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border); }
.ai-context > span { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ai-privacy { display: flex; align-items: center; gap: 5px; padding: 4px 10px; font-size: 11px; color: var(--nm-text-dim); }
.ai-privacy.local { color: var(--nm-success); }
.ai-history { flex: 1; min-height: 0; overflow: auto; padding: 8px 10px; display: flex; flex-direction: column; gap: 2px; }
.ai-history-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 6px; }
.ai-chat-row { display: flex; align-items: center; gap: 6px; padding: 6px 8px; border-radius: 5px; cursor: pointer; }
.ai-chat-row:hover { background: var(--ide-hover, color-mix(in srgb, var(--nm-text) 8%, transparent)); }
.ai-chat-row.disabled { cursor: default; opacity: 0.6; }
.ai-chat-main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.ai-chat-title { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-strong); font-size: 12.5px; }
.ai-better { margin: 4px 10px 6px; padding: 8px 10px; border: 1px solid var(--nm-border); border-radius: 6px; font-size: 12px; color: var(--nm-text); display: flex; flex-direction: column; gap: 6px; }
.ai-better-acts { display: flex; gap: 6px; }
.ai-better-acts .el-button { margin: 0; }

.ai-list { flex: 1; min-height: 0; overflow: auto; padding: 8px 10px 12px; display: flex; flex-direction: column; gap: 12px; }
.ai-empty { color: var(--nm-text); line-height: 1.5; }
.ai-empty p { margin: 0 0 8px; }
.ai-sug { display: block; width: 100%; text-align: left; margin-top: 6px; padding: 6px 9px; border: 1px solid var(--nm-border); border-radius: 6px; background: transparent; color: var(--nm-text); font: inherit; font-size: 12.5px; cursor: pointer; }
.ai-sug:hover { border-color: var(--nm-accent); }
.ai-msg.user { align-self: flex-end; max-width: 90%; display: flex; flex-direction: column; align-items: flex-end; }
.ai-bubble { padding: 7px 10px; border-radius: 10px 10px 2px 10px; background: var(--ide-selection); color: var(--nm-text-strong); white-space: pre-wrap; word-break: break-word; }
.ai-sent { font-size: 10.5px; color: var(--nm-text-dim); margin-top: 3px; }
.ai-msg.assistant { min-width: 0; }
.ai-md :deep(p) { margin: 0 0 6px; line-height: 1.55; }
.ai-md :deep(.ai-h) { font-weight: 600; color: var(--nm-text-strong); }
.ai-md :deep(ul), .ai-md :deep(ol) { margin: 0 0 6px; padding-left: 18px; line-height: 1.55; }
.ai-md :deep(code) { font-family: var(--nm-mono); font-size: 12px; padding: 0 4px; border-radius: 3px; background: rgba(255, 255, 255, 0.07); }
.ai-code { margin: 4px 0 8px; border: 1px solid var(--nm-border); border-radius: 6px; overflow: hidden; background: var(--ide-editor); }
.ai-code-bar { display: flex; align-items: center; gap: 4px; padding: 3px 4px 3px 8px; font-size: 11px; color: var(--nm-text-dim); border-bottom: 1px solid var(--nm-border); }
.ai-code-bar > span { flex: 1; }
.ai-act { display: inline-flex; align-items: center; gap: 4px; padding: 2px 7px; border: none; border-radius: 4px; background: transparent; color: var(--nm-text); font: inherit; font-size: 11.5px; cursor: pointer; }
.ai-act:hover { background: var(--ide-hover); }
.ai-act.primary { color: var(--nm-accent); }
.ai-code pre { margin: 0; padding: 8px 10px; overflow: auto; white-space: pre-wrap; word-break: break-word; max-height: 360px; font-family: var(--nm-mono); font-size: 12px; line-height: 1.5; color: var(--nm-text-strong); }
.ai-warn { display: flex; align-items: center; gap: 6px; padding: 5px 8px; font-size: 11.5px; color: var(--nm-warning); border-top: 1px solid var(--nm-border); background: color-mix(in srgb, var(--nm-warning) 8%, transparent); }
.ai-think { margin-bottom: 6px; font-size: 11.5px; color: var(--nm-text-dim); }
.ai-think pre { white-space: pre-wrap; margin: 4px 0 0; max-height: 160px; overflow: auto; font-family: inherit; }
.ai-phase { display: flex; align-items: center; gap: 6px; }
.ai-error { color: var(--nm-danger); font-size: 12px; white-space: pre-wrap; }

.ai-compose { display: flex; flex-direction: column; gap: 6px; padding: 8px 10px 10px; border-top: 1px solid var(--nm-border); }
.ai-send { align-self: flex-end; }
</style>
