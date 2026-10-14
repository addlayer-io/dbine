<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import { locale } from '../i18n';
import { ALL_ROWS, type QueryControls } from '../composables/queryControls';
import { openQuickInput, openQuickPick, type QuickPickItem } from '../composables/quickPick';
import { useConnectionsStore } from '../stores/connections';
import type { QueryTab, FileTab } from '../stores/tabs';

// The active query tab's connection, database, transaction mode and row
// limit in the status bar; each opens a quick pick to change it.

const props = defineProps<{ tab: QueryTab | FileTab; controls: QueryControls }>();

const conns = useConnectionsStore();
const { t } = useTranslation();

const conn = computed(() => conns.byId(props.tab.connectionId));
const driver = computed(() => conns.driverOf(props.tab.connectionId));
const c = computed(() => props.controls);
/** Switching while a query runs would leave it on the other base. */
const locked = computed(() => c.value.running || c.value.txBusy);

const rowsText = computed(() => (c.value.maxRows >= ALL_ROWS ? t('query:status.rowsAll') : t('query:status.rowsMax', { rows: c.value.maxRows.toLocaleString(locale()) })));

const connTitle = computed(() => (locked.value ? t('query:status.lockedRunning') : t('query:status.connectionTip', { name: conn.value?.name ?? '' })));
const dbTitle = computed(() => (locked.value ? t('query:status.lockedRunning') : t('query:status.databaseTip')));
const txTitle = computed(() => {
  const mode = c.value.manualTx ? t('query:tx.manualTip') : t('query:tx.autoTip');
  const open = c.value.txOpen ? `\n${c.value.txState === 'failed' ? t('query:tx.failed') : t('query:tx.open')}` : '';
  return `${mode}${open}\n${t('query:status.txTip')}`;
});

async function pickConnection() {
  if (locked.value) return;
  const items = () => conns.list
    .map((x): QuickPickItem => ({
      id: x.id, label: x.name, current: x.id === props.tab.connectionId,
      description: [conns.drivers.find((d) => d.id === x.config.driver)?.name, x.config.host].filter(Boolean).join(' · '),
    }))
    .sort((a, b) => a.label.localeCompare(b.label, locale()));
  const chosen = await openQuickPick({ placeholder: t('query:status.pickConnection'), items });
  if (chosen) await c.value.changeConnection(chosen.id);
}

async function pickDatabase() {
  if (locked.value) return;
  void c.value.loadDatabases();
  const items = () => c.value.databases.map((d): QuickPickItem => ({ id: d, label: d, current: d === props.tab.database }));
  const chosen = await openQuickPick({
    placeholder: t('query:status.pickDatabase'), items, busy: () => c.value.loadingDatabases,
  });
  if (chosen) await c.value.changeDatabase(chosen.id);
}

async function pickTxMode() {
  if (locked.value) return;
  const items = () => [
    { id: 'auto', label: t('query:tx.auto'), description: t('query:tx.autoTip'), current: !c.value.manualTx },
    { id: 'manual', label: t('query:tx.manual'), description: t('query:tx.manualTip'), current: c.value.manualTx },
  ];
  const chosen = await openQuickPick({ placeholder: t('query:status.pickTx'), items });
  if (chosen) await c.value.setManualTx(chosen.id === 'manual');
}

const ROWS = [100, 1000, 10000, 100000];
async function pickMaxRows() {
  const n = c.value.maxRows;
  const preset = ROWS.includes(n) || n >= ALL_ROWS;
  const items = (): QuickPickItem[] => [
    ...ROWS.map((r) => ({ id: String(r), label: r.toLocaleString(locale()), current: r === n })),
    { id: String(ALL_ROWS), label: t('query:status.rowsAllOption'), current: n >= ALL_ROWS },
    { id: 'custom', label: t('query:status.rowsCustom'), description: preset ? undefined : n.toLocaleString(locale()), current: !preset },
  ];
  const chosen = await openQuickPick({ placeholder: t('query:status.pickRows'), items });
  if (!chosen) return;
  if (chosen.id !== 'custom') { c.value.setMaxRows(Number(chosen.id)); return; }
  const text = await openQuickInput({
    placeholder: t('query:status.rowsCustomPlaceholder'),
    prompt: t('query:status.rowsCustomPrompt'),
    value: preset ? '' : String(n),
    validate: (v) => (parseRows(v) === null ? t('query:status.rowsCustomInvalid') : null),
  });
  const rows = text === null ? null : parseRows(text);
  if (rows !== null) c.value.setMaxRows(rows);
}
/** A whole number of rows, 1 or more ("10.000", "10,000" and "10 000" too). */
function parseRows(v: string): number | null {
  const s = v.trim().replace(/[.,\s  ']/g, '');
  if (!/^\d+$/.test(s)) return null;
  const n = Number(s);
  return n >= 1 ? Math.min(n, ALL_ROWS) : null;
}
</script>

<template>
  <button class="sb-item sb-btn" :class="{ 'sb-locked': locked }" :aria-disabled="locked || undefined" :title="connTitle" data-sb="connection" @click="pickConnection">
    <el-icon><ei-connection /></el-icon><span class="sb-trim">{{ conn?.name ?? $t('query:status.noConnection') }}</span>
    <span v-if="driver" class="sb-dim">{{ driver.name }}</span>
  </button>
  <button v-if="controls.showDatabase" class="sb-item sb-btn" :class="{ 'sb-locked': locked }" :aria-disabled="locked || undefined" :title="dbTitle" data-sb="database" @click="pickDatabase">
    <el-icon><ei-coin /></el-icon><span class="sb-trim">{{ tab.database || $t('query:defaultDatabase') }}</span>
  </button>
  <button v-if="controls.offersManualTx" class="sb-item sb-btn" :class="{ 'sb-locked': locked, 'sb-tx-open': controls.txOpen, 'sb-tx-failed': controls.txState === 'failed' }" :aria-disabled="locked || undefined" :title="txTitle" data-sb="tx" @click="pickTxMode">
    <el-icon v-if="controls.txOpen"><ei-warning-filled /></el-icon>
    {{ controls.manualTx ? $t('query:tx.manual') : $t('query:tx.auto') }}
    <span v-if="controls.txOpen">· {{ controls.txState === 'failed' ? $t('query:tx.failed') : $t('query:tx.open') }}</span>
  </button>
  <button class="sb-item sb-btn" :title="$t('query:status.rowsTipButton')" data-sb="rows" @click="pickMaxRows">{{ rowsText }}</button>
</template>

<style scoped>
.sb-item { display: inline-flex; align-items: center; gap: 4px; height: 100%; padding: 0 7px; font-variant-numeric: tabular-nums; }
.sb-btn { font: inherit; color: inherit; background: transparent; border: none; cursor: pointer; }
.sb-btn:hover { background: rgba(255, 255, 255, 0.12); }
.sb-btn:focus-visible { outline: 1px solid #ffffff; outline-offset: -1px; }
.sb-locked { cursor: default; opacity: 0.75; }
.sb-locked:hover { background: transparent; }
.sb-trim { max-width: 220px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.sb-dim { opacity: 0.8; }
.sb-tx-open { background: #c27d0e; }
.sb-tx-open:hover { background: #d18a16; }
.sb-tx-failed { background: #c72e2e; }
.sb-tx-failed:hover { background: #d63b3b; }
</style>
