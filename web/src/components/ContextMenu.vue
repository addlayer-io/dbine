<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue';

// A context menu at the pointer, VS Code style. Closes on any outside
// click, Escape, scroll or window blur.

export interface MenuItem {
  label: string;
  action?: () => void;
  shortcut?: string;
  danger?: boolean;
  disabled?: boolean;
  /** Tooltip (why it's disabled). */
  hint?: string;
  /** Separator above this item. */
  divided?: boolean;
  /** A section title, not clickable. */
  header?: boolean;
  /** Shows a check mark (the current choice of a group). */
  checked?: boolean;
}

const props = defineProps<{ x: number; y: number; items: MenuItem[] }>();
const emit = defineEmits<{ close: [] }>();

const el = ref<HTMLDivElement | null>(null);
const pos = ref({ left: props.x, top: props.y });

function close() { emit('close'); }
function run(item: MenuItem) {
  if (item.disabled) return;
  close();
  item.action?.();
}
function onDown(e: MouseEvent) {
  if (el.value && !el.value.contains(e.target as Node)) close();
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') close();
}

onMounted(async () => {
  await nextTick();
  // Keep it on screen.
  const r = el.value?.getBoundingClientRect();
  if (r) {
    pos.value = {
      left: Math.min(props.x, window.innerWidth - r.width - 4),
      top: Math.min(props.y, window.innerHeight - r.height - 4),
    };
  }
  window.addEventListener('mousedown', onDown, true);
  window.addEventListener('keydown', onKey, true);
  window.addEventListener('blur', close);
  window.addEventListener('wheel', close, { passive: true });
});
onBeforeUnmount(() => {
  window.removeEventListener('mousedown', onDown, true);
  window.removeEventListener('keydown', onKey, true);
  window.removeEventListener('blur', close);
  window.removeEventListener('wheel', close);
});
</script>

<template>
  <Teleport to="body">
    <div ref="el" class="cm" :style="{ left: pos.left + 'px', top: pos.top + 'px' }" @contextmenu.prevent>
      <template v-for="(item, i) in items" :key="i">
        <div v-if="item.divided && i > 0" class="cm-sep" />
        <div v-if="item.header" class="cm-header">{{ item.label }}</div>
        <div
          v-else
          class="cm-item"
          :class="{ danger: item.danger, disabled: item.disabled }"
          :title="item.hint"
          @click="run(item)"
        >
          <span><span v-if="item.checked !== undefined" class="cm-check">{{ item.checked ? '✓' : '' }}</span>{{ item.label }}</span>
          <span v-if="item.shortcut" class="cm-key">{{ item.shortcut }}</span>
        </div>
      </template>
    </div>
  </Teleport>
</template>

<style scoped>
.cm {
  position: fixed;
  z-index: 3000;
  min-width: 200px;
  padding: 4px 0;
  background: #252526;
  border: 1px solid #454545;
  border-radius: 4px;
  box-shadow: 0 2px 8px rgba(0, 0, 0, 0.5);
  font-size: 12.5px;
  color: var(--nm-text);
}
.cm-item {
  display: flex;
  justify-content: space-between;
  gap: 24px;
  padding: 0 20px;
  line-height: 24px;
  cursor: pointer;
  white-space: nowrap;
}
.cm-item:hover { background: var(--ide-selection-focus); color: #fff; }
.cm-item.danger { color: #f48771; }
.cm-item.danger:hover { color: #fff; }
.cm-item.disabled { opacity: 0.45; cursor: default; background: transparent; }
.cm-key { color: var(--nm-text-dim); }
.cm-header { padding: 4px 20px 2px; font-size: 10.5px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--nm-text-dim); }
.cm-check { display: inline-block; width: 16px; margin-left: -8px; color: #75beff; }
.cm { max-height: calc(100vh - 16px); overflow-y: auto; }
.cm-sep { height: 1px; margin: 4px 0; background: #454545; }
</style>
