<script setup lang="ts">
import { computed, reactive, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { invoke } from '@tauri-apps/api/core';
import { errorKind, errorMessage } from '../api/client';
import { driversApi, type DriverPackage } from '../api/drivers';
import { syncApi, type LocalBackup, type ProviderKind, type RemoteInfo, type SyncAction } from '../api/sync';
import { COPY_FORMATS, type CopyFormat } from '../composables/copyFormats';
import { TELEMETRY_CONSENT, trackAppStarted } from '../composables/telemetry';
import { useSettingsStore } from '../stores/settings';
import { useTranslation } from 'i18next-vue';
import { LANGUAGES, SETTING_KEY, language, locale, setLanguage, type Lang } from '../i18n';
import { tb } from '../i18n/backend';
import { useSyncStore } from '../stores/sync';
import { useUiStore } from '../stores/ui';
import DriversSettings from './DriversSettings.vue';
import McpSettings from './McpSettings.vue';

// Configuración (⌘,): preferences and the cloud backup (docs/sincronizacion.md).

const ui = useUiStore();
const { t } = useTranslation();
const settings = useSettingsStore();

/** The app's language: applies at once and syncs to the other machines. */
async function pickLanguage(code: Lang) {
  await setLanguage(code);
  await settings.set(SETTING_KEY, code);
}
const sync = useSyncStore();
function openSupport() {
  invoke('open_support_page').catch((e) => ElMessage.error(errorMessage(e)));
}

const visible = computed({
  get: () => ui.settingsSection !== null,
  set: (v) => { if (!v) ui.closeSettings(); },
});
const section = computed({
  get: () => ui.settingsSection ?? 'general',
  set: (v) => ui.openSettings(v),
});

// -- general ------------------------------------------------------------------------
const copyFormat = computed({
  get: () => settings.get<CopyFormat>('grid.copyFormat', 'tsv'),
  set: (v) => settings.set('grid.copyFormat', v),
});
const maxRows = computed({
  get: () => settings.get<number>('query.maxRows', 5000),
  set: (v) => settings.set('query.maxRows', v),
});
/** Anonymous usage data: on by default (TelemetryConsent tells once). */
const telemetry = computed({
  get: () => settings.get<boolean | null>(TELEMETRY_CONSENT, null) !== false,
  set: (v) => { settings.set(TELEMETRY_CONSENT, v); if (v) trackAppStarted(); },
});
const MAX_ROWS = [100, 500, 1000, 5000, 10000, 50000, 100000];
/** Explorer: schemas as a tree level (ExplorerSidebar) or one qualified list. */
const groupBySchema = computed({
  get: () => settings.get<boolean>('explorer.groupBySchema', true),
  set: (v) => settings.set('explorer.groupBySchema', v),
});

// -- drivers ------------------------------------------------------------------------
// Only a build with downloadable drivers has the section (a dev build carries
// them all inside).
const driverPackages = ref<DriverPackage[] | null>(null);
async function loadDrivers() {
  try {
    const r = await driversApi.packages();
    driverPackages.value = r.on_demand ? r.packages : null;
  } catch {
    driverPackages.value = null;
  }
}
watch(visible, (v) => { if (v) loadDrivers(); }, { immediate: true });

// -- sync ---------------------------------------------------------------------------

const info = computed(() => sync.info);
const cfg = computed(() => info.value?.config ?? null);
const providerLabel = computed(() => tb(info.value?.providers.find((p) => p.kind === cfg.value?.provider)?.label ?? ''));
/** Signed in / folder picked, first upload or restore still to do. */
const pendingSetup = computed(() => !!cfg.value?.provider && !cfg.value.enabled);

const busy = ref<string | null>(null);
const connecting = ref<ProviderKind | null>(null);
const remote = ref<RemoteInfo | null>(null);

async function connect(kind: ProviderKind) {
  let folder: string | null = null;
  if (kind === 'folder') {
    const picked = await openDialog({ directory: true, title: t('settings:sync.folderDialogTitle') });
    if (!picked || Array.isArray(picked)) return;
    folder = picked;
  }
  connecting.value = kind;
  try {
    const r = await syncApi.connect(kind, folder);
    remote.value = r.remote;
    resetForm();
    await sync.refresh();
  } catch (e) {
    if (errorKind(e) !== 'cancelled') ElMessage.error(errorMessage(e));
  } finally {
    connecting.value = null;
  }
}
function cancelConnect() { syncApi.cancelConnect(); }

// Passphrase forms.
const form = reactive({ pass: '', confirm: '', understood: false, replace: false, current: '', next: '', nextConfirm: '' });
function resetForm() {
  Object.assign(form, { pass: '', confirm: '', understood: false, replace: false, current: '', next: '', nextConfirm: '' });
}
const MIN = 10;
function strength(p: string): { label: string; pct: number; color: string } {
  let score = 0;
  if (p.length >= MIN) score++;
  if (p.length >= 16) score++;
  if (p.length >= 24) score++;
  if (/[a-z]/.test(p) && /[A-Z]/.test(p)) score++;
  if (/\d/.test(p)) score++;
  if (/[^\w\s]/.test(p) || /\s/.test(p.trim())) score++;
  if (p.length < MIN) return { label: t('settings:sync.strength.min', { min: MIN }), pct: Math.min(30, p.length * 3), color: 'var(--nm-danger)' };
  if (score <= 2) return { label: t('settings:sync.strength.ok'), pct: 45, color: 'var(--nm-warning)' };
  if (score <= 4) return { label: t('settings:sync.strength.good'), pct: 75, color: 'var(--nm-success)' };
  return { label: t('settings:sync.strength.veryGood'), pct: 100, color: 'var(--nm-success)' };
}
const newOk = computed(() => form.pass.length >= MIN && form.pass === form.confirm && form.understood);

function describe(a: SyncAction): string {
  if (a.action === 'up_to_date') return t('settings:sync.result.upToDate');
  if (a.action === 'uploaded') return a.previous_kept ? t('settings:sync.result.uploadedKept') : t('settings:sync.result.uploaded');
  return t(a.local_backup ? 'settings:sync.result.restoredLocal' : 'settings:sync.result.restored', { device: a.device });
}

async function act(label: string, fn: () => Promise<unknown>) {
  busy.value = label;
  try {
    const r = await fn();
    if (r && typeof r === 'object' && 'action' in r) ElMessage.success(describe(r as SyncAction));
    await sync.refresh();
    return true;
  } catch (e) {
    ElMessage.error(errorMessage(e));
    await sync.refresh();
    return false;
  } finally {
    busy.value = null;
  }
}

async function setupUpload() {
  if (remote.value && form.replace) {
    try {
      await ElMessageBox.confirm(
        t('settings:sync.replaceConfirm', { device: remote.value.device, provider: providerLabel.value }),
        t('settings:sync.replaceTitle'), { confirmButtonText: t('settings:sync.replace'), cancelButtonText: t('common:cancel'), type: 'warning' },
      );
    } catch { return; }
  }
  if (await act('setup', () => syncApi.setup(form.pass, 'upload'))) { resetForm(); remote.value = null; }
}
async function setupRestore() {
  try {
    await ElMessageBox.confirm(
      t('settings:sync.restoreSetupConfirm'),
      t('settings:sync.restoreHere'), { confirmButtonText: t('settings:sync.restore'), cancelButtonText: t('common:cancel'), type: 'warning' },
    );
  } catch { return; }
  if (await act('setup', () => syncApi.setup(form.pass, 'restore'))) { resetForm(); remote.value = null; }
}

async function restoreNow() {
  try {
    await ElMessageBox.confirm(
      t('settings:sync.restoreNowConfirm', { provider: providerLabel.value }),
      t('settings:sync.restoreFromCloud'), { confirmButtonText: t('settings:sync.restore'), cancelButtonText: t('common:cancel'), type: 'warning' },
    );
  } catch { return; }
  await act('restore', () => syncApi.restoreNow());
  loadBackups();
}

async function typePassphrase() {
  if (await act('pass', () => syncApi.setPassphrase(form.pass))) {
    resetForm();
    ElMessage.success(t('settings:sync.passSaved'));
    act('sync', () => syncApi.now());
  }
}

const changing = ref(false);
async function changePassphrase() {
  if (await act('change', () => syncApi.changePassphrase(form.current, form.next))) {
    resetForm();
    changing.value = false;
    ElMessage.success(t('settings:sync.passChanged'));
  }
}

async function disconnect() {
  let deleteRemote = false;
  try {
    await ElMessageBox.confirm(
      t('settings:sync.disconnectConfirm'),
      t('common:disconnect'), {
        confirmButtonText: t('common:disconnect'), cancelButtonText: t('common:cancel'), type: 'warning',
        distinguishCancelAndClose: true,
      },
    );
  } catch { return; }
  try {
    await ElMessageBox.confirm(
      t('settings:sync.deleteRemoteConfirm', { provider: providerLabel.value }),
      t('settings:sync.deleteRemoteTitle'), { confirmButtonText: t('settings:sync.deleteRemote'), cancelButtonText: t('settings:sync.keepRemote'), type: 'warning', distinguishCancelAndClose: true },
    );
    deleteRemote = true;
  } catch (action) {
    if (action === 'close') return;
  }
  await act('disconnect', () => syncApi.disconnect(deleteRemote));
  remote.value = null;
}

/** Before the first sync: just pick another place (nothing to delete). */
async function changeProvider() {
  await act('disconnect', () => syncApi.disconnect(false));
  remote.value = null;
  resetForm();
}

const autoSync = computed({
  get: () => !!cfg.value?.auto,
  set: async (v) => { await syncApi.setAuto(v); await sync.refresh(); },
});

const backups = ref<LocalBackup[]>([]);
async function loadBackups() {
  try { backups.value = await syncApi.localBackups(); } catch { backups.value = []; }
}
async function restoreBackup(b: LocalBackup) {
  try {
    await ElMessageBox.confirm(
      t('settings:sync.restoreLocalConfirm', { date: fmtDate(b.updated_at) }),
      t('settings:sync.restoreLocalTitle'), { confirmButtonText: t('settings:sync.restore'), cancelButtonText: t('common:cancel'), type: 'warning' },
    );
  } catch { return; }
  if (await act('local', () => syncApi.restoreLocal(b.path))) {
    ElMessage.success(t('settings:sync.localRestored'));
    loadBackups();
  }
}

watch(visible, (v) => { if (v) { sync.refresh(); loadBackups(); } }, { immediate: true });

function fmtDate(iso: string | null | undefined) {
  if (!iso) return '';
  const d = new Date(iso);
  return isNaN(d.getTime()) ? iso : d.toLocaleString(locale(), { dateStyle: 'medium', timeStyle: 'short' });
}
function ago(iso: string | null | undefined) {
  if (!iso) return t('settings:sync.never');
  const s = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (s < 60) return t('settings:sync.justNow');
  if (s < 3600) return t('settings:sync.minutesAgo', { n: Math.round(s / 60) });
  if (s < 86400) return t('settings:sync.hoursAgo', { n: Math.round(s / 3600) });
  return fmtDate(iso);
}

const run = computed(() => info.value?.status ?? null);
const statusLine = computed(() => {
  if (!info.value) return null;
  if (run.value?.running) return { kind: 'busy', text: t('settings:sync.syncing') };
  if (run.value?.last_error) return { kind: 'error', text: tb(run.value.last_error) };
  if (info.value.dirty) return { kind: 'dirty', text: t(cfg.value?.auto ? 'settings:sync.dirtyAuto' : 'settings:sync.dirty') };
  return { kind: 'ok', text: t('settings:sync.synced', { ago: ago(info.value.last_sync_at) }) };
});
const needsPassphrase = computed(() => run.value?.last_error_kind === 'wrong_passphrase');
const needsSignIn = computed(() => run.value?.last_error_kind === 'sync_auth');

const PROVIDER_HINT: Record<ProviderKind, string> = {
  google_drive: 'settings:sync.hint.googleDrive',
  onedrive: 'settings:sync.hint.onedrive',
  folder: 'settings:sync.hint.folder',
};
</script>

<template>
  <el-dialog v-model="visible" :title="$t('settings:title')" width="860px" class="st-dialog" append-to-body destroy-on-close>
    <div class="st">
      <nav class="st-nav">
        <button :class="{ on: section === 'general' }" @click="section = 'general'"><el-icon><ei-setting /></el-icon>{{ $t('settings:nav.general') }}</button>
        <button :class="{ on: section === 'sync' }" @click="section = 'sync'"><el-icon><ei-upload-filled /></el-icon>{{ $t('settings:nav.sync') }}</button>
        <button :class="{ on: section === 'mcp' }" @click="section = 'mcp'"><el-icon><ei-cpu /></el-icon>{{ $t('mcp:nav') }}</button>
        <button v-if="driverPackages" :class="{ on: section === 'drivers' }" @click="section = 'drivers'"><el-icon><ei-connection /></el-icon>{{ $t('settings:nav.drivers') }}</button>
        <!-- Voluntary support: opens GitHub Sponsors in the browser. -->
        <button class="st-support" :title="$t('settings:nav.supportHint')" @click="openSupport">
          <span class="st-heart">♥</span>{{ $t('settings:nav.support') }}
        </button>
      </nav>

      <!-- General -->
      <section v-if="section === 'general'" class="st-body">
        <h3>{{ $t('settings:nav.general') }}</h3>
        <p class="st-muted">{{ $t('settings:general.help') }}</p>
        <div class="st-row">
          <div><strong>{{ $t('common:language') }}</strong><span>{{ $t('common:languageHelp') }}</span></div>
          <el-select :model-value="language" style="width: 240px" @update:model-value="pickLanguage">
            <el-option v-for="l in LANGUAGES" :key="l.code" :label="l.name" :value="l.code" />
          </el-select>
        </div>
        <div class="st-row">
          <div><strong>{{ $t('settings:general.copyFormat') }}</strong><span>{{ $t('settings:general.copyFormatHelp') }}</span></div>
          <el-select v-model="copyFormat" style="width: 240px">
            <el-option v-for="f in COPY_FORMATS" :key="f.id" :label="f.name" :value="f.id" />
          </el-select>
        </div>
        <div class="st-row">
          <div><strong>{{ $t('settings:general.maxRows') }}</strong><span>{{ $t('settings:general.maxRowsHelp') }}</span></div>
          <el-select v-model="maxRows" style="width: 240px">
            <el-option v-for="n in MAX_ROWS" :key="n" :label="n.toLocaleString(locale())" :value="n" />
          </el-select>
        </div>
        <div class="st-row">
          <div><strong>{{ $t('settings:general.groupBySchema') }}</strong><span>{{ $t('settings:general.groupBySchemaHelp') }}</span></div>
          <el-switch v-model="groupBySchema" />
        </div>
        <div class="st-row">
          <div><strong>{{ $t('telemetry:settings.label') }}</strong><span>{{ $t('telemetry:settings.help') }}</span></div>
          <el-switch v-model="telemetry" />
        </div>
      </section>

      <!-- The local MCP server (docs/mcp.md) -->
      <section v-else-if="section === 'mcp'" class="st-body">
        <McpSettings />
      </section>

      <!-- Downloadable drivers -->
      <section v-else-if="section === 'drivers' && driverPackages" class="st-body">
        <h3>{{ $t('settings:nav.drivers') }}</h3>
        <DriversSettings :packages="driverPackages" @changed="loadDrivers" />
      </section>

      <!-- Sync -->
      <section v-else class="st-body">
        <h3>{{ $t('settings:sync.heading') }}</h3>
        <p class="st-muted">{{ $t('settings:sync.intro') }}</p>

        <div v-if="cfg?.enabled" class="st-secure-line">
          <el-icon><ei-lock /></el-icon>
          {{ $t('settings:sync.secureLine') }}
        </div>
        <div v-else class="st-secure">
          <el-icon :size="22" class="st-secure-icon"><ei-lock /></el-icon>
          <div>
            <strong>{{ $t('settings:sync.secureTitle') }}</strong>
            <p>
              {{ $t('settings:sync.secureBody') }}
              <b>{{ $t('settings:sync.secureWarning') }}</b>
            </p>
          </div>
        </div>

        <!-- 1. Where -->
        <template v-if="!cfg?.provider">
          <h4>{{ $t('settings:sync.where') }}</h4>
          <div class="st-providers">
            <button
              v-for="p in info?.providers ?? []"
              :key="p.kind"
              class="st-provider"
              :disabled="!p.available || !!connecting"
              @click="connect(p.kind)"
            >
              <span class="st-plogo" :class="p.kind">
                <el-icon v-if="p.kind === 'folder'" :size="20"><ei-folder /></el-icon>
                <template v-else>{{ p.kind === 'google_drive' ? 'G' : 'O' }}</template>
              </span>
              <strong>{{ tb(p.label) }}</strong>
              <span class="st-phint">{{ p.available ? $t(PROVIDER_HINT[p.kind]) : $t('settings:sync.notAvailable') }}</span>
              <span v-if="connecting === p.kind" class="st-pbusy"><el-icon class="is-loading"><ei-loading /></el-icon> {{ p.kind === 'folder' ? $t('settings:sync.checking') : $t('settings:sync.signInBrowser') }}</span>
            </button>
          </div>
          <el-button v-if="connecting && connecting !== 'folder'" style="margin-top: 10px" @click="cancelConnect">{{ $t('common:cancel') }}</el-button>
        </template>

        <!-- 2. First upload or restore -->
        <template v-else-if="pendingSetup">
          <div class="st-account">
            <span class="st-plogo" :class="cfg.provider">
              <el-icon v-if="cfg.provider === 'folder'" :size="18"><ei-folder /></el-icon>
              <template v-else>{{ cfg.provider === 'google_drive' ? 'G' : 'O' }}</template>
            </span>
            <div><strong>{{ providerLabel }}</strong><span>{{ cfg.account }}</span></div>
            <el-button text size="small" @click="changeProvider">{{ $t('settings:sync.change') }}</el-button>
          </div>

          <template v-if="remote && !form.replace">
            <h4>{{ $t('settings:sync.foundBackup') }}</h4>
            <p class="st-muted">
              <i18next :translation="$t('settings:sync.foundBackupBody', { date: fmtDate(remote.updated_at) })">
                <template #device><b>{{ remote.device }}</b></template>
              </i18next>
            </p>
            <el-input v-model="form.pass" type="password" show-password :placeholder="$t('settings:sync.backupPassphrase')" style="max-width: 420px" @keyup.enter="form.pass && setupRestore()" />
            <div class="st-actions">
              <el-button type="primary" :disabled="!form.pass" :loading="busy === 'setup'" @click="setupRestore">{{ $t('settings:sync.restoreHere') }}</el-button>
              <el-button text @click="form.replace = true; form.pass = ''">{{ $t('settings:sync.replaceWithThis') }}</el-button>
            </div>
          </template>

          <template v-else>
            <h4>{{ remote ? $t('settings:sync.replaceHeading') : $t('settings:sync.choosePassphrase') }}</h4>
            <p class="st-muted">{{ $t('settings:sync.passphraseHelp') }}</p>
            <div class="st-pass">
              <el-input v-model="form.pass" type="password" show-password :placeholder="$t('settings:sync.passphrase')" />
              <div class="st-meter"><div :style="{ width: strength(form.pass).pct + '%', background: strength(form.pass).color }" /></div>
              <span class="st-meter-label">{{ form.pass ? strength(form.pass).label : $t('settings:sync.strength.min', { min: MIN }) }}</span>
              <el-input v-model="form.confirm" type="password" show-password :placeholder="$t('settings:sync.repeat')" style="margin-top: 8px" />
              <span v-if="form.confirm && form.confirm !== form.pass" class="st-err">{{ $t('settings:sync.mismatch') }}</span>
              <el-checkbox v-model="form.understood" style="margin-top: 8px">
                {{ $t('settings:sync.understood') }}
              </el-checkbox>
            </div>
            <div class="st-actions">
              <el-button type="primary" :disabled="!newOk" :loading="busy === 'setup'" @click="setupUpload">
                {{ remote ? $t('settings:sync.replaceAndEnable') : $t('settings:sync.enableAndUpload') }}
              </el-button>
              <el-button v-if="remote" text @click="form.replace = false; form.pass = ''; form.confirm = ''">{{ $t('settings:sync.goBack') }}</el-button>
            </div>
          </template>
        </template>

        <!-- 3. Active -->
        <template v-else-if="cfg">
          <div class="st-account">
            <span class="st-plogo" :class="cfg.provider">
              <el-icon v-if="cfg.provider === 'folder'" :size="18"><ei-folder /></el-icon>
              <template v-else>{{ cfg.provider === 'google_drive' ? 'G' : 'O' }}</template>
            </span>
            <div><strong>{{ providerLabel }}</strong><span>{{ cfg.account }}</span></div>
            <span v-if="statusLine" class="st-status" :class="statusLine.kind">
              <el-icon v-if="statusLine.kind === 'busy'" class="is-loading"><ei-loading /></el-icon>
              <el-icon v-else-if="statusLine.kind === 'error'"><ei-warning-filled /></el-icon>
              <el-icon v-else-if="statusLine.kind === 'ok'"><ei-circle-check-filled /></el-icon>
              <el-icon v-else><ei-upload /></el-icon>
              {{ statusLine.text }}
            </span>
          </div>

          <div v-if="needsPassphrase" class="st-callout">
            <p>{{ $t('settings:sync.wrongPassphrase') }}</p>
            <div class="st-inline">
              <el-input v-model="form.pass" type="password" show-password :placeholder="$t('settings:sync.currentBackupPassphrase')" @keyup.enter="form.pass && typePassphrase()" />
              <el-button type="primary" :disabled="!form.pass" :loading="busy === 'pass'" @click="typePassphrase">{{ $t('common:save') }}</el-button>
            </div>
          </div>
          <div v-if="needsSignIn" class="st-callout">
            <p>{{ $t('settings:sync.signInAgain', { provider: providerLabel }) }}</p>
            <el-button type="primary" :loading="connecting === cfg.provider" @click="connect(cfg.provider!)">{{ $t('settings:sync.reconnect') }}</el-button>
          </div>

          <div class="st-row">
            <div><strong>{{ $t('settings:sync.auto') }}</strong><span>{{ $t('settings:sync.autoHelp') }}</span></div>
            <el-switch v-model="autoSync" />
          </div>

          <div class="st-actions">
            <el-button :loading="busy === 'sync'" @click="act('sync', () => syncApi.now())"><el-icon><ei-refresh /></el-icon>&nbsp;{{ $t('settings:sync.syncNow') }}</el-button>
            <el-button :loading="busy === 'upload'" @click="act('upload', () => syncApi.uploadNow())"><el-icon><ei-upload /></el-icon>&nbsp;{{ $t('settings:sync.uploadNow') }}</el-button>
            <el-button :loading="busy === 'restore'" @click="restoreNow"><el-icon><ei-download /></el-icon>&nbsp;{{ $t('settings:sync.restoreFromCloud') }}</el-button>
          </div>

          <h4>{{ $t('settings:sync.passphrase') }}</h4>
          <el-button v-if="!changing" @click="changing = true">{{ $t('settings:sync.changePassphrase') }}</el-button>
          <div v-else class="st-pass">
            <el-input v-model="form.current" type="password" show-password :placeholder="$t('settings:sync.currentPassphrase')" />
            <el-input v-model="form.next" type="password" show-password :placeholder="$t('settings:sync.newPassphrase')" style="margin-top: 8px" />
            <div class="st-meter"><div :style="{ width: strength(form.next).pct + '%', background: strength(form.next).color }" /></div>
            <span class="st-meter-label">{{ form.next ? strength(form.next).label : $t('settings:sync.strength.min', { min: MIN }) }}</span>
            <el-input v-model="form.nextConfirm" type="password" show-password :placeholder="$t('settings:sync.repeatNew')" style="margin-top: 8px" />
            <div class="st-actions">
              <el-button
                type="primary"
                :disabled="!form.current || form.next.length < MIN || form.next !== form.nextConfirm"
                :loading="busy === 'change'"
                @click="changePassphrase"
              >{{ $t('settings:sync.changeAndReencrypt') }}</el-button>
              <el-button text @click="changing = false; resetForm()">{{ $t('common:cancel') }}</el-button>
            </div>
          </div>

          <h4>{{ $t('settings:sync.localCopies') }}</h4>
          <p class="st-muted st-small">{{ $t('settings:sync.localCopiesHelp') }}</p>
          <div v-if="!backups.length" class="st-muted st-small">{{ $t('settings:sync.noneYet') }}</div>
          <div v-for="b in backups" :key="b.path" class="st-backup">
            <span>{{ fmtDate(b.updated_at) }}</span>
            <span class="st-muted">{{ b.device }} · {{ (b.size / 1024).toFixed(0) }} KB</span>
            <el-button size="small" text :loading="busy === 'local'" @click="restoreBackup(b)">{{ $t('settings:sync.restore') }}</el-button>
          </div>

          <h4>{{ $t('common:disconnect') }}</h4>
          <el-button type="danger" plain :loading="busy === 'disconnect'" @click="disconnect">{{ $t('settings:sync.stopSyncing') }}</el-button>
        </template>
      </section>
    </div>
  </el-dialog>
</template>

<style scoped>
.st { display: flex; min-height: 480px; max-height: 70vh; margin: -10px -20px -20px; }
.st-nav { width: 180px; flex: none; padding: 10px 8px; border-right: 1px solid var(--nm-border); display: flex; flex-direction: column; gap: 2px; }
.st-nav button {
  display: flex; align-items: center; gap: 8px; padding: 7px 10px; border: none; border-radius: 4px; background: transparent;
  color: var(--nm-text-dim); font: inherit; font-size: 13px; text-align: left; cursor: pointer;
}
.st-nav button:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.st-nav button.on { background: var(--ide-selection); color: var(--nm-text-strong); }
.st-nav .st-support { margin-top: auto; }
.st-heart { width: 16px; text-align: center; color: #f14c4c; }
.st-body { flex: 1; min-width: 0; overflow: hidden auto; padding: 14px 22px 22px; }
.st-body h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.st-body h4 { margin: 20px 0 8px; font-size: 13px; color: var(--nm-text-strong); }
.st-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.st-small { font-size: 12px; }
.st-row { display: flex; align-items: center; justify-content: space-between; gap: 20px; padding: 12px 0; border-bottom: 1px solid var(--nm-border); }
.st-row > div { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
.st-row strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 500; }
.st-row span { font-size: 12px; color: var(--nm-text-dim); }

.st-secure {
  display: flex; gap: 12px; padding: 12px 14px; margin: 6px 0 4px; border-radius: 6px;
  background: color-mix(in srgb, var(--nm-success) 9%, transparent); border: 1px solid color-mix(in srgb, var(--nm-success) 35%, transparent);
}
.st-secure-line { display: flex; align-items: center; gap: 6px; font-size: 12px; color: var(--nm-success); margin: 2px 0 4px; }
.st-account .st-plogo { color: #fff; }
.st-secure-icon { color: var(--nm-success); flex: none; margin-top: 2px; }
.st-secure strong { font-size: 13px; color: var(--nm-text-strong); }
.st-secure p { margin: 4px 0 0; font-size: 12.5px; line-height: 1.55; color: var(--nm-text); }

.st-providers { display: grid; grid-template-columns: repeat(3, 1fr); gap: 10px; }
.st-provider {
  display: flex; flex-direction: column; align-items: flex-start; gap: 6px; padding: 12px; border-radius: 6px; text-align: left;
  border: 1px solid var(--nm-border); background: var(--ide-panel, transparent); color: var(--nm-text); font: inherit; cursor: pointer;
}
.st-provider:hover:not(:disabled) { border-color: var(--el-color-primary); }
.st-provider:disabled { opacity: 0.55; cursor: not-allowed; }
.st-provider strong { font-size: 13px; color: var(--nm-text-strong); }
.st-phint { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.45; }
.st-pbusy { font-size: 12px; color: var(--el-color-primary); display: flex; align-items: center; gap: 6px; }
.st-plogo {
  display: inline-flex; align-items: center; justify-content: center; width: 30px; height: 30px; border-radius: 6px; flex: none;
  font-weight: 700; font-size: 15px; color: #fff; background: #5f6368;
}
.st-plogo.google_drive { background: #1a73e8; }
.st-plogo.onedrive { background: #0364b8; }

.st-account { display: flex; align-items: center; gap: 10px; padding: 10px 12px; margin-top: 12px; border: 1px solid var(--nm-border); border-radius: 6px; }
.st-account > div { display: flex; flex-direction: column; min-width: 0; flex: 1; }
.st-account strong { font-size: 13px; color: var(--nm-text-strong); }
.st-account span { font-size: 12px; color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; }
.st-status { display: flex; align-items: center; gap: 5px; font-size: 12px; max-width: 45%; text-align: right; }
.st-status.ok { color: var(--nm-success); }
.st-status.error { color: var(--nm-danger); }
.st-status.dirty, .st-status.busy { color: var(--nm-text-dim); }

.st-callout { margin-top: 10px; padding: 10px 12px; border-radius: 6px; border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 8%, transparent); }
.st-callout p { margin: 0 0 8px; font-size: 12.5px; }
.st-inline { display: flex; gap: 8px; max-width: 480px; }
.st-actions { display: flex; flex-wrap: wrap; gap: 8px; margin: 12px 0 8px; }
.st-actions :deep(.el-button + .el-button) { margin-left: 0; }
.st-pass { display: flex; flex-direction: column; max-width: 420px; }
.st-meter { height: 3px; margin-top: 6px; border-radius: 2px; background: var(--nm-border); overflow: hidden; }
.st-meter > div { height: 100%; transition: width 0.2s; }
.st-meter-label { font-size: 11.5px; color: var(--nm-text-dim); margin-top: 3px; }
.st-err { font-size: 12px; color: var(--nm-danger); margin-top: 3px; }
.st-backup { display: flex; align-items: center; gap: 12px; padding: 4px 0; font-size: 12.5px; }
.st-backup .st-muted { margin: 0; flex: 1; }
</style>
