<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { errorMessage } from '../api/client';
import { MCP_APPROVALS_EVENT, mcpApi, type McpApprovalRequest, type McpDecision } from '../api/mcp';
import { getCurrentWindow } from '@tauri-apps/api/window';
import { initWindowRole } from '../composables/windowRole';
import { ownsSavedState } from '../stores/tabs';
import CodeEditor from './CodeEditor.vue';

// A write an MCP client wants to run (docs/mcp.md, "Aprobaciones"): one
// dialog at a time, the rest queued. Mounted once, from App.vue. The backend
// rejects it on its own when the countdown ends. Every window keeps the list
// (it's broadcast), but only the one the backend names as `presenter` shows
// the dialog.

/** As broadcast: each request carries the label of the window that shows it. */
type Presented = McpApprovalRequest & { presenter?: string };

const queue = ref<Presented[]>([]);
/** Which window shows the dialog; null until an event says (the list read at
 *  mount has no presenter: then the primary window shows it). */
const presenter = ref<string | null>(null);
/** This window's label, known at once (the role's default says "main" until
 *  the backend answers, which would make every window the presenter). */
const LABEL = (() => { try { return getCurrentWindow().label; } catch { return 'main'; } })();
const mine = computed(() => presenter.value === null ? ownsSavedState() : presenter.value === LABEL);
const current = computed(() => (mine.value ? queue.value[0] : null) ?? null);
const busy = ref(false);
const now = ref(Date.now());

const secondsLeft = computed(() => {
  if (!current.value) return 0;
  const end = new Date(current.value.expires_at).getTime();
  return isNaN(end) ? current.value.timeout_secs : Math.max(0, Math.ceil((end - now.value) / 1000));
});
const countdown = computed(() => {
  const s = secondsLeft.value;
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
});

async function answer(decision: McpDecision) {
  const req = current.value;
  if (!req || busy.value) return;
  busy.value = true;
  try {
    await mcpApi.answerApproval(req.id, decision);
  } catch (e) {
    // Already gone (it timed out meanwhile): the event brings the new list.
    ElMessage.warning(errorMessage(e));
  } finally {
    queue.value = queue.value.filter((r) => r.id !== req.id && !(decision === 'approve_all' && r.client_id === req.client_id));
    busy.value = false;
  }
}

let unlisten: UnlistenFn | null = null;
let timer: number | undefined;

watch(current, (c) => {
  window.clearInterval(timer);
  timer = undefined;
  if (c) {
    now.value = Date.now();
    timer = window.setInterval(() => { now.value = Date.now(); }, 1000);
  }
});

onMounted(async () => {
  try {
    void initWindowRole();
    unlisten = await listen<Presented[]>(MCP_APPROVALS_EVENT, (e) => {
      queue.value = e.payload;
      // Each element names the presenter; an empty list leaves it as it was.
      if (e.payload[0]?.presenter) presenter.value = e.payload[0].presenter;
    });
    queue.value = await mcpApi.pendingApprovals();
  } catch { /* outside Tauri */ }
});
onBeforeUnmount(() => {
  unlisten?.();
  window.clearInterval(timer);
});
</script>

<template>
  <el-dialog
    :model-value="!!current"
    :title="current ? $t(current.database ? 'mcp:approval.title' : 'mcp:approval.titleNoDb', { client: current.client, connection: current.connection, database: current.database }) : ''"
    width="720px"
    append-to-body
    :show-close="false"
    :close-on-click-modal="false"
    :close-on-press-escape="false"
    class="mcp-approval"
  >
    <template v-if="current">
      <p class="mcp-ap-muted">
        {{ $t('mcp:approval.intro', { client: current.client, engine: current.engine }) }}
        <span v-if="queue.length > 1" class="mcp-ap-queue">{{ $t('mcp:approval.pending', { count: queue.length - 1 }) }}</span>
      </p>
      <div class="mcp-ap-code">
        <CodeEditor :model-value="current.code" :language="current.language" :dialect="current.dialect" read-only />
      </div>
      <p class="mcp-ap-muted">{{ $t('mcp:approval.countdown', { time: countdown }) }}</p>
      <div class="mcp-ap-warn">
        <el-icon><ei-warning-filled /></el-icon>
        <span>{{ $t('mcp:approval.approveAllWarning', { client: current.client }) }}</span>
      </div>
    </template>
    <template #footer>
      <el-button :disabled="busy" @click="answer('reject')">{{ $t('mcp:approval.reject') }}</el-button>
      <el-button type="warning" plain :disabled="busy" @click="answer('approve_all')">{{ $t('mcp:approval.approveAll') }}</el-button>
      <el-button type="primary" :disabled="busy" @click="answer('approve')">{{ $t('mcp:approval.approve') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.mcp-ap-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 8px; }
.mcp-ap-queue { margin-left: 6px; color: var(--nm-warning); }
.mcp-ap-code { height: 240px; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; margin-bottom: 8px; }
.mcp-ap-warn {
  display: flex; align-items: flex-start; gap: 8px; padding: 8px 12px; border-radius: 6px; font-size: 12.5px; line-height: 1.5;
  border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 8%, transparent);
}
.mcp-ap-warn .el-icon { color: var(--nm-warning); flex: none; margin-top: 2px; }
</style>
