<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { notebookApi } from '../../api/notebook';
import type { FileGrant } from '../../api/types';
import { locale } from '../../i18n';
import { errorMessage } from '../../api/client';

// "Archivos que los notebooks pueden leer": what "Permitir siempre" stored
// on this machine (docs/notebooks.md › Reading local files › Approval).
// Every notebook's (Configuración › Notebooks) or one notebook's (its
// "Permisos de archivos…"); each row can be removed, or all at once.

const props = defineProps<{
  /** One notebook's file; none: every notebook's. */
  notebook?: string | null;
  /** The notebook is unsaved: it has no grants. */
  unsaved?: boolean;
}>();
const { t } = useTranslation();

const grants = ref<FileGrant[]>([]);
const loading = ref(false);
const error = ref<string | null>(null);
const single = computed(() => props.notebook !== undefined);

async function load() {
  if (props.unsaved) return;
  loading.value = true;
  error.value = null;
  try {
    grants.value = await notebookApi.fileGrants(props.notebook ?? null);
  } catch (e) {
    error.value = errorMessage(e);
  } finally {
    loading.value = false;
  }
}
onMounted(load);

const when = (iso: string) => {
  const d = new Date(iso);
  return Number.isNaN(d.getTime()) ? iso : d.toLocaleString(locale(), { day: '2-digit', month: '2-digit', year: 'numeric', hour: '2-digit', minute: '2-digit' });
};
/** The file name, then its folder. */
const nameOf = (p: string) => p.split(/[\\/]/).pop() ?? p;
const dirOf = (p: string) => p.slice(0, Math.max(0, p.length - nameOf(p).length - 1));

async function remove(g: FileGrant) {
  try {
    await notebookApi.removeFileGrant(g.notebook, g.target);
    grants.value = grants.value.filter((x) => !(x.notebook === g.notebook && x.target === g.target));
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
async function clearAll() {
  try {
    await ElMessageBox.confirm(t('notebook:fileGrants.clearConfirm', { count: grants.value.length }), t('notebook:fileGrants.clearAll'), {
      confirmButtonText: t('notebook:fileGrants.clearAll'), cancelButtonText: t('common:cancel'), type: 'warning',
    });
  } catch {
    return;
  }
  try {
    // One notebook's: its file as the list gave it (canonical).
    await notebookApi.clearFileGrants(single.value ? grants.value[0]?.notebook ?? props.notebook ?? null : null);
    grants.value = [];
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
defineExpose({ load });
</script>

<template>
  <div class="nfg">
    <div class="nfg-head">
      <div>
        <strong>{{ $t('notebook:fileGrants.title') }}</strong>
        <span>{{ single ? $t('notebook:fileGrants.helpOne') : $t('notebook:fileGrants.help') }}</span>
      </div>
      <el-button size="small" :disabled="!grants.length" @click="clearAll">{{ $t('notebook:fileGrants.clearAll') }}</el-button>
    </div>
    <el-alert v-if="error" type="error" :title="error" :closable="false" />
    <div v-else-if="unsaved" class="nfg-empty">{{ $t('notebook:fileGrants.unsaved') }}</div>
    <div v-else-if="!grants.length && !loading" class="nfg-empty">{{ $t('notebook:fileGrants.empty') }}</div>
    <div v-else v-loading="loading" class="nfg-list">
      <div v-for="g in grants" :key="`${g.notebook}\n${g.target}`" class="nfg-row">
        <div class="nfg-what">
          <div v-if="!single" class="nfg-nb" :title="g.notebook">
            <el-icon><ei-notebook /></el-icon><span>{{ nameOf(g.notebook) }}</span><span class="nfg-dim">{{ dirOf(g.notebook) }}</span>
          </div>
          <div class="nfg-target" :title="g.target">
            <span v-if="g.pattern" class="nfg-tag">{{ $t('notebook:fileAccess.pattern') }}</span>
            <code class="nm-selectable">{{ g.target }}</code>
          </div>
        </div>
        <span class="nfg-date">{{ $t('notebook:fileGrants.granted', { when: when(g.granted_at) }) }}</span>
        <el-button size="small" link :title="$t('notebook:fileGrants.remove')" :aria-label="$t('notebook:fileGrants.remove')" @click="remove(g)">
          <el-icon><ei-close /></el-icon>
        </el-button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.nfg-head { display: flex; align-items: center; justify-content: space-between; gap: 20px; padding: 12px 0 8px; }
.nfg-head > div { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
.nfg-head strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 500; }
.nfg-head span { font-size: 12px; color: var(--nm-text-dim); }
.nfg-empty { font-size: 12px; color: var(--nm-text-dim); padding: 10px 12px; border: 1px dashed var(--nm-border); border-radius: 4px; }
.nfg-list { border: 1px solid var(--nm-border); border-radius: 4px; max-height: 340px; overflow: auto; }
.nfg-row { display: flex; align-items: center; gap: 12px; padding: 7px 10px; }
.nfg-row + .nfg-row { border-top: 1px solid var(--nm-border); }
.nfg-what { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 3px; }
.nfg-nb { display: flex; align-items: center; gap: 6px; font-size: 12.5px; color: var(--nm-text-strong); min-width: 0; }
.nfg-nb span { white-space: nowrap; }
.nfg-nb .nfg-dim { color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; font-size: 11.5px; }
.nfg-target { display: flex; align-items: baseline; gap: 6px; min-width: 0; }
.nfg-target code { font-family: var(--nm-mono); font-size: 11.5px; color: var(--nm-text); overflow-wrap: anywhere; }
.nfg-tag { flex-shrink: 0; font-size: 10.5px; padding: 0 5px; border-radius: 3px; background: var(--nm-overlay-code); color: var(--nm-text-dim); }
.nfg-date { flex-shrink: 0; font-size: 11.5px; color: var(--nm-text-dim); white-space: nowrap; }
</style>
