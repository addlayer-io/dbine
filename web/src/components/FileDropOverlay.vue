<script setup lang="ts">
import { fileDragOver, useOpenFiles } from '../composables/openFiles';

// Files dragged from another app over the window: "Soltá para abrir". Also
// where the window starts taking the files the OS hands over
// (composables/openFiles.ts).
useOpenFiles();
</script>

<template>
  <div v-if="fileDragOver" class="fdo" aria-live="polite">
    <div class="fdo-box">
      <el-icon :size="34"><ei-upload-filled /></el-icon>
      <div class="fdo-title">{{ $t('core:openFiles.dropTitle') }}</div>
      <div class="fdo-hint">{{ $t('core:openFiles.dropHint') }}</div>
    </div>
  </div>
</template>

<style scoped>
/* Never in the drop's way: the events go to the window underneath. */
.fdo {
  position: fixed; inset: 0; z-index: 4000; pointer-events: none;
  display: flex; align-items: center; justify-content: center;
  background: color-mix(in srgb, var(--ide-editor) 72%, transparent);
  outline: 2px dashed var(--ide-focus); outline-offset: -10px;
}
.fdo-box { display: flex; flex-direction: column; align-items: center; gap: 8px; color: var(--el-text-color-primary); text-align: center; }
.fdo-title { font-size: 16px; font-weight: 600; }
.fdo-hint { font-size: 12px; color: var(--nm-text-dim); max-width: 420px; }
</style>
