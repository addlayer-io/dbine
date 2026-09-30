<script setup lang="ts">
import { computed, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';
import FolderPicker from './FolderPicker.vue';

// New / edit explorer folder: name, color (connections without their own
// take it — handy to tell production apart) and where it lives.

const ui = useUiStore();
const conns = useConnectionsStore();
const { t } = useTranslation();

const FOLDER_COLORS = ['', '#f14c4c', '#cca700', '#89d185', '#3794ff', '#c586c0', '#4ec9b0', '#ce9178'];

const editing = computed(() => (ui.editingFolder ? conns.folders.find((f) => f.id === ui.editingFolder) ?? null : null));
const name = ref('');
const color = ref('');
const parent = ref<string | null>(null);
const saving = ref(false);

watch(() => ui.editingFolder, (id) => {
  if (id === null) return;
  name.value = editing.value?.name ?? '';
  color.value = editing.value?.color ?? '';
  parent.value = editing.value ? editing.value.parent_id : ui.newFolderParent;
}, { immediate: true });

async function save() {
  if (!name.value.trim()) { ElMessage.warning(t('dialogs:folder.nameRequired')); return; }
  saving.value = true;
  try {
    await conns.saveFolder({ id: editing.value?.id ?? '', name: name.value.trim(), color: color.value || null, parent_id: parent.value });
    ui.closeFolderDialog();
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    saving.value = false;
  }
}
</script>

<template>
  <el-dialog
    :model-value="ui.editingFolder !== null"
    :title="editing ? $t('dialogs:folder.edit', { name: editing.name }) : $t('dialogs:folder.new')"
    width="440px"
    append-to-body
    @close="ui.closeFolderDialog()"
  >
    <el-form label-position="top" @submit.prevent="save">
      <el-form-item :label="$t('dialogs:folder.name')">
        <el-input v-model="name" :placeholder="$t('dialogs:folder.namePlaceholder')" autofocus @keyup.enter="save" />
      </el-form-item>
      <el-form-item :label="$t('dialogs:folder.color')">
        <div class="fd-colors">
          <button
            v-for="c in FOLDER_COLORS"
            :key="c"
            class="fd-color"
            :class="{ active: color === c }"
            :style="{ background: c || 'transparent' }"
            :title="c || $t('dialogs:folder.noColor')"
            @click.prevent="color = c"
          >{{ c ? '' : '∅' }}</button>
        </div>
        <div class="fd-help">{{ $t('dialogs:folder.colorHelp') }}</div>
      </el-form-item>
      <el-form-item :label="$t('dialogs:folder.inside')">
        <FolderPicker v-model="parent" :exclude="editing?.id ?? null" />
      </el-form-item>
    </el-form>
    <template #footer>
      <el-button @click="ui.closeFolderDialog()">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="saving" @click="save">{{ editing ? $t('common:save') : $t('dialogs:folder.create') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.fd-colors { display: flex; gap: 6px; }
.fd-color {
  width: 20px; height: 20px; border-radius: 50%; border: 1px solid var(--nm-border);
  cursor: pointer; color: var(--nm-text-dim); font-size: 11px; padding: 0;
}
.fd-color.active { outline: 2px solid #fff; outline-offset: 1px; }
.fd-help { font-size: 11.5px; color: var(--nm-text-dim); margin-top: 4px; line-height: 1.4; }
</style>
