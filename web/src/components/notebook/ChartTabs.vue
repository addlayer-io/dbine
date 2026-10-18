<script setup lang="ts">
import { nextTick, ref } from 'vue';
import { MAX_TITLE_CHARS } from '../../composables/chartSettings';

// A notebook cell's charts, next to the result's view selector: one small
// tab per chart (click: show it; double click or the menu: rename), "+" adds
// one. Titles are the user's; an empty one shows "Gráfico N".

const props = defineProps<{ titles: string[]; active: number }>();
const emit = defineEmits<{ select: [i: number]; add: []; rename: [i: number, title: string]; remove: [i: number] }>();

const editing = ref<number | null>(null);
const draft = ref('');
const input = ref<HTMLInputElement[] | null>(null);

function startRename(i: number) {
  editing.value = i;
  draft.value = props.titles[i] ?? '';
  void nextTick(() => { const el = input.value?.[0]; el?.focus(); el?.select(); });
}
function commit() {
  if (editing.value === null) return;
  const i = editing.value;
  editing.value = null;
  const next = draft.value.trim().slice(0, MAX_TITLE_CHARS);
  if (next !== (props.titles[i] ?? '')) emit('rename', i, next);
}
function onMenu(cmd: string, i: number) {
  if (cmd === 'rename') startRename(i);
  else if (cmd === 'remove') emit('remove', i);
}
</script>

<template>
  <div class="ct" role="tablist" :aria-label="$t('notebook:chart.tabs')">
    <template v-for="(title, i) in titles" :key="i">
      <input
        v-if="editing === i"
        ref="input"
        v-model="draft"
        class="ct-input"
        :maxlength="MAX_TITLE_CHARS"
        :placeholder="$t('notebook:chart.untitled', { n: i + 1 })"
        @keydown.enter.prevent="commit"
        @keydown.esc.prevent="editing = null"
        @blur="commit"
      >
      <div v-else class="ct-tab" :class="{ on: i === active }" role="tab" :aria-selected="i === active">
        <button class="ct-name" :title="$t('notebook:chart.renameTip')" @click="emit('select', i)" @dblclick="startRename(i)">
          {{ title || $t('notebook:chart.untitled', { n: i + 1 }) }}
        </button>
        <el-dropdown v-if="i === active" trigger="click" placement="bottom-start" @command="(c: string) => onMenu(c, i)">
          <button class="ct-more" :aria-label="$t('notebook:chart.menu')"><el-icon><ei-arrow-down /></el-icon></button>
          <template #dropdown>
            <el-dropdown-menu>
              <el-dropdown-item command="rename">{{ $t('notebook:chart.rename') }}</el-dropdown-item>
              <el-dropdown-item command="remove" :disabled="titles.length < 2">{{ $t('notebook:chart.remove') }}</el-dropdown-item>
            </el-dropdown-menu>
          </template>
        </el-dropdown>
      </div>
    </template>
    <button class="ct-add" :title="$t('notebook:chart.add')" :aria-label="$t('notebook:chart.add')" @click="emit('add')"><el-icon><ei-plus /></el-icon></button>
  </div>
</template>

<style scoped>
.ct { display: inline-flex; align-items: center; gap: 2px; min-width: 0; overflow-x: auto; scrollbar-width: none; align-self: center; }
.ct-tab {
  display: inline-flex; align-items: center; flex-shrink: 0; height: 20px; border: 1px solid transparent; border-radius: 3px; color: var(--nm-text-dim);
}
.ct-tab:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.ct-tab.on { border-color: var(--nm-border); background: var(--ide-selection); color: var(--nm-text-strong); }
.ct-name {
  max-width: 160px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; padding: 0 7px; border: 0; background: none;
  font: inherit; font-size: 11.5px; color: inherit; cursor: pointer;
}
.ct-tab.on .ct-name { padding-right: 2px; }
.ct-more { display: inline-flex; align-items: center; padding: 0 4px 0 0; border: 0; background: none; color: inherit; cursor: pointer; font-size: 10px; }
.ct-input {
  width: 140px; height: 20px; padding: 0 6px; border: 1px solid var(--nm-accent); border-radius: 3px; outline: none;
  background: var(--nm-bg); color: var(--nm-text-strong); font: inherit; font-size: 11.5px;
}
.ct-add {
  display: inline-flex; align-items: center; justify-content: center; flex-shrink: 0; width: 20px; height: 20px; border: 0; border-radius: 3px;
  background: none; color: var(--nm-text-dim); cursor: pointer;
}
.ct-add:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
</style>
