<script setup lang="ts">
import { nextTick, onBeforeUnmount, onMounted, ref } from 'vue';

// A context menu at the pointer, VS Code style. Closes on any outside
// click, Escape, scroll or window blur. It takes the focus while open (and
// gives it back on close): the arrows move through the items, Right and Left
// open and leave a submenu, Enter or Space runs the item.

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
  /** A submenu, opened on hover (the item then has no action of its own). */
  children?: MenuItem[];
}

const props = defineProps<{ x: number; y: number; items: MenuItem[] }>();
const emit = defineEmits<{ close: [] }>();

const el = ref<HTMLDivElement | null>(null);
const pos = ref({ left: props.x, top: props.y });

function close() { emit('close'); }
function run(item: MenuItem) {
  if (item.disabled || item.children) return;
  giveFocusBack();
  close();
  item.action?.();
}
function onDown(e: MouseEvent) {
  if (el.value && !el.value.contains(e.target as Node)) close();
}
// -- submenu: one level, to the right of its item (left when there's no room).
const sub = ref<{ at: number; left: number; top: number; flip: number } | null>(null);
const subEl = ref<HTMLDivElement | null>(null);
async function hover(e: Event, i: number) {
  const item = props.items[i];
  if (!item.children?.length || item.disabled) { sub.value = null; return; }
  if (sub.value?.at === i) return;
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  sub.value = { at: i, left: r.right - 2, top: r.top - 5, flip: r.left + 2 };
  await nextTick();
  const s = subEl.value?.getBoundingClientRect();
  if (s && sub.value) {
    sub.value = {
      ...sub.value,
      left: sub.value.left + s.width > window.innerWidth - 4 ? Math.max(4, sub.value.flip - s.width) : sub.value.left,
      top: Math.max(4, Math.min(sub.value.top, window.innerHeight - s.height - 4)),
    };
  }
}
/** The items of the menu (or submenu) that holds `from`. */
function itemsOf(from: Element | null): HTMLElement[] {
  const box = from?.closest('.cm') ?? el.value;
  return box ? [...box.querySelectorAll<HTMLElement>(':scope > .cm-item:not(.disabled)')] : [];
}
function onKey(e: KeyboardEvent) {
  if (e.key === 'Escape') { close(); return; }
  const active = document.activeElement as HTMLElement | null;
  const onItem = !!active?.classList.contains('cm-item') && !!el.value?.contains(active);
  const inSub = onItem && !!subEl.value?.contains(active);
  let handled = true;
  if (e.key === 'ArrowDown' || e.key === 'ArrowUp' || e.key === 'Home' || e.key === 'End') {
    const list = itemsOf(onItem ? active : null);
    const at = onItem ? list.indexOf(active!) : -1;
    const next = e.key === 'Home' ? 0 : e.key === 'End' ? list.length - 1
      : e.key === 'ArrowDown' ? (at + 1) % list.length : (at <= 0 ? list.length : at) - 1;
    list[next]?.focus();
  } else if (onItem && (e.key === 'Enter' || e.key === ' ')) {
    if (!inSub && sub.value) itemsOf(subEl.value?.querySelector('.cm-item') ?? null)[0]?.focus();
    else active!.click();
  } else if (onItem && e.key === 'ArrowRight' && !inSub && sub.value) {
    itemsOf(subEl.value?.querySelector('.cm-item') ?? null)[0]?.focus();
  } else if (inSub && e.key === 'ArrowLeft') {
    el.value?.querySelectorAll<HTMLElement>(':scope > .cm-item')[sub.value!.at - headersBefore(sub.value!.at)]?.focus();
  } else handled = false;
  if (handled) { e.preventDefault(); e.stopPropagation(); }
}
/** Section titles before item `i` (they aren't `.cm-item`s). */
function headersBefore(i: number) {
  return props.items.slice(0, i).filter((x) => x.header).length;
}

