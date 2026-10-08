<script setup lang="ts">
import { computed, ref } from 'vue';
import {
  SNIPPET_FAMILIES, SNIPPETS_SETTING, builtInSnippets, type SnippetFamily, type UserSnippet,
} from '../composables/snippets';
import { useSettingsStore } from '../stores/settings';

// Configuración › Snippets: the user's own (abbreviation, engines, body with
// `${1:texto}` tab stops), saved in the settings, and the built-in sets to
// look at.

const settings = useSettingsStore();
const list = ref<UserSnippet[]>(settings.get<UserSnippet[]>(SNIPPETS_SETTING, []).map((s) => ({ ...s })));

/** Saved a moment after the last keystroke. */
let timer: ReturnType<typeof setTimeout> | null = null;
function save() {
  if (timer) clearTimeout(timer);
  timer = setTimeout(() => settings.set(SNIPPETS_SETTING, list.value.map((s) => ({ ...s }))), 400);
}
function add() {
  list.value.push({ abbr: '', family: 'all', body: '' });
  save();
}
function remove(i: number) {
  list.value.splice(i, 1);
  save();
}

const families = ['all', 'sql', ...SNIPPET_FAMILIES.filter((f) => f !== 'sql')] as const;
const duplicated = computed(() => {
  const seen = new Map<string, number>();
  for (const s of list.value) {
    const k = `${s.family}:${s.abbr.trim().toLowerCase()}`;
    seen.set(k, (seen.get(k) ?? 0) + 1);
  }
  return (s: UserSnippet) => !!s.abbr.trim() && (seen.get(`${s.family}:${s.abbr.trim().toLowerCase()}`) ?? 0) > 1;
});
const badAbbr = (s: UserSnippet) => !!s.abbr && !/^\w+$/.test(s.abbr.trim());

const shown = ref<SnippetFamily>('postgres');
const builtIns = computed(() => builtInSnippets(shown.value));
</script>

<template>
  <div class="sn">
    <h3>{{ $t('snippets:nav') }}</h3>
    <p class="sn-muted">{{ $t('snippets:help') }}</p>

    <div class="sn-head">
      <h4>{{ $t('snippets:mine') }}</h4>
      <el-button size="small" @click="add"><el-icon><ei-plus /></el-icon>&nbsp;{{ $t('snippets:add') }}</el-button>
    </div>
    <p v-if="!list.length" class="sn-muted">{{ $t('snippets:empty') }}</p>
    <div v-for="(s, i) in list" :key="i" class="sn-item">
      <div class="sn-line">
        <el-input v-model="s.abbr" size="small" class="sn-abbr" :placeholder="$t('snippets:abbr')" :aria-label="$t('snippets:abbr')" @input="save" />
        <el-select v-model="s.family" size="small" class="sn-fam" :aria-label="$t('snippets:family')" @change="save">
          <el-option v-for="f in families" :key="f" :value="f" :label="$t(`snippets:families.${f}`)" />
        </el-select>
        <el-input v-model="s.description" size="small" class="sn-desc" :placeholder="$t('snippets:description')" :aria-label="$t('snippets:description')" @input="save" />
        <el-button link size="small" :aria-label="$t('snippets:remove')" :title="$t('snippets:remove')" @click="remove(i)"><el-icon><ei-delete /></el-icon></el-button>
      </div>
      <el-input
        v-model="s.body"
        type="textarea"
        :autosize="{ minRows: 2, maxRows: 10 }"
        class="sn-body"
        :placeholder="$t('snippets:bodyPlaceholder')"
        :aria-label="$t('snippets:body')"
        @input="save"
      />
      <div v-if="badAbbr(s)" class="sn-warn">{{ $t('snippets:badAbbr') }}</div>
      <div v-else-if="duplicated(s)" class="sn-warn">{{ $t('snippets:duplicated') }}</div>
    </div>

    <div class="sn-head">
      <h4>{{ $t('snippets:builtIn') }}</h4>
      <el-select v-model="shown" size="small" class="sn-fam" :aria-label="$t('snippets:family')">
        <el-option v-for="f in SNIPPET_FAMILIES" :key="f" :value="f" :label="$t(`snippets:families.${f}`)" />
      </el-select>
    </div>
    <p class="sn-muted">{{ $t('snippets:builtInHelp') }}</p>
    <table class="sn-table">
      <tbody>
        <tr v-for="b in builtIns" :key="b.abbr">
          <td><code>{{ b.abbr }}</code></td>
          <td class="sn-detail">{{ b.detail }}</td>
        </tr>
      </tbody>
    </table>
  </div>
</template>

<style scoped>
.sn h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.sn h4 { margin: 0; font-size: 13px; color: var(--nm-text-strong); }
.sn-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.sn-head { display: flex; align-items: center; justify-content: space-between; margin: 18px 0 8px; }
.sn-item { display: flex; flex-direction: column; gap: 6px; padding: 8px 0; border-bottom: 1px solid var(--nm-border-soft); }
.sn-line { display: flex; align-items: center; gap: 6px; }
.sn-abbr { width: 110px; flex: none; }
.sn-fam { width: 190px; flex: none; }
.sn-desc { flex: 1; min-width: 0; }
.sn-body :deep(textarea) { font-family: var(--nm-mono); font-size: 12px; }
.sn-warn { font-size: 11.5px; color: var(--nm-warning); }
.sn-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.sn-table td { padding: 3px 6px; border-bottom: 1px solid var(--nm-border-soft); vertical-align: top; }
.sn-table code { font-family: var(--nm-mono); color: var(--nm-text-strong); }
.sn-detail { color: var(--nm-text-dim); font-family: var(--nm-mono); }
</style>
