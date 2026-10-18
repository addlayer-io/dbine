<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from 'vue';
import { useTabsStore, type Tab } from '../stores/tabs';
import { MAX_GROUPS, type SplitDir } from '../stores/editorGroups';
import { isMac } from '../composables/shortcuts';
import EditorTabs from './EditorTabs.vue';

// The editor area: the editor groups (split editor, as in VS Code), each a
// tab strip over the tabs' views, side by side or one above the other, with
// a divider to resize them. The views of every open tab stay mounted in one
// list (the default slot renders one), each placed in its group's cell of a
// CSS grid: moving a tab to another group moves the cell it's shown in, not
// the view, so its editor, results and session are untouched.
//
// The focused group is the one clicked last; new tabs open there and it's
// the one `tabs.active` speaks for. ⌘\ / Ctrl+\ splits the focused tab to
// the right; dragging a tab onto a group's right or bottom half
// splits there, onto the rest of it moves the tab into that group.

const props = defineProps<{
  /** False until the connections are loaded: restored tabs wait for them. */
  ready: boolean;
}>();
defineSlots<{
  /** A tab's view; `shown`: it's the one its group shows. */
  default(p: { tab: Tab; shown: boolean }): unknown;
  /** No tab at all (one group, nothing shown): the welcome view. */
  empty(): unknown;
}>();

const tabs = useTabsStore();
const root = ref<HTMLElement | null>(null);

const row = computed(() => tabs.split === 'row');
/** The strips show once there's a tab (or more than one group). */
const strips = computed(() => tabs.tabs.length > 0 || tabs.groups.length > 1);
const STRIP = '35px';

const gridStyle = computed(() => {
  const sizes = tabs.groups.map((g) => `minmax(0, ${g.size}fr)`);
  const head = strips.value ? STRIP : '0px';
  if (row.value) return { gridTemplateColumns: sizes.join(' 1px '), gridTemplateRows: `${head} minmax(0, 1fr)` };
  return { gridTemplateColumns: 'minmax(0, 1fr)', gridTemplateRows: sizes.map((s) => `${head} ${s}`).join(' 1px ') };
});

const indexOf = (gid: string) => Math.max(0, tabs.groups.findIndex((g) => g.id === gid));
/** A group's strip or body cell. */
function cell(i: number, part: 'head' | 'body') {
  if (row.value) return { gridColumn: `${2 * i + 1}`, gridRow: part === 'head' ? '1' : '2' };
  return { gridColumn: '1', gridRow: `${3 * i + (part === 'head' ? 1 : 2)}` };
}
/** The divider before group `i`. */
function sashCell(i: number) {
  return row.value ? { gridColumn: `${2 * i}`, gridRow: '1 / span 2' } : { gridColumn: '1', gridRow: `${3 * i}` };
}

const views = computed(() => (props.ready ? tabs.tabs : []));

// -- divider
function dragSash(e: PointerEvent, i: number) {
  const a = root.value?.querySelector<HTMLElement>(`[data-head="${tabs.groups[i - 1].id}"]`);
  const b = root.value?.querySelector<HTMLElement>(`[data-body="${tabs.groups[i].id}"]`);
  if (!a || !b) return;
  const ra = a.getBoundingClientRect();
  const rb = b.getBoundingClientRect();
  const from = row.value ? ra.left : ra.top;
  const to = row.value ? rb.right : rb.bottom;
  const at = (ev: PointerEvent) => ((row.value ? ev.clientX : ev.clientY) - from) / Math.max(1, to - from);
  const move = (ev: PointerEvent) => tabs.resizeGroups(i, at(ev));
  const up = (ev: PointerEvent) => {
    window.removeEventListener('pointermove', move);
    window.removeEventListener('pointerup', up);
    document.body.classList.remove(row.value ? 'ea-resizing-x' : 'ea-resizing-y');
    tabs.resizeGroups(i, at(ev), true);
  };
  document.body.classList.add(row.value ? 'ea-resizing-x' : 'ea-resizing-y');
  window.addEventListener('pointermove', move);
  window.addEventListener('pointerup', up);
}

// -- dropping a dragged tab on a group's editor
const DRAG_TYPE = 'application/x-dbine-tab';
type Zone = 'center' | SplitDir;
const drop = ref<{ gid: string; zone: Zone } | null>(null);
function zoneAt(e: DragEvent): Zone {
  const r = (e.currentTarget as HTMLElement).getBoundingClientRect();
  const x = (e.clientX - r.left) / r.width;
  const y = (e.clientY - r.top) / r.height;
  if (tabs.groups.length >= MAX_GROUPS || (x <= 0.5 && y <= 0.5)) return 'center';
  return x >= y ? 'row' : 'column';
}
function onDragOver(e: DragEvent, gid: string) {
  if (!e.dataTransfer?.types.includes(DRAG_TYPE)) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = 'move';
  drop.value = { gid, zone: zoneAt(e) };
}
function onDrop(e: DragEvent, gid: string) {
  const id = e.dataTransfer?.getData(DRAG_TYPE);
  const zone = drop.value?.zone ?? 'center';
  endDrag();
  if (!id) return;
  e.preventDefault();
  const t = tabs.tabs.find((x) => x.id === id);
  if (!t) return;
  if (zone !== 'center') tabs.moveToNewGroup(id, gid, zone);
  else if (tabs.groupOf(t) !== gid) tabs.moveTab(id, gid);
}
function endDrag() {
  drop.value = null;
  tabs.dragTab = null;
}

