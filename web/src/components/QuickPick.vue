<script setup lang="ts">
import { computed, nextTick, ref, watch } from 'vue';
import { quickPick, type QuickPickItem } from '../composables/quickPick';

// The quick pick's box (composables/quickPick.ts): centered at the top,
// a filter (or a plain input) and the list. Click outside or Esc closes it.

const filter = ref('');
const activeIndex = ref(0);
const input = ref<HTMLInputElement | null>(null);
const list = ref<HTMLElement | null>(null);
/** Focus to give back when it closes (the status bar item). */
let returnFocus: HTMLElement | null = null;

const req = computed(() => quickPick.value);
const pick = computed(() => (req.value?.kind === 'pick' ? req.value.options : null));
const ask = computed(() => (req.value?.kind === 'input' ? req.value.options : null));

const shown = computed<QuickPickItem[]>(() => {
  const items = pick.value?.items() ?? [];
  const q = filter.value.trim().toLowerCase();
  return q ? items.filter((i) => i.label.toLowerCase().includes(q) || i.description?.toLowerCase().includes(q)) : items;
});
const busy = computed(() => !!pick.value?.busy?.());
const inputError = computed(() => (ask.value?.validate ? ask.value.validate(filter.value) : null));

watch(req, async (r) => {
  if (!r) return;
  if (!(document.activeElement instanceof HTMLElement && document.activeElement.closest('.qp'))) {
    returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
  }
  filter.value = r.kind === 'input' ? r.options.value ?? '' : '';
  activeIndex.value = r.kind === 'pick' ? Math.max(0, shown.value.findIndex((i) => i.current)) : 0;
  await nextTick();
  input.value?.focus();
  input.value?.select();
  scrollActive();
});
// Filtering moves the highlight to the first match (the current one before typing).
watch(filter, () => {
  if (!pick.value) return;
  const i = filter.value ? shown.value.findIndex((x) => !x.disabled) : shown.value.findIndex((x) => x.current);
  activeIndex.value = Math.max(0, i);
});
// A list that arrives while open: highlight the current one.
watch(() => shown.value.length, (n, before) => {
  if (!before && n && !filter.value) activeIndex.value = Math.max(0, shown.value.findIndex((x) => x.current));
});

function scrollActive() {
  void nextTick(() => list.value?.querySelector<HTMLElement>('.qp-item.active')?.scrollIntoView({ block: 'nearest' }));
}
function move(step: number) {
  const n = shown.value.length;
  if (!n) return;
  let i = activeIndex.value;
  for (let k = 0; k < n; k++) {
    i = (i + step + n) % n;
    if (!shown.value[i].disabled) break;
  }
  activeIndex.value = i;
  scrollActive();
}

function close(value: QuickPickItem | string | null) {
  const r = quickPick.value;
  if (!r) return;
  quickPick.value = null;
  if (r.kind === 'pick') r.resolve(value as QuickPickItem | null);
  else r.resolve(value as string | null);
  const back = returnFocus;
  returnFocus = null;
  // A follow-up quick pick (a custom value) keeps the focus inside.
  void nextTick(() => { if (!quickPick.value && back?.isConnected) back.focus(); });
}
function choose(item: QuickPickItem | undefined) {
  if (item && !item.disabled) close(item);
}
function accept() {
  if (pick.value) choose(shown.value[activeIndex.value]);
  else if (ask.value && !inputError.value) close(filter.value.trim());
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') { e.preventDefault(); e.stopPropagation(); close(null); }
  else if (e.key === 'Enter') { e.preventDefault(); accept(); }
  else if (pick.value && e.key === 'ArrowDown') { e.preventDefault(); move(1); }
  else if (pick.value && e.key === 'ArrowUp') { e.preventDefault(); move(-1); }
  else if (pick.value && (e.key === 'PageDown' || e.key === 'PageUp')) { e.preventDefault(); move(e.key === 'PageDown' ? 8 : -8); }
}
</script>

