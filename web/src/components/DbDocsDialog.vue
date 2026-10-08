<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { save } from '@tauri-apps/plugin-dialog';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { dbDocsApi, defaultDocOptions, type DocFormat, type DocOptions, type DocsOutline } from '../api/dbDocs';
import { useConnectionsStore } from '../stores/connections';
import { dbDocsTarget, runDbDocs } from '../composables/dbDocs';

// "Documentar la base…" (docs/documentar-la-base.md): format, schemas, what
// to include (only what the database has) and the file. The run goes on as
// a background task (composables/dbDocs.ts) and the dialog closes.

const { t } = useTranslation();
const conns = useConnectionsStore();

const loading = ref(false);
const error = ref<string | null>(null);
const outline = ref<DocsOutline | null>(null);
const opts = ref<DocOptions>(defaultDocOptions());
const path = ref('');

const target = computed(() => dbDocsTarget.value);
const name = computed(() => target.value?.database || conns.byId(target.value?.connectionId ?? '')?.name || '');
const driver = computed(() => (target.value ? conns.driverOf(target.value.connectionId) : undefined));

type Part = 'tables' | 'views' | 'routines' | 'triggers' | 'others';
const VIEWS = ['view', 'materialized_view'];
const ROUTINES = ['procedure', 'function', 'package', 'package_body', 'aggregate'];
/** Where a kind goes (as `section_of` in src-tauri/src/dbdocs/mod.rs). */
function partOf(kind: string): Part {
  if (kind === 'table' || kind === 'collection') return 'tables';
  if (VIEWS.includes(kind)) return 'views';
  if (ROUTINES.includes(kind)) return 'routines';
  if (kind === 'trigger') return 'triggers';
  return driver.value?.object_kinds.find((k) => k.id === kind)?.has_columns ? 'tables' : 'others';
}
const present = computed(() => {
  const out = new Set<Part>();
  for (const [kind, n] of Object.entries(outline.value?.kinds ?? {})) if (n > 0) out.add(partOf(kind));
  return out;
});
const hasCode = computed(() => ['views', 'routines', 'triggers'].some((p) => present.value.has(p as Part)));

/** The checkboxes: what each needs from the database. */
const parts = computed(() => [
  { key: 'tables', ok: present.value.has('tables') },
  { key: 'views', ok: present.value.has('views') },
  { key: 'routines', ok: present.value.has('routines') },
  { key: 'triggers', ok: present.value.has('triggers') },
  { key: 'others', ok: present.value.has('others') },
  { key: 'source', ok: hasCode.value },
  { key: 'indexes', ok: present.value.has('tables') },
  { key: 'foreign_keys', ok: present.value.has('tables') && !!outline.value?.foreign_keys },
  { key: 'dependencies', ok: present.value.has('tables') && !!outline.value?.dependencies },
  { key: 'diagram', ok: present.value.has('tables') && opts.value.format === 'html' },
] as { key: keyof DocOptions; ok: boolean }[]);
const partLabel = (key: string) => t(`dbDocs:parts.${key === 'foreign_keys' ? 'foreignKeys' : key}`);

async function load() {
  const tg = target.value;
  if (!tg) return;
  loading.value = true;
  error.value = null;
  outline.value = null;
  opts.value = defaultDocOptions();
  path.value = '';
  try {
    outline.value = await dbDocsApi.outline(tg.connectionId, tg.database);
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
watch(target, (v) => { if (v) void load(); }, { immediate: true });

const ext = (f: DocFormat) => (f === 'html' ? 'html' : 'md');
watch(() => opts.value.format, (f) => {
  if (path.value) path.value = path.value.replace(/\.(html?|md|markdown)$/i, '') + `.${ext(f)}`;
});

async function browse() {
  const f = opts.value.format;
  const safe = name.value.replace(/[\\/:*?"<>|]+/g, '_') || 'base';
  try {
    const picked = await save({
      title: t('dbDocs:pickFile'),
      defaultPath: path.value || `${safe}.${ext(f)}`,
      filters: [{ name: t(`dbDocs:formats.${f}`), extensions: f === 'html' ? ['html', 'htm'] : ['md', 'markdown'] }],
    });
    if (picked) path.value = picked;
  } catch { /* no dialog */ }
}

async function generate() {
  const tg = target.value;
  if (!tg) return;
  if (!path.value.trim()) {
    await browse();
    if (!path.value.trim()) { ElMessage.info(t('dbDocs:noFile')); return; }
  }
  // Parts the database lacks go off: nothing to read for them.
  const options: DocOptions = { ...opts.value, labels: defaultDocOptions().labels };
  for (const p of parts.value) if (!p.ok) (options[p.key] as boolean) = false;
  runDbDocs(tg.connectionId, tg.database, path.value.trim(), options);
  ElMessage.info(t('dbDocs:started'));
  close();
}

function close() {
  dbDocsTarget.value = null;
}
</script>

<template>
  <el-dialog :model-value="!!target" :title="$t('dbDocs:title', { name })" width="580px" append-to-body @close="close">
    <div v-if="loading" class="dd-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon><span>{{ $t('dbDocs:loading') }}</span></div>
    <el-alert v-else-if="error" type="error" :title="error" :closable="false" show-icon />
    <el-form v-else-if="outline" label-position="top" size="small" @submit.prevent>
      <el-form-item :label="$t('dbDocs:format')">
        <el-radio-group v-model="opts.format">
          <el-radio value="html">{{ $t('dbDocs:formats.html') }}</el-radio>
          <el-radio value="markdown">{{ $t('dbDocs:formats.markdown') }}</el-radio>
        </el-radio-group>
        <p class="dd-hint">{{ $t(`dbDocs:formatHint.${opts.format}`) }}</p>
      </el-form-item>
      <el-form-item v-if="outline.schemas.length" :label="$t('dbDocs:schemas')">
        <el-select v-model="opts.schemas" multiple collapse-tags collapse-tags-tooltip filterable clearable :placeholder="$t('dbDocs:allSchemas')" class="dd-wide">
          <el-option v-for="s in outline.schemas" :key="s" :value="s" :label="s" />
        </el-select>
      </el-form-item>
      <el-form-item :label="$t('dbDocs:include')">
        <div class="dd-parts">
          <el-checkbox
            v-for="p in parts" :key="p.key" v-model="(opts[p.key] as boolean)" :disabled="!p.ok"
            :title="p.ok ? '' : $t('dbDocs:unavailable')"
          >{{ partLabel(p.key) }}</el-checkbox>
        </div>
      </el-form-item>
      <el-form-item :label="$t('dbDocs:destination')">
        <el-input v-model="path" class="dd-wide">
          <template #append><el-button @click="browse">{{ $t('dbDocs:browse') }}</el-button></template>
        </el-input>
      </el-form-item>
    </el-form>
    <template #footer>
      <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :disabled="!outline" @click="generate">{{ $t('dbDocs:generate') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.dd-empty { display: flex; align-items: center; gap: 8px; justify-content: center; padding: 30px 0; color: var(--nm-text-dim); }
.dd-hint { margin: 4px 0 0; font-size: 11.5px; line-height: 1.4; color: var(--nm-text-dim); width: 100%; }
.dd-wide { width: 100%; }
.dd-parts { display: grid; grid-template-columns: 1fr 1fr; gap: 0 16px; width: 100%; }
.dd-parts .el-checkbox { margin-right: 0; }
</style>
