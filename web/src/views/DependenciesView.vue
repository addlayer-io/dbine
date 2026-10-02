<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { Confidence, Dependent, DependencyReport, Relation } from '../api/types';
import { tb } from '../i18n/backend';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type DependenciesTab } from '../stores/tabs';

// "Dependencias · <objeto>": what depends on a table, column, view or
// routine. Foreign keys, indexes and checks come from the catalog; views,
// routines and triggers from searching their source (probable, or "revisar"
// when the name only shows inside dynamic SQL).

const props = defineProps<{ tab: DependenciesTab }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();

const report = ref<DependencyReport | null>(null);
const loading = ref(false);
const error = ref<string | null>(null);

const qualified = computed(() => {
  const o = props.tab.object;
  const name = o.schema ? `${o.schema}.${o.name}` : o.name;
  return props.tab.column ? `${name}.${props.tab.column}` : name;
});

async function load() {
  loading.value = true;
  error.value = null;
  try {
    report.value = await api.getDependents(props.tab.connectionId, props.tab.database, { object: props.tab.object, column: props.tab.column });
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);

const ORDER: Relation[] = ['foreign_key', 'index', 'check', 'code'];
const groups = computed(() =>
  ORDER.map((relation) => ({ relation, items: (report.value?.items ?? []).filter((d) => d.relation === relation) })).filter((g) => g.items.length),
);
const counts = computed(() => {
  const c: Record<Confidence, number> = { confirmed: 0, probable: 0, review: 0 };
  for (const d of report.value?.items ?? []) c[d.confidence]++;
  return c;
});

const fullName = (d: Dependent) => (d.schema ? `${d.schema}.${d.name}` : d.name);
// Views, routines and triggers open their source; a key, index or check
// opens its table's structure.
function open(d: Dependent) {
  const object = { kind: d.kind, schema: d.schema, name: d.name };
  tabs.openObject(props.tab.connectionId, props.tab.database, object, d.relation === 'code' ? 'definition' : 'structure');
}

async function copy() {
  const lines = (report.value?.items ?? []).map((d) =>
    [t(`dependencies:relation.${d.relation}`), d.kind, fullName(d), t(`dependencies:confidence.${d.confidence}`), d.detail ?? d.mentions.map((m) => `${m.line}: ${m.text}`).join(' | ')].join('\t'),
  );
  await navigator.clipboard.writeText(lines.join('\n'));
  ElMessage.success(t('dependencies:copied'));
}
</script>

<template>
  <div class="dp" v-loading="loading && !report">
    <header class="dp-head">
      <h2>{{ $t('dependencies:tab') }} · {{ qualified }}</h2>
      <span class="dp-dim">{{ tab.database || conns.byId(tab.connectionId)?.name }}</span>
      <span style="flex: 1" />
      <el-button size="small" :loading="loading" @click="load">
        <el-icon><ei-refresh /></el-icon><span>{{ $t('dependencies:refresh') }}</span>
      </el-button>
      <el-button size="small" :disabled="!report?.items.length" @click="copy">
        <el-icon><ei-document-copy /></el-icon><span>{{ $t('dependencies:copy') }}</span>
      </el-button>
    </header>
    <div v-if="error" class="dp-error">{{ error }}</div>
    <template v-else-if="report">
      <p class="dp-note">
        {{ $t('dependencies:summary', { confirmed: counts.confirmed, probable: counts.probable, review: counts.review, scanned: report.scanned }) }}
        <br>{{ $t('dependencies:outside') }}
      </p>
      <p v-if="report.note" class="dp-note warn">{{ tb(report.note) }}</p>
      <p v-if="report.unreadable.length" class="dp-note warn">{{ $t('dependencies:unreadable', { names: report.unreadable.join(', ') }) }}</p>
      <div class="dp-body">
        <p v-if="!groups.length" class="dp-dim">{{ $t('dependencies:none') }}</p>
        <section v-for="g in groups" :key="g.relation">
          <h3>{{ $t(`dependencies:relation.${g.relation}`) }} <span class="dp-dim">{{ g.items.length }}</span></h3>
          <div v-for="d in g.items" :key="`${d.kind}:${fullName(d)}:${d.detail ?? ''}`" class="dp-item">
            <div class="dp-line">
              <span class="dp-tag">{{ $t(`dependencies:kind.${d.kind}`, { defaultValue: d.kind }) }}</span>
              <a class="dp-name" @click="open(d)">{{ fullName(d) }}</a>
              <span v-if="d.parent" class="dp-dim">{{ $t('dependencies:on', { parent: d.parent }) }}</span>
              <span class="dp-badge" :class="d.confidence" :title="$t(`dependencies:confidenceTip.${d.confidence}`)">{{ $t(`dependencies:confidence.${d.confidence}`) }}</span>
            </div>
            <div v-if="d.detail" class="dp-detail nm-selectable">{{ d.detail }}</div>
            <div v-for="m in d.mentions" :key="m.line" class="dp-mention nm-selectable">
              <span class="dp-ln">{{ m.line }}</span><code>{{ m.text }}</code>
              <span v-if="m.dynamic" class="dp-dyn">{{ $t('dependencies:dynamic') }}</span>
            </div>
          </div>
        </section>
      </div>
    </template>
  </div>
</template>

<style scoped>
.dp { height: 100%; min-height: 0; display: flex; flex-direction: column; padding: 16px 22px 16px; background: var(--ide-editor); }
.dp-head { display: flex; align-items: center; gap: 8px; }
.dp-head h2 { margin: 0; font-size: 17px; color: var(--nm-text-strong); }
.dp-head .el-button { margin: 0; }
.dp-head .el-button .el-icon + span { margin-left: 4px; }
.dp-dim { color: var(--nm-text-dim); font-size: 12px; }
.dp-note { margin: 10px 0 0; font-size: 12px; color: var(--nm-text-dim); }
.dp-note.warn { color: var(--nm-text); }
.dp-error { margin: 10px 0; padding: 8px 10px; border-radius: 3px; border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); color: var(--nm-text); white-space: pre-wrap; font-size: 12px; }
.dp-body { flex: 1; min-height: 0; overflow: auto; margin-top: 12px; }
.dp-body h3 { margin: 14px 0 6px; font-size: 13px; font-weight: 600; color: var(--nm-text-strong); }
.dp-item { padding: 6px 0; border-bottom: 1px solid var(--nm-border-soft); font-size: 12.5px; }
.dp-line { display: flex; align-items: center; gap: 8px; }
.dp-tag { font-size: 10.5px; padding: 0 5px; border-radius: 3px; border: 1px solid var(--nm-border); color: var(--nm-text-dim); }
.dp-name { font-weight: 500; color: var(--nm-text-strong); cursor: pointer; }
.dp-name:hover { text-decoration: underline; }
.dp-badge { font-size: 10.5px; padding: 0 6px; border-radius: 8px; }
.dp-badge.confirmed { background: color-mix(in srgb, var(--nm-success) 20%, transparent); color: var(--nm-success); }
.dp-badge.probable { background: color-mix(in srgb, var(--nm-accent) 18%, transparent); color: var(--nm-text); }
.dp-badge.review { background: color-mix(in srgb, var(--nm-warning) 22%, transparent); color: var(--nm-warning); }
.dp-detail { margin: 3px 0 0 2px; color: var(--nm-text-dim); font-family: var(--nm-mono, monospace); font-size: 12px; }
.dp-mention { display: flex; align-items: baseline; gap: 8px; margin: 2px 0 0 2px; }
.dp-ln { min-width: 34px; text-align: right; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; font-size: 11.5px; }
.dp-mention code { white-space: pre; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text); }
.dp-dyn { font-size: 10.5px; color: var(--nm-warning); }
</style>