<template>
  <Teleport to="body">
    <div v-if="req" class="qp-backdrop" @pointerdown.self="close(null)">
      <div class="qp" role="dialog" :aria-label="pick?.placeholder ?? ask?.placeholder" @keydown="onKey">
        <input
          ref="input"
          v-model="filter"
          class="qp-input"
          :class="{ invalid: ask && inputError && filter }"
          :placeholder="pick?.placeholder ?? ask?.placeholder"
          :aria-label="pick?.placeholder ?? ask?.placeholder"
          :role="pick ? 'combobox' : undefined"
          :aria-expanded="pick ? true : undefined"
          aria-controls="qp-list"
          :aria-activedescendant="pick && shown[activeIndex] ? `qp-item-${activeIndex}` : undefined"
          spellcheck="false"
          autocomplete="off"
        >
        <div v-if="busy" class="qp-progress" />
        <template v-if="pick">
          <div id="qp-list" ref="list" class="qp-list" role="listbox">
            <div
              v-for="(item, i) in shown"
              :id="`qp-item-${i}`"
              :key="item.id"
              class="qp-item"
              :class="{ active: i === activeIndex, disabled: item.disabled }"
              role="option"
              :aria-selected="i === activeIndex"
              :aria-disabled="item.disabled || undefined"
              @pointermove="item.disabled || (activeIndex = i)"
              @click="choose(item)"
            >
              <span class="qp-check"><el-icon v-if="item.current"><ei-check /></el-icon></span>
              <span class="qp-label">{{ item.label }}</span>
              <span v-if="item.description" class="qp-desc">{{ item.description }}</span>
            </div>
            <div v-if="!shown.length && !busy" class="qp-empty">{{ pick.empty ?? $t('workbench:quickPick.noResults') }}</div>
          </div>
        </template>
        <div v-else-if="ask" class="qp-prompt" :class="{ invalid: inputError && filter }" role="status">
          {{ inputError && filter ? inputError : ask.prompt }}
        </div>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.qp-backdrop { position: fixed; inset: 0; z-index: 3000; }
.qp {
  position: absolute; top: 44px; left: 50%; transform: translateX(-50%);
  width: min(600px, calc(100vw - 32px)); padding: 6px 6px 4px; box-sizing: border-box;
  background: var(--ide-sidebar); color: var(--nm-text); border: 1px solid var(--nm-border); border-radius: 6px;
  box-shadow: 0 0 8px 2px rgba(0, 0, 0, 0.36); font-size: 13px;
}
.qp-input {
  width: 100%; box-sizing: border-box; height: 26px; padding: 0 6px; font: inherit; color: var(--nm-text-strong);
  background: var(--ide-input); border: 1px solid var(--ide-focus); border-radius: 2px; outline: none;
}
.qp-input.invalid { border-color: var(--nm-danger); }
.qp-progress { height: 2px; margin-top: 2px; background: linear-gradient(90deg, transparent, var(--ide-focus), transparent); background-size: 40% 100%; background-repeat: no-repeat; animation: qp-busy 1.2s linear infinite; }
@keyframes qp-busy { from { background-position: -40% 0; } to { background-position: 140% 0; } }
.qp-list { max-height: min(440px, 60vh); overflow-y: auto; margin-top: 4px; }
.qp-item {
  display: flex; align-items: center; gap: 6px; height: 22px; padding: 0 6px; border-radius: 3px; cursor: pointer;
  white-space: nowrap; overflow: hidden;
}
.qp-item.active { background: var(--ide-selection-focus); color: var(--nm-text-strong); }
.qp-item.disabled { opacity: 0.5; cursor: default; }
.qp-check { flex: none; width: 16px; display: inline-flex; color: var(--nm-text-strong); }
.qp-label { flex: none; max-width: 70%; overflow: hidden; text-overflow: ellipsis; }
.qp-desc { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; color: var(--nm-text-dim); font-size: 12px; }
.qp-empty { padding: 4px 8px; color: var(--nm-text-dim); }
.qp-prompt { padding: 6px 4px 4px; font-size: 12px; color: var(--nm-text-dim); }
.qp-prompt.invalid { color: var(--nm-danger); }
</style>