let before: HTMLElement | null = null;
/** Back to where the focus was before the menu (unless it already moved on). */
function giveFocusBack() {
  if (el.value?.contains(document.activeElement)) before?.focus({ preventScroll: true });
}
onMounted(async () => {
  before = document.activeElement as HTMLElement | null;
  await nextTick();
  el.value?.focus({ preventScroll: true });
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
  giveFocusBack();
  window.removeEventListener('mousedown', onDown, true);
  window.removeEventListener('keydown', onKey, true);
  window.removeEventListener('blur', close);
  window.removeEventListener('wheel', close);
});
</script>

<template>
  <Teleport to="body">
    <div ref="el" class="cm" role="menu" tabindex="-1" :style="{ left: pos.left + 'px', top: pos.top + 'px' }" @contextmenu.prevent>
      <template v-for="(item, i) in items" :key="i">
        <div v-if="item.divided && i > 0" class="cm-sep" role="separator" />
        <div v-if="item.header" class="cm-header" role="presentation">{{ item.label }}</div>
        <div
          v-else
          class="cm-item"
          :class="{ danger: item.danger, disabled: item.disabled, open: sub?.at === i }"
          :role="item.checked !== undefined ? 'menuitemcheckbox' : 'menuitem'"
          tabindex="-1"
          :aria-checked="item.checked"
          :aria-disabled="item.disabled || undefined"
          :aria-haspopup="item.children ? 'menu' : undefined"
          :aria-expanded="item.children ? sub?.at === i : undefined"
          :title="item.hint"
          @click="run(item)"
          @mouseenter="hover($event, i)"
          @focus="hover($event, i)"
        >
          <span><span v-if="item.checked !== undefined" class="cm-check">{{ item.checked ? '✓' : '' }}</span>{{ item.label }}</span>
          <span v-if="item.children" class="cm-key">›</span>
          <span v-else-if="item.shortcut" class="cm-key">{{ item.shortcut }}</span>
        </div>
      </template>
      <div v-if="sub" ref="subEl" class="cm cm-sub" role="menu" :style="{ left: sub.left + 'px', top: sub.top + 'px' }">
        <template v-for="(child, k) in items[sub.at].children" :key="k">
          <div v-if="child.divided && k > 0" class="cm-sep" role="separator" />
          <div v-if="child.header" class="cm-header" role="presentation">{{ child.label }}</div>
          <div
            v-else class="cm-item" :class="{ danger: child.danger, disabled: child.disabled }"
            role="menuitem" tabindex="-1" :aria-disabled="child.disabled || undefined" :title="child.hint" @click="run(child)"
          >
            <span>{{ child.label }}</span>
            <span v-if="child.shortcut" class="cm-key">{{ child.shortcut }}</span>
          </div>
        </template>
      </div>
    </div>
  </Teleport>
</template>

<style scoped>
.cm {
  position: fixed;
  z-index: 3000;
  min-width: 200px;
  padding: 4px 0;
  background: var(--ide-widget);
  border: 1px solid var(--ide-widget-border);
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
.cm:focus { outline: none; }
.cm-item:hover, .cm-item.open, .cm-item:focus { background: var(--ide-selection-focus); color: #fff; outline: none; }
.cm-item.danger { color: var(--nm-error-fg); }
.cm-item.danger:hover { color: #fff; }
.cm-item.disabled { opacity: 0.45; cursor: default; background: transparent; }
.cm-key { color: var(--nm-text-dim); }
.cm-header { padding: 4px 20px 2px; font-size: 10.5px; letter-spacing: 0.06em; text-transform: uppercase; color: var(--nm-text-dim); }
.cm-check { display: inline-block; width: 16px; margin-left: -8px; color: var(--nm-accent-2); }
.cm { max-height: calc(100vh - 16px); overflow-y: auto; }
.cm-sep { height: 1px; margin: 4px 0; background: var(--ide-widget-border); }
</style>
