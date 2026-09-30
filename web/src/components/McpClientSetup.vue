<script setup lang="ts">
import { computed, ref } from 'vue';
import { mcpSnippets } from '../api/mcp';

// How to connect each assistant to DBine's MCP server: one tab per client,
// with what to run or where to paste it, and a copy button.

const props = defineProps<{ url: string; token: string }>();
const emit = defineEmits<{ copy: [text: string] }>();

type Client = 'claude' | 'claudeDesktop' | 'cursor' | 'codex' | 'vscode' | 'windsurf' | 'chatgpt';
const CLIENTS: { id: Client; label: string }[] = [
  { id: 'claude', label: 'Claude Code' },
  { id: 'claudeDesktop', label: 'Claude Desktop' },
  { id: 'cursor', label: 'Cursor' },
  { id: 'codex', label: 'Codex' },
  { id: 'vscode', label: 'VS Code' },
  { id: 'windsurf', label: 'Windsurf' },
  { id: 'chatgpt', label: 'ChatGPT' },
];
const active = ref<Client>('claude');
const snippets = computed(() => mcpSnippets(props.url, props.token));
const text = computed(() => (active.value === 'chatgpt' ? '' : snippets.value[active.value]));
</script>

<template>
  <el-tabs v-model="active" class="mcs">
    <el-tab-pane v-for="c in CLIENTS" :key="c.id" :name="c.id" :label="c.label">
      <p class="mcs-where">{{ $t(`mcp:setup.${c.id}`) }}</p>
      <div v-if="c.id !== 'chatgpt'" class="mcs-snip">
        <pre class="nm-selectable">{{ text }}</pre>
        <el-button size="small" @click="emit('copy', text)"><el-icon><ei-document-copy /></el-icon>&nbsp;{{ $t('mcp:copy') }}</el-button>
      </div>
    </el-tab-pane>
  </el-tabs>
</template>

<style scoped>
.mcs :deep(.el-tabs__header) { margin-bottom: 8px; }
.mcs-where { margin: 0 0 6px; font-size: 12px; color: var(--nm-text-dim); line-height: 1.5; }
.mcs-snip { display: flex; align-items: flex-start; gap: 8px; }
.mcs-snip pre {
  flex: 1; margin: 0; padding: 8px 10px; overflow: auto; max-height: 220px;
  border: 1px solid var(--nm-border); border-radius: 4px; background: var(--ide-editor, var(--nm-bg-elev));
  font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text-strong); white-space: pre-wrap; word-break: break-all;
}
</style>
