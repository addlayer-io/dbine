<script setup lang="ts">
import { computed } from 'vue';
import { useSettingsStore } from '../../stores/settings';
import NotebookFileGrants from './NotebookFileGrants.vue';
import {
  BYTES_DEFAULT, BYTES_RANGE, BYTES_SETTING, RESULTS_CAP_DEFAULT, RESULTS_CAP_RANGE, RESULTS_CAP_SETTING, ROWS_DEFAULT, ROWS_RANGE, ROWS_SETTING, clamp,
} from '../../composables/notebookModel';

// Configuración › Notebooks: how much of each cell's result a notebook file
// keeps (docs/notebooks.md › The result and what is saved). The screen
// always shows the whole result; only the file is cut. Also the rows per
// table a "Resultados del notebook" cell loads, and the local files
// notebooks may read ("Permitir siempre").

const settings = useSettingsStore();
const MB = 1_000_000;

const rows = computed({
  get: () => settings.get<number>(ROWS_SETTING, ROWS_DEFAULT),
  set: (v: number | undefined) => settings.set(ROWS_SETTING, clamp(v ?? ROWS_DEFAULT, ROWS_RANGE)),
});
/** In MB on screen, bytes in the setting. */
const megabytes = computed({
  get: () => settings.get<number>(BYTES_SETTING, BYTES_DEFAULT) / MB,
  set: (v: number | undefined) => settings.set(BYTES_SETTING, clamp((v ?? 1) * MB, BYTES_RANGE)),
});
/** Rows per table of the notebook's results (in memory, never on disk). */
const resultsCap = computed({
  get: () => settings.get<number>(RESULTS_CAP_SETTING, RESULTS_CAP_DEFAULT),
  set: (v: number | undefined) => settings.set(RESULTS_CAP_SETTING, clamp(v ?? RESULTS_CAP_DEFAULT, RESULTS_CAP_RANGE)),
});
</script>

<template>
  <div class="ns">
    <h3>{{ $t('notebook:settings.title') }}</h3>
    <p class="ns-muted">{{ $t('notebook:settings.help') }}</p>
    <div class="ns-row">
      <div><strong>{{ $t('notebook:settings.rows') }}</strong><span>{{ $t('notebook:settings.rowsHelp') }}</span></div>
      <el-input-number v-model="rows" :min="ROWS_RANGE[0]" :max="ROWS_RANGE[1]" :step="50" controls-position="right" style="width: 160px" />
    </div>
    <div class="ns-row">
      <div><strong>{{ $t('notebook:settings.size') }}</strong><span>{{ $t('notebook:settings.sizeHelp') }}</span></div>
      <div class="ns-mb">
        <el-input-number v-model="megabytes" :min="0.0625" :max="8" :step="0.25" controls-position="right" style="width: 160px" />
        <span>MB</span>
      </div>
    </div>
    <div class="ns-row">
      <div><strong>{{ $t('notebook:settings.resultsCap') }}</strong><span>{{ $t('notebook:settings.resultsCapHelp') }}</span></div>
      <el-input-number v-model="resultsCap" :min="RESULTS_CAP_RANGE[0]" :max="RESULTS_CAP_RANGE[1]" :step="100000" controls-position="right" style="width: 160px" />
    </div>
    <NotebookFileGrants />
  </div>
</template>

<style scoped>
.ns h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.ns-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.ns-row { display: flex; align-items: center; justify-content: space-between; gap: 20px; padding: 12px 0; border-bottom: 1px solid var(--nm-border); }
.ns-row > div:first-child { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
.ns-row strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 500; }
.ns-row span { font-size: 12px; color: var(--nm-text-dim); }
.ns-mb { display: inline-flex; align-items: center; gap: 6px; flex-shrink: 0; }
</style>
