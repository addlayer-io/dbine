<script setup lang="ts">
import { computed, onBeforeUnmount, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { ANY_SQL } from '../api/library';
import { FAMILY_LABELS, type Family, type Language } from '../api/types';
import { newQuery } from '../composables/actions';
import { useAiStore } from '../stores/ai';
import { dbKey, useConnectionsStore } from '../stores/connections';
import { fillPlaceholders, placeholders, useLibraryStore } from '../stores/library';
import { useTabsStore } from '../stores/tabs';
import CodeEditor from './CodeEditor.vue';
import { inCodeEditor, isSaveShortcut } from '../composables/shortcuts';

// The Library's dialogs: edit / create a script, and open one (where, and
// its {{parameters}}).

const lib = useLibraryStore();
const { t } = useTranslation();
/** A `{{parameter}}` sample with a translated name. */
function param(word: string) {
  return `{{${t(`library:dialogs.param.${word}`)}}}`;
}
const conns = useConnectionsStore();
const tabs = useTabsStore();
const ai = useAiStore();

// -- edit ------------------------------------------------------------------------------------
const editOpen = computed({ get: () => !!lib.editing, set: (v) => { if (!v) lib.editing = null; } });
const e = computed(() => lib.editing);
const saving = ref(false);

/** Engines to pick from: "any SQL" and every driver, by family. */
const engineGroups = computed(() => {
  const byFamily = new Map<Family, { id: string; name: string }[]>();
  for (const d of conns.drivers) byFamily.set(d.family, [...(byFamily.get(d.family) ?? []), { id: d.id, name: d.name }]);
  return [...byFamily.entries()].map(([f, list]) => ({ label: FAMILY_LABELS[f] ?? f, list: list.sort((a, b) => a.name.localeCompare(b.name)) }));
});
/** The editor's language: from the first engine. */
const editLanguage = computed<Language>(() => {
  const first = e.value?.engines.find((x) => x !== ANY_SQL);
  return (first && conns.drivers.find((d) => d.id === first)?.language) || 'sql';
});
const editDialect = computed(() => {
  const first = e.value?.engines.find((x) => x !== ANY_SQL);
  return (first && conns.drivers.find((d) => d.id === first)?.dialect) || '';
});

async function saveScript() {
  const s = lib.editing;
  if (!s) return;
  if (!s.name.trim()) { ElMessage.warning(t('library:dialogs.nameRequired')); return; }
  if (!s.engines.length) { ElMessage.warning(t('library:dialogs.engineRequired')); return; }
  saving.value = true;
  try {
    await lib.save(s);
    ElMessage.success(s.id ? t('library:dialogs.saved') : t('library:dialogs.added'));
    lib.editing = null;
  } catch (err) {
    ElMessage.error(errorMessage(err));
  } finally {
    saving.value = false;
  }
}

// ⌘S (Ctrl+S on Windows / Linux) is "Guardar en biblioteca" while the editor
// is open. Inside the code editor CodeMirror takes the key (`@save`).
function onSaveKey(ev: KeyboardEvent) {
  if (!isSaveShortcut(ev) || inCodeEditor(ev)) return;
  ev.preventDefault();
  if (!saving.value) saveScript();
}
watch(editOpen, (open) => {
  if (open) window.addEventListener('keydown', onSaveKey);
  else window.removeEventListener('keydown', onSaveKey);
}, { immediate: true });
onBeforeUnmount(() => window.removeEventListener('keydown', onSaveKey));

// -- move ------------------------------------------------------------------------------------
const moveOpen = computed({ get: () => !!lib.moving, set: (v) => { if (!v) lib.moving = null; } });
const moveTo = ref('');
watch(() => lib.moving, (m) => { moveTo.value = m?.folder ?? ''; });
async function doMove() {
  const m = lib.moving;
  if (!m) return;
  try {
    await lib.moveScript(m.id, moveTo.value ?? '');
    lib.moving = null;
  } catch (err) {
    ElMessage.error(errorMessage(err));
  }
}

// -- open -----------------------------------------------------------------------------------
const openOpen = computed({ get: () => !!lib.opening, set: (v) => { if (!v) lib.opening = null; } });
const target = reactive({ connectionId: '', database: '' });
const values = reactive<Record<string, string>>({});
const params = computed(() => (lib.opening ? placeholders(lib.opening.script.text) : []));
const appendable = computed(() => tabs.active?.kind === 'query' && !!ai.activeBridge);

/** Connections whose engine the script fits (all, if none fits). */
const targetConns = computed(() => {
  const s = lib.opening?.script;
  const fit = conns.list.filter((c) => s && lib.fits(s, conns.driverOf(c.id)));
  return fit.length ? fit : conns.list;
});
const targetDbs = computed(() => conns.live[target.connectionId]?.databases ?? []);
/** Object names of the target database, for {{tabla}}-like parameters. */
const objectNames = computed(() =>
  (conns.objects[dbKey(target.connectionId, target.database)]?.items ?? [])
    .filter((o) => !o.parent)
    .map((o) => (o.schema ? `${o.schema}.${o.name}` : o.name)),
);

watch(() => lib.opening, async (o) => {
  for (const k of Object.keys(values)) delete values[k];
  if (!o) return;
  const t = tabs.active;
  if (o.mode === 'append' || (t && lib.fits(o.script, conns.driverOf(t.connectionId)))) {
    target.connectionId = t?.connectionId ?? '';
    target.database = t?.database ?? '';
  } else {
    target.connectionId = targetConns.value[0]?.id ?? '';
    target.database = '';
  }
  // Nothing to ask: go straight on.
  if (!params.value.length && target.connectionId && (o.mode === 'append' || t)) {
    await finish();
    return;
  }
  await loadTarget();
});
async function loadTarget() {
  if (!target.connectionId) return;
  if (!(await conns.ensureConnected(target.connectionId))) return;
  if (!target.database) target.database = conns.live[target.connectionId]?.defaultDatabase ?? targetDbs.value[0] ?? '';
  await conns.loadObjects(target.connectionId, target.database);
}
watch(() => [target.connectionId, target.database], () => { if (lib.opening) loadTarget(); });

function suggest(q: string, cb: (x: { value: string }[]) => void) {
  const l = q.toLowerCase();
  cb(objectNames.value.filter((n) => n.toLowerCase().includes(l)).slice(0, 50).map((value) => ({ value })));
}

async function finish() {
  const o = lib.opening;
  if (!o) return;
  const missing = params.value.filter((p) => !values[p]?.trim());
  if (missing.length) { ElMessage.warning(t('library:dialogs.fillIn', { list: missing.join(', ') })); return; }
  const text = fillPlaceholders(o.script.text, values);
  if (o.mode === 'append' && ai.activeBridge) {
    ai.activeBridge.append(text);
    ElMessage.success(t('library:dialogs.appended', { name: o.script.name }));
  } else {
    if (!target.connectionId) { ElMessage.warning(t('library:dialogs.pickConnection')); return; }
    await newQuery(target.connectionId, target.database, text, o.script.name);
  }
  lib.opening = null;
}
</script>

<template>
  <!-- Edit / new -->
  <el-dialog v-model="editOpen" :title="e?.id ? $t('library:dialogs.editTitle') : $t('library:dialogs.newTitle')" width="760px" append-to-body destroy-on-close>
    <div v-if="e" class="ld">
      <div class="ld-row">
        <el-input v-model="e.name" :placeholder="$t('library:dialogs.namePlaceholder')" class="ld-name" />
        <el-select v-model="e.folder" filterable allow-create default-first-option clearable :placeholder="$t('library:dialogs.folderPlaceholder')" class="ld-folder">
          <el-option v-for="f in lib.folders" :key="f" :label="f" :value="f" />
        </el-select>
      </div>
      <el-select v-model="e.engines" multiple filterable collapse-tags collapse-tags-tooltip :placeholder="$t('library:dialogs.enginesPlaceholder')" style="width: 100%">
        <el-option :value="ANY_SQL" :label="$t('library:dialogs.anySql')" />
        <el-option-group v-for="g in engineGroups" :key="g.label" :label="g.label">
          <el-option v-for="d in g.list" :key="d.id" :label="d.name" :value="d.id" />
        </el-option-group>
      </el-select>
      <el-input v-model="e.description" :placeholder="$t('library:dialogs.descriptionPlaceholder')" />
      <div class="ld-editor">
        <CodeEditor v-model="e.text" :language="editLanguage" :dialect="editDialect" @save="!saving && saveScript()" :placeholder="$t('library:dialogs.textPlaceholder', { example: param('name'), example2: param('table') })" />
      </div>
      <p class="ld-hint">
        <i18next :translation="$t('library:dialogs.paramsHint')">
          <template #table><code>{{ param('table') }}</code></template>
          <template #schema><code>{{ param('schema') }}</code></template>
          <template #days><code>{{ param('days') }}</code></template>
        </i18next>
        <template v-if="placeholders(e.text).length">
          {{ ' ' }}<i18next :translation="$t('library:dialogs.inThisScript')">
            <template #list><b>{{ placeholders(e.text).join(', ') }}</b></template>
          </i18next>
        </template>
      </p>
    </div>
    <template #footer>
      <el-button @click="lib.editing = null">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" :loading="saving" @click="saveScript">{{ $t('library:dialogs.saveToLibrary') }}</el-button>
    </template>
  </el-dialog>

  <!-- Move -->
  <el-dialog v-model="moveOpen" :title="lib.moving ? $t('library:dialogs.moveTitle', { name: lib.moving.name }) : ''" width="420px" append-to-body destroy-on-close>
    <el-select v-model="moveTo" filterable allow-create default-first-option :placeholder="$t('library:dialogs.folder')" style="width: 100%">
      <el-option :label="$t('library:dialogs.root')" value="" />
      <el-option v-for="f in lib.folders" :key="f" :label="f" :value="f" />
    </el-select>
    <p class="ld-hint" style="margin-top: 8px">{{ $t('library:dialogs.dragHint') }}</p>
    <template #footer>
      <el-button @click="lib.moving = null">{{ $t('common:cancel') }}</el-button>
      <el-button type="primary" @click="doMove">{{ $t('library:dialogs.move') }}</el-button>
    </template>
  </el-dialog>

  <!-- Open -->
  <el-dialog v-model="openOpen" :title="lib.opening ? $t('library:dialogs.openTitle', { name: lib.opening.script.name }) : ''" width="520px" append-to-body destroy-on-close>
    <div v-if="lib.opening" class="ld">
      <p v-if="lib.opening.script.description" class="ld-hint">{{ lib.opening.script.description }}</p>
      <template v-if="lib.opening.mode === 'new'">
        <label class="ld-label">{{ $t('library:dialogs.database') }}</label>
        <div class="ld-row">
          <el-select v-model="target.connectionId" filterable :placeholder="$t('library:dialogs.connection')" class="ld-conn" @change="target.database = ''">
            <el-option v-for="c in targetConns" :key="c.id" :label="c.name" :value="c.id">
              <span>{{ c.name }}</span><span class="ld-opt-detail">{{ conns.driverOf(c.id)?.name }}</span>
            </el-option>
          </el-select>
          <el-select v-if="targetDbs.length" v-model="target.database" filterable :placeholder="$t('library:dialogs.db')" class="ld-db">
            <el-option v-for="d in targetDbs" :key="d" :label="d" :value="d" />
          </el-select>
        </div>
      </template>
      <template v-for="p in params" :key="p">
        <label class="ld-label">{{ p }}</label>
        <el-autocomplete v-model="values[p]" :fetch-suggestions="suggest" :trigger-on-focus="true" clearable :placeholder="$t('library:dialogs.valueOf', { name: p })" style="width: 100%" @keyup.enter="finish" />
      </template>
      <p class="ld-hint">{{ $t('library:dialogs.openHint') }}</p>
    </div>
    <template #footer>
      <el-button @click="lib.opening = null">{{ $t('common:cancel') }}</el-button>
      <el-button v-if="lib.opening?.mode === 'new' && appendable" @click="lib.opening && (lib.opening.mode = 'append') && finish()">{{ $t('library:menu.appendOpen') }}</el-button>
      <el-button type="primary" @click="finish">{{ lib.opening?.mode === 'append' ? $t('library:dialogs.append') : $t('library:menu.openNew') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.ld { display: flex; flex-direction: column; gap: 8px; }
.ld-row { display: flex; gap: 8px; }
.ld-name { flex: 1; }
.ld-folder { width: 240px; }
.ld-conn { flex: 1; }
.ld-db { width: 200px; }
.ld-editor { height: 280px; border: 1px solid var(--nm-border); border-radius: 4px; overflow: hidden; }
.ld-hint { margin: 0; font-size: 12px; color: var(--nm-text-dim); line-height: 1.5; }
.ld-hint code { font-family: var(--nm-mono); }
.ld-label { font-size: 12px; color: var(--nm-text-dim); margin-top: 4px; }
.ld-opt-detail { float: right; margin-left: 12px; font-size: 11px; color: var(--nm-text-dim); }
</style>
