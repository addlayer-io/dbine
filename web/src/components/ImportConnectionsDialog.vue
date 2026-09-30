<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { open } from '@tauri-apps/plugin-dialog';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import { errorMessage } from '../api/client';
import { useConnectionsStore } from '../stores/connections';
import { tagColor } from '../composables/tags';

// Brings connections from other tools (DBeaver, DbGate, DataGrip, Azure Data
// Studio, SSMS) or pasted URLs: reads their own files, lists what's there and
// saves the chosen ones. Passwords never reach the UI: the backend moves them
// from the other tool's file or keychain entry to DBine's keychain.

type Source = 'dbeaver' | 'dbgate' | 'datagrip' | 'azure_data_studio' | 'ssms' | 'url';
interface Item {
  key: string; name: string; folder: string[]; color: string | null; source_kind: string;
  driver: string; host: string; port: number; database: string; username: string | null;
  has_secret: boolean; keychain: boolean; tags: string[]; notes: string[]; unsupported: string | null; existing: string | null;
}
interface Scan { path: string; items: Item[]; warnings: string[] }

const props = defineProps<{ initialSource?: Source | null }>();
const emit = defineEmits<{ close: [] }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

const SOURCES = computed<{ id: Source; label: string; hint?: string }[]>(() => [
  { id: 'dbeaver', label: 'DBeaver' },
  { id: 'dbgate', label: 'DbGate' },
  { id: 'datagrip', label: 'DataGrip / JetBrains' },
  { id: 'azure_data_studio', label: 'Azure Data Studio' },
  { id: 'ssms', label: 'SSMS', hint: t('importConnections:source.ssmsHint') },
  { id: 'url', label: t('importConnections:source.pasteUrls'), hint: 'postgres://…, jdbc:…, Server=…;Database=…' },
]);
const detected = ref<Partial<Record<Source, string | null>>>({});
const urlText = ref('');
const source = ref<Source>(props.initialSource ?? 'dbeaver');
const path = ref<string | null>(null);
const scan = ref<Scan | null>(null);
const error = ref<string | null>(null);
const loading = ref(false);
const importing = ref(false);
const picked = ref<Set<string>>(new Set());
const passwords = ref(true);

const driverName = (id: string) => conns.drivers.find((d) => d.id === id)?.name ?? id;
const where = (i: Item) => {
  if (!i.host && !i.database) return '';
  const port = i.port ? `:${i.port}` : '';
  return `${i.host}${port}${i.database ? ` / ${i.database}` : ''}`;
};

async function detect() {
  try {
    detected.value = await invoke<Partial<Record<Source, string | null>>>('import_connections_detect');
    const first = SOURCES.value.find((s) => detected.value[s.id]);
    if (!props.initialSource && first && first.id !== source.value) { source.value = first.id; return; }
  } catch { /* the scan says what's missing */ }
  load();
}

