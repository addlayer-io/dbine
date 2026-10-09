<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { MCP_APPROVALS_EVENT, mcpApi, mcpSnippets, type McpActivity, type McpClient, type McpLevel, type McpStatus } from '../api/mcp';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import McpClientSetup from './McpClientSetup.vue';

// Configuración › MCP (docs/mcp.md): the local server, its clients and what
// they did.

const { t } = useTranslation();
const conns = useConnectionsStore();

const status = ref<McpStatus | null>(null);
const port = ref(27517);
const saving = ref(false);

async function load() {
  try {
    status.value = await mcpApi.status();
    port.value = status.value.port;
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function configure(patch: Partial<Pick<McpStatus, 'enabled' | 'port' | 'default_level'>>) {
  if (!status.value) return;
  const next = { enabled: status.value.enabled, port: status.value.port, default_level: status.value.default_level, ...patch };
  saving.value = true;
  try {
    status.value = await mcpApi.configure(next.enabled, next.port, next.default_level);
    port.value = status.value.port;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    port.value = status.value.port;
  } finally {
    saving.value = false;
  }
}

function applyPort() {
  if (status.value && port.value && port.value !== status.value.port) configure({ port: port.value });
}

const LEVELS: McpLevel[] = ['disabled', 'schema', 'read', 'write'];

// -- clients ---------------------------------------------------------------------------
const newName = ref('');
const creating = ref(false);
/** The client just created, with its token (shown once). */
const created = ref<{ client: McpClient; token: string } | null>(null);
const snippets = computed(() => (created.value && status.value ? mcpSnippets(status.value.url, created.value.token) : null));

async function createClient() {
  const name = newName.value.trim();
  if (!name) return;
  creating.value = true;
  try {
    created.value = await mcpApi.createClient(name);
    newName.value = '';
    await load();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    creating.value = false;
  }
}

async function copy(text: string, message = t('mcp:copied')) {
  try {
    await navigator.clipboard.writeText(text);
    ElMessage.success({ message, duration: 2500 });
  } catch { /* no clipboard */ }
}

/** A listed client's config: its token isn't kept, so a placeholder goes in. */
function copyConfig(kind: 'claude' | 'codex' | 'json') {
  if (!status.value) return;
  copy(mcpSnippets(status.value.url, '<token>')[kind], t('mcp:copiedWithoutToken'));
}

/** Ask again before each write of this client. */
async function clearApproveAll(c: McpClient, kind: 'read' | 'write') {
  try {
    status.value = await mcpApi.clearApproveAll(c.id, kind);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

async function revoke(c: McpClient) {
  try {
    await ElMessageBox.confirm(t('mcp:revokeConfirm', { name: c.name }), t('mcp:revokeTitle'), {
      confirmButtonText: t('mcp:revoke'), cancelButtonText: t('common:cancel'), type: 'warning',
    });
  } catch { return; }
  try {
    await mcpApi.revokeClient(c.id);
    await load();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}

// -- activity --------------------------------------------------------------------------
const activity = ref<McpActivity[]>([]);
const filterClient = ref<string | null>(null);
const filterConnection = ref<string | null>(null);
const loadingActivity = ref(false);

async function loadActivity() {
  loadingActivity.value = true;
  try {
    activity.value = await mcpApi.activity(filterClient.value || null, filterConnection.value || null);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    loadingActivity.value = false;
  }
}

const connectionNames = computed(() => [...new Set(conns.list.map((c) => c.name))].sort((a, b) => a.localeCompare(b)));

function fmtDate(iso: string | null | undefined) {
  if (!iso) return '';
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toLocaleString(locale(), { dateStyle: 'short', timeStyle: 'medium' });
}

/** The log's `execute:approved`… as "execute · aprobado". */
function toolLabel(tool: string) {
  const [name, phase] = tool.split(':');
  return phase ? `${name} · ${t(`mcp:phase.${phase}`)}` : tool;
}

// A write was asked or answered: approve-all and the log may have changed.
let unlisten: UnlistenFn | null = null;
onMounted(async () => {
  load();
  loadActivity();
  try {
    unlisten = await listen(MCP_APPROVALS_EVENT, () => { load(); loadActivity(); });
  } catch { /* outside Tauri */ }
});
onBeforeUnmount(() => unlisten?.());
</script>

<template>
  <div class="mcp">
    <h3>{{ $t('mcp:title') }}</h3>
    <p class="mcp-muted">{{ $t('mcp:intro') }}</p>
    <div class="mcp-warn"><el-icon><ei-warning-filled /></el-icon>{{ $t('mcp:warning') }}</div>

    <template v-if="status">
      <div class="mcp-row">
        <div>
          <strong>{{ $t('mcp:server') }}</strong>
          <span>{{ $t('mcp:serverHelp') }}</span>
          <span v-if="status.enabled && status.running" class="mcp-ok">
            {{ $t('mcp:running', { url: status.url }) }}
            <el-button size="small" text :title="$t('mcp:copyUrl')" @click="copy(status.url, t('mcp:urlCopied'))">
              <el-icon><ei-document-copy /></el-icon>&nbsp;{{ $t('mcp:copyUrl') }}
            </el-button>
          </span>
          <span v-else-if="status.enabled && status.error" class="mcp-err">{{ tb(status.error) }}</span>
          <span v-else-if="!status.enabled">{{ $t('mcp:stopped') }}</span>
        </div>
        <el-switch :model-value="status.enabled" :loading="saving" @update:model-value="(v: string | number | boolean) => configure({ enabled: !!v })" />
      </div>
      <div class="mcp-row">
        <div><strong>{{ $t('mcp:port') }}</strong><span>{{ $t('mcp:portHelp') }}</span></div>
        <el-input-number v-model="port" :min="1024" :max="65535" :controls="false" style="width: 120px" @blur="applyPort" @keyup.enter="applyPort" />
      </div>
      <div class="mcp-row">
        <div>
          <strong>{{ $t('mcp:defaultLevel') }}</strong>
          <span>{{ $t('mcp:defaultLevelHelp') }}</span>
          <span>{{ $t(`mcp:levelHelp.${status.default_level}`) }}</span>
        </div>
        <el-select :model-value="status.default_level" style="width: 240px" @update:model-value="(v: McpLevel) => configure({ default_level: v })">
          <el-option v-for="l in LEVELS" :key="l" :label="$t(`mcp:level.${l}`)" :value="l" />
        </el-select>
      </div>

      <h4>{{ $t('mcp:clients') }}</h4>
      <p class="mcp-muted">{{ $t('mcp:clientsHelp') }}</p>
      <div class="mcp-inline">
        <el-input v-model="newName" :placeholder="$t('mcp:clientName')" @keyup.enter="createClient" />
        <el-button type="primary" :disabled="!newName.trim()" :loading="creating" @click="createClient">{{ $t('mcp:create') }}</el-button>
      </div>
      <div v-if="!status.clients.length" class="mcp-muted mcp-small">{{ $t('mcp:noClients') }}</div>
      <div v-for="c in status.clients" :key="c.id" class="mcp-client">
        <el-icon><ei-key /></el-icon>
        <div class="mcp-client-text">
          <strong>{{ c.name }}</strong>
          <span>{{ $t('mcp:created', { date: fmtDate(c.created_at) }) }} · {{ c.last_used_at ? $t('mcp:lastUsed', { date: fmtDate(c.last_used_at) }) : $t('mcp:neverUsed') }}</span>
          <span v-if="c.approve_all" class="mcp-approve-all">
            <el-icon><ei-warning-filled /></el-icon>{{ $t('mcp:approveAllOn') }}
            <el-button size="small" link type="warning" :title="$t('mcp:approveAllOffTitle')" @click="clearApproveAll(c, 'write')">{{ $t('mcp:approveAllOff') }}</el-button>
          </span>
          <span v-if="c.approve_all_reads" class="mcp-approve-all">
            <el-icon><ei-warning-filled /></el-icon>{{ $t('mcp:approveAllReadsOn') }}
            <el-button size="small" link type="warning" :title="$t('mcp:approveAllReadsOffTitle')" @click="clearApproveAll(c, 'read')">{{ $t('mcp:approveAllOff') }}</el-button>
          </span>
        </div>
        <el-dropdown trigger="click" @command="copyConfig">
          <el-button size="small" text>{{ $t('mcp:copyConfig') }}<el-icon class="el-icon--right"><ei-arrow-down /></el-icon></el-button>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item command="claude">{{ $t('mcp:forClaude') }}</el-dropdown-item>
              <el-dropdown-item command="codex">{{ $t('mcp:forCodex') }}</el-dropdown-item>
              <el-dropdown-item command="json">{{ $t('mcp:forJson') }}</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
        <el-button size="small" text type="danger" @click="revoke(c)">{{ $t('mcp:revoke') }}</el-button>
      </div>

      <h4>{{ $t('mcp:setup.title') }}</h4>
      <p class="mcp-muted">{{ $t('mcp:setup.help') }}</p>
      <McpClientSetup :url="status.url" token="<token>" @copy="(x) => copy(x, t('mcp:copiedWithoutToken'))" />

      <h4>{{ $t('mcp:activity') }}</h4>
      <p class="mcp-muted">{{ $t('mcp:activityHelp') }}</p>
      <div class="mcp-inline">
        <el-select v-model="filterClient" clearable :placeholder="$t('mcp:allClients')" @change="loadActivity">
          <el-option v-for="c in status.clients" :key="c.id" :label="c.name" :value="c.name" />
        </el-select>
        <el-select v-model="filterConnection" clearable filterable :placeholder="$t('mcp:allConnections')" @change="loadActivity">
          <el-option v-for="n in connectionNames" :key="n" :label="n" :value="n" />
        </el-select>
        <el-button :loading="loadingActivity" @click="loadActivity"><el-icon><ei-refresh /></el-icon>&nbsp;{{ $t('mcp:refresh') }}</el-button>
      </div>
      <el-table :data="activity" size="small" max-height="320" :empty-text="$t('mcp:noActivity')" class="mcp-log">
        <el-table-column :label="$t('mcp:col.at')" width="150">
          <template #default="{ row }">{{ fmtDate(row.at) }}</template>
        </el-table-column>
        <el-table-column prop="client" :label="$t('mcp:col.client')" width="110" show-overflow-tooltip />
        <el-table-column prop="connection" :label="$t('mcp:col.connection')" width="120" show-overflow-tooltip />
        <el-table-column :label="$t('mcp:col.tool')" width="150" show-overflow-tooltip>
          <template #default="{ row }">{{ toolLabel(row.tool) }}</template>
        </el-table-column>
        <el-table-column prop="summary" :label="$t('mcp:col.summary')" min-width="160" show-overflow-tooltip />
        <el-table-column :label="$t('mcp:col.result')" width="120" show-overflow-tooltip>
          <template #default="{ row }">
            <span v-if="row.tool.endsWith(':request')">—</span>
            <span v-else-if="row.ok" class="mcp-ok">{{ $t('mcp:ok') }}<template v-if="row.rows !== null"> · {{ $t('mcp:rows', { count: row.rows }) }}</template></span>
            <span v-else class="mcp-err" :title="tb(row.error)">{{ $t('mcp:error') }}: {{ tb(row.error) }}</span>
          </template>
        </el-table-column>
      </el-table>
    </template>

    <!-- The new client's token: shown this once. -->
    <el-dialog :model-value="!!created" :title="created ? $t('mcp:tokenTitle', { name: created.client.name }) : ''" width="640px" append-to-body @close="created = null">
      <template v-if="created && snippets">
        <p class="mcp-warn"><el-icon><ei-warning-filled /></el-icon>{{ $t('mcp:tokenOnce') }}</p>
        <div class="mcp-snip">
          <strong>{{ $t('mcp:token') }}</strong>
          <pre>{{ created.token }}</pre>
          <el-button size="small" @click="copy(created.token)">{{ $t('mcp:copy') }}</el-button>
        </div>
        <McpClientSetup :url="status!.url" :token="created.token" @copy="(x) => copy(x)" />
      </template>
      <template #footer>
        <el-button type="primary" @click="created = null">{{ $t('mcp:done') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.mcp h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.mcp h4 { margin: 20px 0 8px; font-size: 13px; color: var(--nm-text-strong); }
.mcp-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.mcp-small { font-size: 12px; }
.mcp-warn {
  display: flex; align-items: flex-start; gap: 8px; padding: 8px 12px; margin: 4px 0 6px; border-radius: 6px; font-size: 12.5px; line-height: 1.5;
  border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 8%, transparent);
}
.mcp-warn .el-icon { color: var(--nm-warning); flex: none; margin-top: 2px; }
.mcp-row { display: flex; align-items: center; justify-content: space-between; gap: 20px; padding: 12px 0; border-bottom: 1px solid var(--nm-border); }
.mcp-row > div { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
.mcp-row strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 500; }
.mcp-row span { font-size: 12px; color: var(--nm-text-dim); }
.mcp-row span.mcp-ok, .mcp-ok { color: var(--nm-success); }
.mcp-row span.mcp-err, .mcp-err { color: var(--nm-danger); }
.mcp-inline { display: flex; gap: 8px; margin-bottom: 10px; }
.mcp-inline :deep(.el-select) { width: 200px; }
.mcp-client { display: flex; align-items: center; gap: 10px; padding: 6px 0; border-bottom: 1px solid var(--nm-border); font-size: 12.5px; }
.mcp-client-text { display: flex; flex-direction: column; flex: 1; min-width: 0; }
.mcp-client-text strong { font-weight: 500; color: var(--nm-text-strong); }
.mcp-client-text span { font-size: 11.5px; color: var(--nm-text-dim); }
.mcp-client-text span.mcp-approve-all { display: flex; align-items: center; gap: 4px; color: var(--nm-warning); }
.mcp-log { width: 100%; }
.mcp-snip { display: grid; grid-template-columns: 1fr auto; gap: 2px 10px; margin-top: 12px; }
.mcp-snip strong { font-size: 12.5px; color: var(--nm-text-strong); }
.mcp-snip span { grid-column: 1; font-size: 11.5px; color: var(--nm-text-dim); }
.mcp-snip pre {
  grid-column: 1; margin: 4px 0 0; padding: 8px 10px; border-radius: 4px; border: 1px solid var(--nm-border);
  font: 12px var(--nm-font-mono, monospace); white-space: pre-wrap; word-break: break-all; user-select: text; cursor: text;
}
.mcp-snip .el-button { grid-column: 2; grid-row: 1 / span 3; align-self: end; }
</style>
