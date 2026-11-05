<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { locale } from '../i18n';
import { contextSaves } from '../composables/contexts';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type ContextTab } from '../stores/tabs';

// A context of the AI assistant ("Contextos" node): its text, saved as it's
// written. It's guidance for the model; the limits stay in DBine's code.

const props = defineProps<{ tab: ContextTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();

const saved = computed(() => conns.contextById(props.tab.contextId));
const body = ref(saved.value?.body ?? '');
const scope = computed(() => [conns.byId(props.tab.connectionId)?.name, saved.value?.database || t('ai:contexts.wholeConnection')].join(' › '));
const length = computed(() => [...body.value].length);

// A tab restored after its context was deleted goes away.
onMounted(async () => {
  await conns.loadContexts(props.tab.connectionId);
  if (conns.contexts[props.tab.connectionId]?.status === 'ready' && !saved.value) {
    useTabsStore().closeWhere((x) => x.id === props.tab.id);
  }
});

let timer: ReturnType<typeof setTimeout> | undefined;
let dirty = false;
async function flush() {
  clearTimeout(timer);
  const x = saved.value;
  if (!dirty || !x) return;
  dirty = false;
  const save = conns.saveContext({ ...x, body: body.value });
  contextSaves.set(x.id, save.catch(() => undefined));
  try { await save; } catch (e) { ElMessage.error(errorMessage(e)); }
}
function onInput(v: string) {
  body.value = v;
  dirty = true;
  clearTimeout(timer);
  timer = setTimeout(flush, 500);
}
onBeforeUnmount(flush);

// Loaded after the tab opened, or changed from another window.
watch(() => saved.value?.body, (v) => { if (v !== undefined && !dirty) body.value = v; });

async function setActive(active: boolean) {
  const x = saved.value;
  if (!x) return;
  try { await conns.saveContext({ ...x, body: body.value, active }); dirty = false; } catch (e) { ElMessage.error(errorMessage(e)); }
}
</script>

<template>
  <div class="cx">
    <header class="cx-head">
      <div class="cx-title">
        <strong>{{ saved?.name }}</strong>
        <span>{{ scope }}</span>
      </div>
      <el-switch
        :model-value="saved?.active ?? false"
        :disabled="!saved"
        :active-text="$t('ai:contexts.active')"
        @update:model-value="setActive(!!$event)"
      />
    </header>
    <el-input
      class="cx-body"
      type="textarea"
      :model-value="body"
      :disabled="!saved"
      :placeholder="$t('ai:contexts.placeholder')"
      :input-style="{ height: '100%', resize: 'none', fontFamily: 'var(--nm-mono)' }"
      @update:model-value="onInput"
    />
    <footer class="cx-foot">
      <p>{{ $t('ai:contexts.guidance') }}</p>
      <p>{{ $t('ai:contexts.sharing') }}</p>
      <p>{{ $t('ai:contexts.size', { count: length, shown: length.toLocaleString(locale()) }) }}</p>
    </footer>
  </div>
</template>

<style scoped>
.cx { display: flex; flex-direction: column; height: 100%; gap: 10px; padding: 14px 18px; background: var(--nm-bg); }
.cx-head { display: flex; align-items: center; justify-content: space-between; gap: 16px; }
.cx-title { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.cx-title strong { color: var(--nm-text-strong); font-weight: 500; }
.cx-title span { font-size: 12px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cx-body { flex: 1; min-height: 0; }
.cx-body :deep(.el-textarea__inner) { height: 100%; }
.cx-foot { font-size: 12px; color: var(--nm-text-dim); }
.cx-foot p { margin: 2px 0; }
</style>