async function load() {
  if (source.value === 'url' && !urlText.value.trim()) { scan.value = null; error.value = null; return; }
  loading.value = true;
  error.value = null;
  scan.value = null;
  try {
    const r = await invoke<Scan>('import_connections_scan', { args: { source: source.value, path: path.value, text: source.value === 'url' ? urlText.value : null } });
    scan.value = r;
    picked.value = new Set(r.items.filter((i) => !i.unsupported && !i.existing).map((i) => i.key));
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}

watch(source, () => { path.value = null; load(); });

async function choose(directory: boolean) {
  const r = await open({
    directory,
    multiple: false,
    title: directory ? t('importConnections:dialogFolder') : t('importConnections:dialogFile'),
    filters: directory ? undefined : [{ name: source.value === 'dbgate' ? 'connections.jsonl' : 'data-sources.json', extensions: ['json', 'jsonl'] }],
  });
  if (typeof r === 'string') {
    path.value = r;
    load();
  }
}

const importable = computed(() => scan.value?.items.filter((i) => !i.unsupported) ?? []);
const allPicked = computed(() => importable.value.length > 0 && importable.value.every((i) => picked.value.has(i.key)));
function toggleAll(v: boolean) {
  picked.value = new Set(v ? importable.value.map((i) => i.key) : []);
}
function toggle(i: Item, v: boolean) {
  const next = new Set(picked.value);
  if (v) next.add(i.key); else next.delete(i.key);
  picked.value = next;
}

async function doImport() {
  if (!scan.value || !picked.value.size) return;
  importing.value = true;
  try {
    const r = await invoke<{ imported: number; folders: number; failed: [string, string][] }>('import_connections_apply', {
      args: {
        source: source.value, path: source.value === 'url' ? null : scan.value.path,
        text: source.value === 'url' ? urlText.value : null, keys: [...picked.value], passwords: passwords.value,
      },
    });
    await conns.load();
    const folders = r.folders ? t('importConnections:andFolders', { count: r.folders }) : '';
    ElMessage.success(`${t('importConnections:imported', { count: r.imported })}${folders}`);
    if (r.failed.length) {
      await ElMessageBox.alert(r.failed.map(([n, e]) => `${n}: ${tb(e)}`).join('\n'), t('importConnections:failedTitle'), { confirmButtonText: t('importConnections:gotIt') }).catch(() => {});
    }
    emit('close');
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    importing.value = false;
  }
}

detect();
</script>

<template>
  <el-dialog :model-value="true" :title="$t('importConnections:title')" width="880px" top="8vh" append-to-body @close="emit('close')">
    <div class="ic">
      <div class="ic-sources">
        <button v-for="s in SOURCES" :key="s.id" class="ic-source" :class="{ on: source === s.id }" @click="source = s.id">
          <strong>{{ s.label }}</strong>
          <span>{{ s.hint ?? (detected[s.id] ? $t('importConnections:source.found') : $t('importConnections:source.notFound')) }}</span>
        </button>
      </div>

      <div v-if="source === 'url'" class="ic-urls">
        <el-input
          v-model="urlText"
          type="textarea"
          :rows="4"
          spellcheck="false"
          :placeholder="$t('importConnections:urlsPlaceholder')"
        />
        <el-button :disabled="!urlText.trim()" :loading="loading" @click="load">{{ $t('importConnections:read') }}</el-button>
      </div>
      <div v-else class="ic-path">
        <span class="ic-path-text" :title="scan?.path ?? path ?? ''">{{ scan?.path ?? path ?? detected[source] ?? '—' }}</span>
        <el-button size="small" @click="choose(true)">{{ $t('importConnections:chooseFolder') }}</el-button>
        <el-button size="small" @click="choose(false)">{{ $t('importConnections:chooseFile') }}</el-button>
      </div>

      <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon />
      <el-alert v-for="w in scan?.warnings ?? []" :key="w" type="warning" :title="tb(w)" :closable="false" show-icon />

      <div v-if="loading" class="ic-empty"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('importConnections:reading') }}</div>
      <div v-else-if="scan && !scan.items.length" class="ic-empty">{{ source === 'url' ? $t('importConnections:noneInText') : $t('importConnections:noneAtPath') }}</div>
      <div v-else-if="scan" class="ic-table">
        <div class="ic-row ic-head">
          <el-checkbox :model-value="allPicked" :indeterminate="picked.size > 0 && !allPicked" @change="(v: any) => toggleAll(!!v)" />
          <span>{{ $t('importConnections:columns.name') }}</span><span>{{ $t('importConnections:columns.engine') }}</span><span>{{ $t('importConnections:columns.server') }}</span><span>{{ $t('importConnections:columns.user') }}</span><span />
        </div>
        <div v-for="i in scan.items" :key="i.key" class="ic-row" :class="{ off: !!i.unsupported }">
          <el-checkbox :model-value="picked.has(i.key)" :disabled="!!i.unsupported" @change="(v: any) => toggle(i, !!v)" />
          <span class="ic-name">
            <span v-if="i.color" class="ic-dot" :style="{ background: i.color }" />
            <span class="ic-ellipsis" :title="i.name">{{ i.name }}</span>
            <span v-for="tag in i.tags" :key="tag" class="ic-tag" :style="{ color: tagColor(tag), borderColor: tagColor(tag) }">{{ tag }}</span>
            <span v-if="i.folder.length" class="ic-folder" :title="i.folder.join(' / ')"><el-icon><ei-folder /></el-icon>{{ i.folder.join(' / ') }}</span>
          </span>
          <span class="ic-ellipsis" :title="i.source_kind">{{ i.driver ? driverName(i.driver) : i.source_kind }}</span>
          <span class="ic-ellipsis nm-muted" :title="where(i)">{{ where(i) }}</span>
          <span class="ic-ellipsis nm-muted">
            {{ i.username ?? '' }}
            <el-icon v-if="i.has_secret" class="ic-key" :title="$t('importConnections:hasSecret')"><ei-key /></el-icon>
            <el-icon v-else-if="i.keychain" class="ic-key" :title="$t('importConnections:inKeychain')"><ei-key /></el-icon>
          </span>
          <span class="ic-status">
            <span v-if="i.unsupported" class="ic-bad" :title="tb(i.unsupported)">{{ tb(i.unsupported) }}</span>
            <template v-else>
              <span v-if="i.existing" class="ic-warn" :title="$t('importConnections:existsTitle', { name: i.existing })">{{ $t('importConnections:exists') }}</span>
              <el-tooltip v-if="i.notes.length" placement="top" :content="i.notes.map(tb).join(' ')">
                <el-icon class="ic-note"><ei-info-filled /></el-icon>
              </el-tooltip>
            </template>
          </span>
        </div>
      </div>
    </div>

    <template #footer>
      <div class="ic-footer">
        <el-checkbox v-model="passwords">{{ $t('importConnections:passwords') }}</el-checkbox>
        <div class="nm-spacer" />
        <el-button @click="emit('close')">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :disabled="!picked.size" :loading="importing" @click="doImport">
          {{ $t('importConnections:importN', { n: picked.size || '' }) }}
        </el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.ic { display: flex; flex-direction: column; gap: 10px; min-height: 360px; }
