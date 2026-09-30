<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { errorMessage } from '../api/client';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';
import FolderPicker from './FolderPicker.vue';

// "Mover a…": the keyboard/menu way to do what drag and drop does.

const ui = useUiStore();
const conns = useConnectionsStore();
const target = ref<string | null>(null);

const label = computed(() => {
  const m = ui.moving;
  if (!m) return '';
  return m.kind === 'connection' ? conns.byId(m.id)?.name ?? '' : conns.folders.find((f) => f.id === m.id)?.name ?? '';
});

watch(() => ui.moving, (m) => {
  if (!m) return;
  target.value = m.kind === 'connection'
    ? conns.byId(m.id)?.folder_id ?? null
    : conns.folders.find((f) => f.id === m.id)?.parent_id ?? null;
});

async function move() {
  const m = ui.moving;
  if (!m) return;
  try {
    if (m.kind === 'connection') await conns.moveConnection(m.id, target.value);
    else await conns.moveFolder(m.id, target.value);
    ui.closeMove();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
</script>

<template>
  <el-dialog :model-value="!!ui.moving" :title="$t('dialogs:move.title', { name: label })" width="420px" append-to-body @close="ui.closeMove()">
    <FolderPicker v-model="target" :exclude="ui.moving?.kind === 'folder' ? ui.moving.id : null" />
    <template #footer>
      <el-button @click="ui.closeMove()">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" @click="move">{{ $t('dialogs:move.move') }}</el-button>
    </template>
  </el-dialog>
</template>
