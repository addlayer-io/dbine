<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { readJson, writeJson } from '../stores/storage';

// The search row at the top of a key database's "Claves" folder (Redis,
// etcd): a pattern searched on the server, and the type to keep. It lives
// inside the virtualized tree, so it keeps clicks and keys to itself.

const props = defineProps<{ connectionId: string; database: string }>();
const conns = useConnectionsStore();
const { t } = useTranslation();

const search = computed(() => conns.driverOf(props.connectionId)?.key_search ?? null);
const browse = computed(() => conns.keys[dbKey(props.connectionId, props.database)]);

const text = ref(browse.value?.pattern ?? '');
const keyType = ref(browse.value?.keyType ?? '');
// A refresh or a "search in this folder" from the menu changes the search.
watch(() => [browse.value?.pattern, browse.value?.keyType], ([p, kt]) => {
  text.value = p ?? '';
  keyType.value = kt ?? '';
});

const HISTORY = 'dbine.keySearches';
const MAX_HISTORY = 12;
const history = ref<string[]>(readJson<Record<string, string[]>>(HISTORY, {})[props.connectionId] ?? []);
const listId = computed(() => `ks-history-${props.connectionId}`);

function remember(pattern: string) {
  if (!pattern) return;
  const all = readJson<Record<string, string[]>>(HISTORY, {});
  const list = [pattern, ...(all[props.connectionId] ?? []).filter((p) => p !== pattern)].slice(0, MAX_HISTORY);
  all[props.connectionId] = list;
  writeJson(HISTORY, all);
  history.value = list;
}

function run() {
  const pattern = text.value.trim();
  remember(pattern);
  conns.searchKeys(props.connectionId, props.database, pattern, keyType.value);
}

function clear() {
  text.value = '';
  run();
}

const placeholder = computed(() =>
  search.value?.syntax === 'prefix' ? t('explorer:keySearch.placeholderPrefix') : t('explorer:keySearch.placeholderGlob'),
);
const help = computed(() => {
  const s = search.value;
  if (!s) return '';
  const lines = s.syntax === 'prefix'
    ? [t('explorer:keySearch.helpPrefix')]
    : [t('explorer:keySearch.helpWildcards'), t('explorer:keySearch.helpContains')];
  if (s.case_sensitive) lines.push(t('explorer:keySearch.helpCase'));
  lines.push(t('explorer:keySearch.helpEnter'));
  return lines.join('\n');
});
</script>

<template>
  <span
    v-if="search"
    class="ks"
    @click.stop
    @mousedown.stop
    @dblclick.stop
    @keydown.stop
  >
    <input
      :id="`ks-${connectionId}-${database}`"
      v-model="text"
      class="ks-input"
      type="text"
      spellcheck="false"
      autocomplete="off"
      :list="listId"
      :placeholder="placeholder"
      :title="help"
      @keydown.enter.prevent="run"
      @keydown.esc.prevent="clear"
    >
    <datalist :id="listId">
      <option v-for="h in history" :key="h" :value="h" />
    </datalist>
    <select
      v-if="search.types.length"
      :id="`ks-type-${connectionId}-${database}`"
      v-model="keyType"
      class="ks-type"
      :title="$t('explorer:keySearch.typeTitle')"
      @change="run"
    >
      <option value="">{{ $t('common:all') }}</option>
      <option v-for="ty in search.types" :key="ty" :value="ty">{{ ty }}</option>
    </select>
    <button v-if="browse?.pattern || browse?.keyType" class="ks-clear" :title="$t('explorer:keySearch.showAll')" @click="keyType = ''; clear()">×</button>
  </span>
</template>

<style scoped>
.ks { display: flex; align-items: center; gap: 4px; flex: 1; min-width: 0; padding-right: 6px; }
.ks-input, .ks-type {
  height: 18px; box-sizing: border-box; font: 12px/1 inherit; color: var(--nm-text-strong);
  background: var(--ide-input); border: 1px solid transparent; border-radius: 3px; outline: none;
}
.ks-input { flex: 1; min-width: 60px; padding: 0 6px; }
.ks-input::placeholder { color: var(--nm-text-muted); }
.ks-input:focus, .ks-type:focus { border-color: var(--ide-focus); }
.ks-type { flex: none; max-width: 76px; padding: 0 2px; }
.ks-clear {
  flex: none; width: 16px; height: 16px; padding: 0; border: 0; border-radius: 3px; cursor: pointer;
  background: none; color: var(--nm-text-dim); font-size: 14px; line-height: 16px;
}
.ks-clear:hover { color: var(--nm-text-strong); background: var(--ide-hover); }
</style>
