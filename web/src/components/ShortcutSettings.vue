<script setup lang="ts">
import { computed, onBeforeUnmount, ref } from 'vue';
import { useTranslation } from 'i18next-vue';
import {
  COMMANDS, bindingLabel, bindingOf, commandDef, conflicts, isOverridden, isReserved, keyOfEvent, recordingKeys,
  resetAllBindings, resetBinding, setBinding, type CommandGroup,
} from '../composables/commands';
import { formatKey, isModifierOnly, isMac } from '../composables/keybindings';

// Configuración › Atajos de teclado: every action of the registry with its
// keys. "Cambiar" records the next combination (Esc cancels); the change
// applies at once and syncs with the other settings. A combination another
// action uses where both act is allowed but shown as a conflict.

const { t } = useTranslation();
const query = ref('');
const GROUPS: CommandGroup[] = ['general', 'view', 'editor', 'results'];

const rows = computed(() => {
  const q = query.value.trim().toLowerCase();
  return COMMANDS.filter((c) => {
    if (!q) return true;
    return t(c.label).toLowerCase().includes(q) || bindingLabel(c.id).toLowerCase().includes(q) || c.id.toLowerCase().includes(q);
  });
});
const groups = computed(() => GROUPS
  .map((g) => ({ id: g, rows: rows.value.filter((c) => c.group === g) }))
  .filter((g) => g.rows.length));

function conflictText(id: string): string {
  const others = conflicts.value.get(id);
  if (!others?.length) return '';
  return t('shortcuts:conflict', { names: others.map((o) => t(commandDef(o)?.label ?? o)).join(', ') });
}

// -- recording ------------------------------------------------------------------------------
const recording = ref<string | null>(null);
/** What's held while recording (shows "⌘⇧…" before the key). */
const held = ref('');

function onRecordKey(e: KeyboardEvent) {
  // Before anything else in the window: nothing else runs these keys (and
  // Esc doesn't close the dialog).
  e.preventDefault();
  e.stopImmediatePropagation();
  const id = recording.value;
  if (!id) return;
  if (isModifierOnly(e)) {
    const mods = [e.metaKey && isMac ? 'Mod' : '', e.ctrlKey ? (isMac ? 'Ctrl' : 'Mod') : '', e.altKey ? 'Alt' : '', e.shiftKey ? 'Shift' : ''].filter(Boolean);
    held.value = mods.length ? formatKey([...mods, 'x'].join('-'), isMac).slice(0, -1) : '';
    return;
  }
  if (e.key === 'Escape' && !e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) { stopRecording(); return; }
  const k = keyOfEvent(e);
  if (!k) return;
  stopRecording();
  void setBinding(id, k);
}
function onRecordKeyUp(e: KeyboardEvent) {
  e.stopImmediatePropagation();
  if (!e.metaKey && !e.ctrlKey && !e.altKey && !e.shiftKey) held.value = '';
}

function startRecording(id: string) {
  stopRecording();
  recording.value = id;
  held.value = '';
  recordingKeys.value = true;
  window.addEventListener('keydown', onRecordKey, true);
  window.addEventListener('keyup', onRecordKeyUp, true);
  window.addEventListener('pointerdown', onPointer, true);
}
function stopRecording() {
  if (!recording.value) return;
  recording.value = null;
  held.value = '';
  recordingKeys.value = false;
  window.removeEventListener('keydown', onRecordKey, true);
  window.removeEventListener('keyup', onRecordKeyUp, true);
  window.removeEventListener('pointerdown', onPointer, true);
}
/** A click anywhere but the recording cell ends it. */
function onPointer(e: PointerEvent) {
  if (!(e.target instanceof Element && e.target.closest('.ks-recording'))) stopRecording();
}
onBeforeUnmount(stopRecording);

const anyOverride = computed(() => COMMANDS.some((c) => isOverridden(c.id)));
</script>