.ic-sources { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 8px; }
.ic-urls { display: flex; align-items: flex-end; gap: 8px; }
.ic-urls :deep(textarea) { font-family: var(--nm-mono); font-size: 12px; }
.ic-source {
  display: flex; flex-direction: column; align-items: flex-start; gap: 2px; padding: 8px 12px; min-width: 0;
  border: 1px solid var(--nm-border); border-radius: 6px; background: transparent; color: var(--nm-text); cursor: pointer; text-align: left;
}
.ic-source span { max-width: 100%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; color: var(--nm-text-muted); }
.ic-source.on { border-color: var(--ide-focus); background: color-mix(in srgb, var(--ide-focus) 12%, transparent); }
.ic-path { display: flex; align-items: center; gap: 8px; }
.ic-path-text { flex: 1; min-width: 0; font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ic-empty { display: flex; align-items: center; justify-content: center; gap: 6px; padding: 40px; color: var(--nm-text-muted); }
.ic-table { max-height: 52vh; overflow-y: auto; overflow-x: hidden; border: 1px solid var(--nm-border); border-radius: 6px; }
.ic-row {
  display: grid; grid-template-columns: 28px minmax(0, 2.2fr) minmax(0, 1fr) minmax(0, 1.8fr) minmax(0, 1fr) minmax(0, 1.2fr);
  align-items: center; gap: 8px; padding: 3px 10px; font-size: 12.5px; border-bottom: 1px solid var(--nm-border-soft);
}
.ic-row:last-child { border-bottom: none; }
.ic-row :deep(.el-checkbox) { margin-right: 0; height: 22px; }
.ic-head { position: sticky; top: 0; z-index: 1; background: var(--ide-sidebar); font-weight: 600; color: var(--nm-text-strong); }
.ic-row.off { opacity: 0.55; }
.ic-name { display: flex; align-items: center; gap: 6px; min-width: 0; }
.ic-dot { width: 8px; height: 8px; border-radius: 50%; flex: none; }
.ic-ellipsis { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; min-width: 0; }
.ic-folder { display: inline-flex; align-items: center; gap: 3px; flex: none; max-width: 45%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11px; color: var(--nm-text-muted); }
.ic-tag { flex: none; padding: 0 5px; border: 1px solid; border-radius: 8px; font-size: 10.5px; line-height: 15px; }
.ic-key { vertical-align: -2px; color: #d7ba7d; }
.ic-status { display: flex; align-items: center; gap: 6px; min-width: 0; font-size: 12px; }
.ic-bad { color: var(--nm-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.ic-warn { color: var(--nm-warning); white-space: nowrap; }
.ic-note { color: var(--nm-text-muted); cursor: help; }
.ic-footer { display: flex; align-items: center; gap: 8px; }
</style>