// -- keys and focus
function onKey(e: KeyboardEvent) {
  const mod = isMac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  // The key's code: on many layouts "\" itself takes ⌥ / AltGr. With ⇧
  // it's the editor's "go to the matching bracket" (CodeMirror, VS Code).
  if (!mod || e.shiftKey || (e.key !== '\\' && e.code !== 'Backslash') || !tabs.tabs.length) return;
  e.preventDefault();
  tabs.splitEditor('row');
}
onMounted(() => {
  window.addEventListener('keydown', onKey);
  // A drop elsewhere (or a tab that moved while dragged) still ends the drag.
  window.addEventListener('dragend', endDrag, true);
  window.addEventListener('drop', endDrag);
});
onBeforeUnmount(() => {
  window.removeEventListener('keydown', onKey);
  window.removeEventListener('dragend', endDrag, true);
  window.removeEventListener('drop', endDrag);
});
</script>

<template>
  <div ref="root" class="ea" :class="tabs.split" :style="gridStyle">
    <template v-for="(g, i) in tabs.groups" :key="g.id">
      <div v-if="strips" class="ea-head" :data-head="g.id" :style="cell(i, 'head')" @pointerdown.capture="tabs.focusGroup(g.id)">
        <EditorTabs :group="g.id" />
      </div>
      <div class="ea-body" :data-body="g.id" :style="cell(i, 'body')" @pointerdown.capture="tabs.focusGroup(g.id)">
        <template v-if="!tabs.activeOf(g.id)">
          <slot v-if="tabs.groups.length === 1" name="empty" />
          <div v-else class="ea-empty">
            <p>{{ $t('split:empty') }}</p>
            <el-button v-if="!tabs.tabsOf(g.id).length" size="small" @click="tabs.closeEditorGroup(g.id)">{{ $t('split:closeGroup') }}</el-button>
          </div>
        </template>
      </div>
      <div v-if="i > 0" class="ea-sash" :style="sashCell(i)">
        <div class="ea-sash-hit" @pointerdown.prevent="dragSash($event, i)" />
      </div>
    </template>
    <div
      v-for="t in views"
      v-show="tabs.isShown(t)"
      :key="t.id"
      class="ea-view"
      :class="{ 'ea-unfocused': tabs.groupOf(t) !== tabs.focused }"
      :style="cell(indexOf(tabs.groupOf(t)), 'body')"
      @pointerdown.capture="tabs.focusGroup(tabs.groupOf(t))"
    >
      <slot :tab="t" :shown="tabs.isShown(t)" />
    </div>
    <!-- While a tab is dragged: the drop zones, over the views (an editor
         would take the drop as text). -->
    <template v-if="tabs.dragTab">
      <div
        v-for="(g, i) in tabs.groups"
        :key="`drop:${g.id}`"
        class="ea-drop"
        :style="cell(i, 'body')"
        @dragover="onDragOver($event, g.id)"
        @dragleave="drop = null"
        @drop="onDrop($event, g.id)"
      >
        <div v-if="drop?.gid === g.id" class="ea-drop-mark" :class="drop.zone" />
      </div>
    </template>
  </div>
</template>

<style scoped>
.ea { display: grid; min-width: 0; min-height: 0; overflow: hidden; }
.ea-head { min-width: 0; overflow: hidden; }
.ea-body, .ea-view { min-width: 0; min-height: 0; overflow: hidden; display: flex; flex-direction: column; }
.ea-body > * { flex: 1; min-height: 0; }
.ea-view { z-index: 1; }
.ea-view > * { flex: 1; min-height: 0; }
.ea-empty {
  display: flex; flex-direction: column; align-items: center; justify-content: center; gap: 10px;
  color: var(--nm-text-muted); font-size: 13px; text-align: center; padding: 16px;
}
.ea-empty p { margin: 0; max-width: 280px; }
.ea-sash { position: relative; background: var(--nm-border-soft, rgba(255, 255, 255, 0.08)); z-index: 3; }
.ea-sash-hit { position: absolute; inset: 0 -3px; cursor: col-resize; }
.ea.column .ea-sash-hit { inset: -3px 0; cursor: row-resize; }
.ea-sash-hit:hover { background: var(--ide-focus); }
.ea-drop { position: relative; z-index: 4; }
.ea-drop-mark {
  position: absolute; inset: 0; pointer-events: none;
  background: color-mix(in srgb, var(--ide-focus, #0078d4) 22%, transparent);
  outline: 1px solid color-mix(in srgb, var(--ide-focus, #0078d4) 60%, transparent); outline-offset: -1px;
  transition: inset 0.08s ease;
}
.ea-drop-mark.row { left: 50%; }
.ea-drop-mark.column { top: 50%; }
</style>

<style>
/* While a divider is dragged, the whole window shows the resize cursor and
   no text gets selected. */
body.ea-resizing-x, body.ea-resizing-x * { cursor: col-resize !important; user-select: none !important; }
body.ea-resizing-y, body.ea-resizing-y * { cursor: row-resize !important; user-select: none !important; }
</style>