<template>
  <div class="ks">
    <div class="ks-head">
      <h3>{{ $t('shortcuts:nav') }}</h3>
      <el-button size="small" :disabled="!anyOverride" @click="resetAllBindings">{{ $t('shortcuts:resetAll') }}</el-button>
    </div>
    <p class="ks-muted">{{ $t('shortcuts:help') }}</p>
    <el-input v-model="query" size="small" clearable class="ks-search" :placeholder="$t('shortcuts:search')" :aria-label="$t('shortcuts:search')">
      <template #prefix><el-icon><ei-search /></el-icon></template>
    </el-input>
    <p v-if="!groups.length" class="ks-muted">{{ $t('shortcuts:none') }}</p>
    <template v-for="g in groups" :key="g.id">
      <h4>{{ $t(`shortcuts:group.${g.id}`) }}</h4>
      <div v-for="c in g.rows" :key="c.id" class="ks-row" :class="{ 'ks-conflict': conflicts.has(c.id) }">
        <div class="ks-name">
          <span>{{ $t(c.label) }}</span>
          <small v-if="conflicts.has(c.id)" class="ks-warn"><el-icon><ei-warning /></el-icon>{{ conflictText(c.id) }}</small>
          <small v-else-if="isReserved(c.id)" class="ks-warn"><el-icon><ei-warning /></el-icon>{{ $t('shortcuts:reserved') }}</small>
        </div>
        <button
          v-if="recording === c.id"
          class="ks-key ks-recording"
          :aria-label="$t('shortcuts:recording')"
          @click="stopRecording"
        >{{ held ? held + '…' : $t('shortcuts:recording') }}</button>
        <button
          v-else
          class="ks-key"
          :class="{ 'ks-custom': isOverridden(c.id), 'ks-empty': !bindingOf(c.id) }"
          :disabled="c.fixed"
          :title="$t('shortcuts:change')"
          @click="startRecording(c.id)"
        >
          <kbd v-if="bindingOf(c.id)">{{ bindingLabel(c.id) }}</kbd>
          <span v-else>{{ $t('shortcuts:unbound') }}</span>
        </button>
        <div class="ks-actions">
          <el-button link size="small" :disabled="c.fixed || !bindingOf(c.id)" :title="$t('shortcuts:remove')" :aria-label="$t('shortcuts:remove')" @click="setBinding(c.id, null)">
            <el-icon><ei-close /></el-icon>
          </el-button>
          <el-button link size="small" :disabled="!isOverridden(c.id)" :title="$t('shortcuts:reset')" :aria-label="$t('shortcuts:reset')" @click="resetBinding(c.id)">
            <el-icon><ei-refresh-left /></el-icon>
          </el-button>
        </div>
      </div>
    </template>
  </div>
</template>

<style scoped>
.ks h3 { margin: 0; font-size: 16px; color: var(--nm-text-strong); }
.ks h4 { margin: 18px 0 4px; font-size: 12px; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim); font-weight: 600; }
.ks-head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 4px; }
.ks-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.ks-search { max-width: 360px; }
.ks-row { display: flex; align-items: center; gap: 12px; padding: 5px 0; border-bottom: 1px solid var(--nm-border-soft); }
.ks-name { flex: 1; min-width: 0; display: flex; flex-direction: column; gap: 2px; font-size: 13px; color: var(--nm-text); }
.ks-warn { display: flex; align-items: center; gap: 4px; font-size: 11.5px; color: var(--nm-warning); }
.ks-key {
  width: 170px; flex: none; height: 26px; padding: 0 8px; border-radius: 4px; border: 1px solid var(--nm-border);
  background: transparent; color: var(--nm-text); font: inherit; font-size: 12px; text-align: left; cursor: pointer;
}
.ks-key:hover:not(:disabled) { border-color: var(--el-color-primary); }
.ks-key kbd { font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text-strong); }
.ks-key.ks-custom kbd { color: var(--el-color-primary); }
.ks-key.ks-empty { color: var(--nm-text-dim); font-style: italic; }
.ks-conflict .ks-key { border-color: color-mix(in srgb, var(--nm-warning) 60%, transparent); }
.ks-recording { border-color: var(--el-color-primary); color: var(--el-color-primary); box-shadow: 0 0 0 2px color-mix(in srgb, var(--el-color-primary) 25%, transparent); }
.ks-actions { display: flex; flex: none; }
.ks-actions :deep(.el-button + .el-button) { margin-left: 2px; }
</style>
