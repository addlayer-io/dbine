<script setup lang="ts">
import { ref, watch } from 'vue';
import { invoke } from '@tauri-apps/api/core';
import { useSettingsStore } from '../stores/settings';

// First run: if other database tools with connections are on this machine,
// offer to import them. Shown once, whatever the answer (`import.suggested`,
// a preference that travels with the cloud backup).

type Source = 'dbeaver' | 'dbgate' | 'datagrip' | 'azure_data_studio' | 'ssms';
const LABELS: Record<Source, string> = {
  dbeaver: 'DBeaver',
  dbgate: 'DbGate',
  datagrip: 'DataGrip / JetBrains',
  azure_data_studio: 'Azure Data Studio',
  ssms: 'SQL Server Management Studio',
};
const KEY = 'import.suggested';

const emit = defineEmits<{ import: [source: Source] }>();
const settings = useSettingsStore();
const found = ref<{ source: Source; count: number }[]>([]);
const open = ref(false);

async function evaluate() {
  if (settings.get(KEY, false)) return;
  try {
    const detected = await invoke<Partial<Record<Source, string | null>>>('import_connections_detect');
    const out: { source: Source; count: number }[] = [];
    for (const source of Object.keys(LABELS) as Source[]) {
      if (!detected[source]) continue;
      try {
        const r = await invoke<{ items: { unsupported: string | null; existing: string | null }[] }>('import_connections_scan', { args: { source, path: null } });
        const count = r.items.filter((i) => !i.unsupported && !i.existing).length;
        if (count) out.push({ source, count });
      } catch { /* unreadable: not offered */ }
    }
    found.value = out;
    if (out.length) open.value = true;
    else settings.set(KEY, true);
  } catch { /* try again next start */ }
}

function answer(source: Source | null) {
  settings.set(KEY, true);
  open.value = false;
  if (source) emit('import', source);
}

watch(() => settings.loaded, (l) => { if (l) evaluate(); }, { immediate: true });
</script>

<template>
  <el-dialog :model-value="open" :title="$t('importConnections:suggestion.title')" width="460px" append-to-body :close-on-click-modal="false" @close="answer(null)">
    <p class="is-text">{{ $t('importConnections:suggestion.found') }}</p>
    <ul class="is-list">
      <li v-for="f in found" :key="f.source">
        <strong>{{ LABELS[f.source] }}</strong>
        <span>{{ $t('importConnections:suggestion.count', { count: f.count }) }}</span>
      </li>
    </ul>
    <p class="is-text is-muted">
      {{ $t('importConnections:suggestion.hintBefore') }} <el-icon class="is-inline"><ei-download /></el-icon> {{ $t('importConnections:suggestion.hintAfter') }}
    </p>
    <template #footer>
      <el-button @click="answer(null)">{{ $t('importConnections:suggestion.notNow') }}</el-button>
      <el-button type="primary" @click="answer(found[0].source)">
        {{ $t('importConnections:suggestion.viewAndImport') }}
      </el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.is-text { margin: 0 0 10px; line-height: 1.5; }
.is-muted { color: var(--nm-text-muted); font-size: 12.5px; margin: 10px 0 0; }
.is-list { margin: 0; padding: 0; list-style: none; border: 1px solid var(--nm-border); border-radius: 6px; }
.is-list li { display: flex; justify-content: space-between; padding: 7px 12px; }
.is-list li + li { border-top: 1px solid var(--nm-border-soft); }
.is-list span { color: var(--nm-text-muted); }
.is-inline { vertical-align: -2px; }
</style>
