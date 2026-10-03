<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, reactive, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { open as openFile } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { tb } from '../i18n/backend';
import { api, errorMessage } from '../api/client';
import { FAMILY_LABELS, TYPED_FIELDS, type ConnectionConfig, type DriverInfo, type Family, type Field, type SavedConnection } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore, type ConnectionFormTab } from '../stores/tabs';
import EngineIcon from '../components/EngineIcon.vue';
import FolderPicker from '../components/FolderPicker.vue';
import SshTunnelSection, { type SshValues } from '../components/SshTunnelSection.vue';
import { askTrustSshHost } from '../composables/sshTrust';
import { TAG_SUGGESTIONS, tagColor, tagsInUse } from '../composables/tags';
import { mcpApi, type McpLevel } from '../api/mcp';
import { inCodeEditor, isSaveShortcut, modalOpen } from '../composables/shortcuts';

// New / edit connection, as an editor tab. Step 1 picks the engine; step 2 is
// the form the driver declares (its `fields`). Secret fields are never read
// back: when editing, leaving one empty keeps what's in the keychain.

const props = defineProps<{ tab: ConnectionFormTab }>();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const { t } = useTranslation();
function close() {
  tabs.close(props.tab.id);
}

const COLORS = ['', '#3794ff', '#89d185', '#cca700', '#f14c4c', '#c586c0', '#4ec9b0', '#ce9178'];

const editing = computed(() => (props.tab.editId ? conns.byId(props.tab.editId) ?? null : null));
const step = ref<'pick' | 'form'>('pick');
const search = ref('');
/** Family filter of the picker's rail (`null`: every family). */
const family = ref<Family | null>(null);
const driver = ref<DriverInfo | null>(null);
const name = ref('');
const color = ref('');
const savePassword = ref(true);
const folder = ref<string | null>(null);
const tags = ref<string[]>([]);
/** MCP access (docs/mcp.md): '' = the global default. */
const mcpLevel = ref<McpLevel | ''>('');
const mcpDefault = ref<McpLevel>('schema');
const mcpOn = ref(true);
mcpApi.status().then((s) => { mcpDefault.value = s.default_level; mcpOn.value = s.enabled; }).catch(() => {});
/** Why MCP can't go above read here (a `prod` tag, a read-only connection). */
const mcpCap = computed(() => {
  if (tags.value.some((t) => t.trim().toLowerCase() === 'prod')) return 'capProd';
  if (values.read_only === true) return 'capReadOnly';
  return null;
});
/** Tags offered: the usual ones and those already in use. */
const tagOptions = computed(() => {
  const all = [...TAG_SUGGESTIONS, ...tagsInUse(conns.list)];
  return all.filter((t, i) => all.findIndex((x) => x.toLowerCase() === t.toLowerCase()) === i);
});
const values = reactive<Record<string, string | boolean>>({});
/** The SSH tunnel (`ssh.*` options; docs/tuneles-ssh.md). */
const SSH_EMPTY: SshValues = { enabled: false, host: '', port: '', user: '', auth: 'password', password: '', key_path: '', passphrase: '', jump: '', trusted: '' };
const ssh = ref<SshValues>({ ...SSH_EMPTY });
/** Engines reached over the network (a server host, not a file): they can use a tunnel. */
const networked = computed(() => driver.value?.fields.some((f) => f.key === 'host' && f.kind.type !== 'file') ?? false);

// -- tabs: General, SSH, SSL, Advanced (the driver says where each field goes) --
type Section = 'general' | 'ssh' | 'ssl' | 'advanced';
const section = ref<Section>('general');
const sectionOf = (f: Field): Section => f.section ?? 'general';
/** A field applies given the others' values (its `when`). */
function visible(f: Field, depth = 0): boolean {
  if (!f.when) return true;
  const v = values[f.when.key];
  if (!f.when.values.includes(typeof v === 'boolean' ? String(v) : String(v ?? ''))) return false;
  // Chained: the field it depends on must be showing too (IAM → auth mode → keys).
  const parent = driver.value?.fields.find((x) => x.key === f.when!.key);
  return !parent || depth > 8 || visible(parent, depth + 1);
}
const shownFields = computed(() => driver.value?.fields.filter((f) => visible(f)) ?? []);
/** Fields others depend on (the auth method…): a row of their own, so what
 *  they show lines up below them. */
const controllers = computed(() => new Set(driver.value?.fields.map((f) => f.when?.key).filter((k): k is string => !!k) ?? []));
/** A tab's fields, each option's fields right below the field that chooses
 *  them (the auth method, then its user/password/token…). */
function fieldsIn(s: Section): Field[] {
  const here = shownFields.value.filter((f) => sectionOf(f) === s);
  const out: Field[] = [];
  const place = (f: Field) => {
    if (out.includes(f)) return;
    out.push(f);
    for (const d of here) if (d.when?.key === f.key) place(d);
  };
  for (const f of here) {
    // A dependent field goes with the field it depends on (when that one is here).
    if (f.when && here.some((x) => x.key === f.when!.key)) continue;
    place(f);
  }
  return out;
}
const sections = computed<Section[]>(() => {
  const out: Section[] = ['general'];
  if (networked.value) out.push('ssh');
  if (driver.value?.fields.some((f) => sectionOf(f) === 'ssl')) out.push('ssl');
  if (driver.value?.fields.some((f) => sectionOf(f) === 'advanced')) out.push('advanced');
  return out;
});
/** A tab with something turned on (the tunnel, TLS…): a dot on its name. */
function sectionOn(s: Section): boolean {
  if (s === 'ssh') return ssh.value.enabled;
  if (s === 'general') return false;
  return fieldsIn(s).some((f) => (f.kind.type === 'bool' ? values[f.key] === true : String(values[f.key] ?? '') !== '' && String(values[f.key]) !== f.default));
}
const testing = ref(false);
const saving = ref(false);
const testResult = ref<{ ok: boolean; message: string } | null>(null);

watch(() => props.tab.editId, () => {
  testResult.value = null;
  search.value = '';
  family.value = null;
  const c = editing.value;
  const source = c ?? (props.tab.duplicateOf ? conns.byId(props.tab.duplicateOf) ?? null : null);
  if (source) {
    const d = conns.driver(source.config.driver);
    if (!d) { ElMessage.error(t('connection:missingDriver', { driver: source.config.driver })); close(); return; }
    pick(d, source);
    folder.value = source.folder_id;
    // A copy keeps everything but the secrets, which stay with the original.
    if (!c) name.value = t('connection:copyName', { name: source.name });
  } else {
    driver.value = null;
    step.value = 'pick';
    folder.value = props.tab.folderId;
  }
}, { immediate: true });

const matching = computed(() => {
  const q = search.value.trim().toLowerCase();
  return conns.drivers.filter((d) => !q || d.name.toLowerCase().includes(q) || d.id.includes(q));
});

/** Every family in the catalog, with how many engines match the search. */
const allGroups = computed(() => {
  const order = Object.keys(FAMILY_LABELS) as Family[];
  const families = [...new Set(conns.drivers.map((d) => d.family))].sort((a, b) => order.indexOf(a) - order.indexOf(b));
  return families.map((f) => ({ family: f, label: familyLabel(f), matches: matching.value.filter((d) => d.family === f).length }));
});

const groups = computed(() =>
  allGroups.value
    .filter((g) => g.matches && (family.value === null || g.family === family.value))
    .map((g) => ({ ...g, drivers: matching.value.filter((d) => d.family === g.family) })),
);

/** A family's name in the current language. */
function familyLabel(f: Family): string {
  return t(`connection:family.${f}`, { defaultValue: FAMILY_LABELS[f] ?? f });
}

/** Subtitle for engines without a port: file-based or cloud services. */
function engineKind(d: DriverInfo): string {
  return d.fields.some((f) => f.kind.type === 'file') ? t('connection:pick.localFile') : t('connection:pick.cloudService');
}

function pick(d: DriverInfo, existing: SavedConnection | null = null) {
  driver.value = d;
  for (const k of Object.keys(values)) delete values[k];
  for (const f of d.fields) {
    values[f.key] = f.kind.type === 'bool' ? f.default === 'true' : f.key === 'port' ? String(d.default_port || '') : f.default;
  }
  const o = existing?.config.options ?? {};
  ssh.value = {
    ...SSH_EMPTY,
    enabled: o['ssh.enabled'] === 'true', host: o['ssh.host'] ?? '', port: o['ssh.port'] ?? '', user: o['ssh.user'] ?? '',
    auth: (o['ssh.auth'] as SshValues['auth']) || 'password', key_path: o['ssh.key_path'] ?? '', jump: o['ssh.jump'] ?? '', trusted: o['ssh.trusted'] ?? '',
  };
  if (existing) {
    const cfg = existing.config as unknown as Record<string, unknown>;
    for (const f of d.fields) {
      if (f.secret) { values[f.key] = ''; continue; }
      const v = (TYPED_FIELDS as readonly string[]).includes(f.key) ? cfg[f.key] : existing.config.options[f.key];
      if (v === undefined || v === null) continue;
      values[f.key] = f.kind.type === 'bool' ? v === true || v === 'true' : String(f.key === 'port' && v === 0 ? '' : v);
    }
    name.value = existing.name;
    color.value = existing.color ?? '';
    tags.value = [...(existing.tags ?? [])];
    mcpLevel.value = existing.mcp_level ?? '';
    savePassword.value = existing.save_password;
  } else {
    name.value = '';
    color.value = '';
    tags.value = [];
    mcpLevel.value = '';
    savePassword.value = true;
  }
  testResult.value = null;
  section.value = 'general';
  step.value = 'form';
}

const secretFields = computed(() => shownFields.value.filter((f) => f.secret));

function buildConfig(): ConnectionConfig {
  const d = driver.value!;
  const cfg: ConnectionConfig = {
    driver: d.id, host: '', port: 0, database: '', username: null, password: null,
    encrypt: false, trust_server_certificate: false, read_only: false, options: {},
  };
  // Only the fields that apply: another auth method's leftovers aren't saved.
  for (const f of d.fields.filter((f) => visible(f))) {
    const v = values[f.key];
    switch (f.key) {
      case 'host': cfg.host = String(v ?? '').trim(); break;
      case 'port': cfg.port = Number(v) || 0; break;
      case 'database': cfg.database = String(v ?? '').trim(); break;
      case 'username': cfg.username = String(v ?? '').trim() || null; break;
      case 'password': cfg.password = String(v ?? '') || null; break;
      case 'encrypt': cfg.encrypt = !!v; break;
      case 'trust_server_certificate': cfg.trust_server_certificate = !!v; break;
      case 'read_only': cfg.read_only = !!v; break;
      default:
        if (f.kind.type === 'bool') cfg.options[f.key] = v ? 'true' : 'false';
        else if (String(v ?? '') !== '') cfg.options[f.key] = String(v);
    }
  }
  const s = ssh.value;
  if (networked.value && s.enabled) {
    const set = (k: string, v: string) => { if (v.trim()) cfg.options[`ssh.${k}`] = v.trim(); };
    cfg.options['ssh.enabled'] = 'true';
    set('host', s.host); set('port', s.port); set('user', s.user); set('auth', s.auth); set('jump', s.jump);
    if (s.auth === 'key') { set('key_path', s.key_path); if (s.passphrase) cfg.options['ssh.passphrase'] = s.passphrase; }
    if (s.auth === 'password' && s.password) cfg.options['ssh.password'] = s.password;
  }
  if (s.trusted.trim()) cfg.options['ssh.trusted'] = s.trusted.trim();
  return cfg;
}

function defaultName(): string {
  const cfg = buildConfig();
  const where = cfg.host ? cfg.host.split(/[\\/]/).pop() : '';
  return where ? `${driver.value!.name} · ${where}` : driver.value!.name;
}

const missing = computed(() => {
  const out: { label: string; section: Section }[] = shownFields.value
    .filter((f) => f.required && !(f.secret && editing.value) && String(values[f.key] ?? '').trim() === '')
    .map((f) => ({ label: tb(f.label), section: sectionOf(f) }));
  const s = ssh.value;
  if (networked.value && s.enabled) {
    if (!s.host.trim()) out.push({ label: t('tunnel:host'), section: 'ssh' });
    if (!s.user.trim()) out.push({ label: t('tunnel:user'), section: 'ssh' });
    if (s.auth === 'key' && !s.key_path.trim()) out.push({ label: t('tunnel:keyPath'), section: 'ssh' });
  }
  return out;
});

async function test() {
  testing.value = true;
  testResult.value = null;
  try {
    const r = await api.testConnection(buildConfig(), editing.value?.id ?? null);
    testResult.value = { ...r, message: tb(r.message) };
  } catch (e) {
    // The tunnel's SSH server isn't known yet: trust it and test again.
    const fingerprint = await askTrustSshHost(e);
    if (fingerprint) {
      ssh.value.trusted = [ssh.value.trusted, fingerprint].filter(Boolean).join(',');
      testing.value = false;
      return test();
    }
    testResult.value = { ok: false, message: errorMessage(e) };
  } finally {
    testing.value = false;
  }
}

async function save() {
  if (missing.value.length) {
    ElMessage.warning(t('connection:form.missingFields', { fields: missing.value.map((m) => m.label).join(', ') }));
    section.value = missing.value[0].section;
    return;
  }
  saving.value = true;
  try {
    const saved = await conns.save({
      id: editing.value?.id ?? '',
      name: name.value.trim() || defaultName(),
      color: color.value || null,
      config: buildConfig(),
      save_password: savePassword.value,
      folder_id: folder.value,
      tags: tags.value.map((t) => t.trim()).filter(Boolean),
      mcp_level: mcpLevel.value || null,
      updated_at: '',
    });
    close();
    conns.connect(saved.id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  } finally {
    saving.value = false;
  }
}

// ⌘S (Ctrl+S on Windows / Linux) is the form's "Guardar" / "Guardar y
// conectar", when this tab is the visible one and no dialog is open.
const root = ref<HTMLDivElement | null>(null);
function onSaveKey(e: KeyboardEvent) {
  if (!isSaveShortcut(e) || inCodeEditor(e) || !root.value?.offsetParent) return;
  e.preventDefault();
  if (step.value === 'form' && !saving.value && !modalOpen()) save();
}
onMounted(() => window.addEventListener('keydown', onSaveKey));
onBeforeUnmount(() => window.removeEventListener('keydown', onSaveKey));

async function browse(f: Field) {
  try {
    const p = await openFile({ multiple: false, directory: false });
    if (typeof p === 'string') values[f.key] = p;
  } catch { /* no dialog outside Tauri */ }
}

function selectOptions(f: Field): [string, string][] {
  return f.kind.type === 'select' ? f.kind.options : [];
}
</script>

<template>
  <div ref="root" class="cv">
    <div class="cv-body">
    <h2 class="cv-title">{{ editing ? $t('connection:title.edit', { name: editing.name }) : tab.duplicateOf ? $t('connection:title.duplicate') : $t('connection:title.new') }}</h2>
    <!-- step 1: engine -->
    <div v-if="step === 'pick'" class="cd-pick">
      <nav class="cd-rail">
        <button class="cd-rail-item" :class="{ active: family === null }" @click="family = null">
          <span>{{ $t('common:all') }}</span><span class="cd-rail-n">{{ matching.length }}</span>
        </button>
        <button
          v-for="g in allGroups"
          :key="g.family"
          class="cd-rail-item"
          :class="{ active: family === g.family }"
          :disabled="!g.matches"
          @click="family = g.family"
        >
          <span>{{ g.label }}</span><span class="cd-rail-n">{{ g.matches }}</span>
        </button>
      </nav>
      <div class="cd-pick-main">
        <el-input v-model="search" :placeholder="$t('connection:pick.searchPlaceholder')" clearable autofocus size="default">
          <template #prefix><el-icon><ei-search /></el-icon></template>
        </el-input>
        <div class="cd-groups">
          <div v-for="g in groups" :key="g.family" class="cd-group">
            <div v-if="family === null" class="nm-section-title">{{ g.label }}</div>
            <div class="cd-grid">
              <button v-for="d in g.drivers" :key="d.id" class="cd-engine" :data-engine="d.id" :title="d.name" @click="pick(d)">
                <EngineIcon :id="d.id" :name="d.name" :size="26" />
                <span class="cd-engine-text">
                  <span class="cd-engine-name">{{ d.name }}</span>
                  <span class="cd-engine-sub">{{ d.default_port ? $t('connection:pick.port', { port: d.default_port }) : engineKind(d) }}</span>
                </span>
              </button>
            </div>
          </div>
          <div v-if="!groups.length" class="cd-none">
            <el-icon :size="22"><ei-search /></el-icon>
            <span>{{ $t('connection:pick.noMatch', { search }) }}</span>
          </div>
        </div>
      </div>
    </div>

    <!-- step 2: form -->
    <el-form v-else-if="driver" label-position="top" class="cd-form" @submit.prevent>
      <div class="cd-head">
        <EngineIcon :id="driver.id" :name="driver.name" :size="32" />
        <div class="cd-head-text">
          <strong>{{ driver.name }}</strong>
          <span>{{ familyLabel(driver.family) }}</span>
        </div>
        <el-button v-if="!editing" class="cd-change" text @click="step = 'pick'">
          <el-icon><ei-switch /></el-icon><span>{{ $t('connection:form.changeEngine') }}</span>
        </el-button>
      </div>
      <div class="cd-tabs" role="tablist">
        <button
          v-for="s in sections"
          :key="s"
          type="button"
          role="tab"
          class="cd-tab"
          :class="{ active: section === s }"
          :aria-selected="section === s"
          @click="section = s"
        >
          {{ $t(`tunnel:tabs.${s}`) }}<span v-if="sectionOn(s)" class="cd-tab-dot" />
        </button>
      </div>
      <div class="cd-panel">
        <template v-if="section === 'general'">
          <div class="nm-grid-2">
            <el-form-item :label="$t('connection:form.name')">
              <el-input v-model="name" :placeholder="defaultName()" />
            </el-form-item>
            <el-form-item :label="$t('connection:form.color')">
              <div class="cd-colors">
                <button
                  v-for="c in COLORS"
                  :key="c"
                  class="cd-color"
                  :class="{ active: color === c }"
                  :style="{ background: c || 'transparent' }"
                  :title="c ? c : $t('connection:form.noColor')"
                  @click.prevent="color = c"
                >{{ c ? '' : '∅' }}</button>
              </div>
            </el-form-item>
          </div>
          <div class="nm-grid-2">
            <el-form-item :label="$t('connection:form.folder')">
              <FolderPicker v-model="folder" />
            </el-form-item>
            <el-form-item :label="$t('connection:form.tags')">
              <el-select
                v-model="tags"
                multiple
                filterable
                allow-create
                default-first-option
                :reserve-keyword="false"
                :placeholder="$t('connection:form.tagsPlaceholder')"
                style="width: 100%"
              >
                <el-option v-for="tag in tagOptions" :key="tag" :label="tag" :value="tag">
                  <span class="cd-tag-dot" :style="{ background: tagColor(tag) }" />{{ tag }}
                </el-option>
              </el-select>
            </el-form-item>
          </div>
          <el-form-item :label="$t('mcp:connection.label')">
            <el-select v-model="mcpLevel" style="width: 100%">
              <el-option :label="$t('mcp:connection.useDefault', { level: $t(`mcp:level.${mcpDefault}`) })" value="" />
              <el-option v-for="l in (['disabled', 'schema', 'read', 'write'] as const)" :key="l" :label="$t(`mcp:level.${l}`)" :value="l" />
            </el-select>
            <div style="font-size: 12px; line-height: 1.5; color: var(--nm-text-dim); margin-top: 4px">
              {{ $t('mcp:connection.help') }}
              <template v-if="mcpLevel === 'write' && !mcpCap"><br>{{ $t('mcp:levelHelp.write') }}</template>
              <template v-if="mcpCap"><br>{{ $t(`mcp:connection.${mcpCap}`) }}</template>
              <template v-if="!mcpOn"><br>{{ $t('mcp:connection.serverOff') }}</template>
            </div>
          </el-form-item>
        </template>
        <SshTunnelSection v-if="section === 'ssh'" v-model="ssh" :editing="!!editing" />
        <template v-else>
          <div class="cd-fields">
            <el-form-item
              v-for="f in fieldsIn(section)"
              :key="f.key"
              :label="f.kind.type === 'bool' ? '' : tb(f.label)"
              :required="f.required && !(f.secret && editing)"
              :class="{ 'cd-wide': ['textarea', 'file'].includes(f.kind.type) || f.key === 'host' || controllers.has(f.key), 'cd-bool': f.kind.type === 'bool' }"
            >
              <el-checkbox v-if="f.kind.type === 'bool'" v-model="values[f.key] as boolean">{{ tb(f.label) }}</el-checkbox>
              <el-select v-else-if="f.kind.type === 'select'" v-model="values[f.key] as string" style="width: 100%">
                <el-option v-for="[v, l] in selectOptions(f)" :key="v" :label="tb(l)" :value="v" />
              </el-select>
              <el-input
                v-else-if="f.kind.type === 'textarea'"
                v-model="values[f.key] as string"
                type="textarea"
                :rows="4"
                :placeholder="f.secret && editing ? $t('connection:form.secretKeptMasc') : tb(f.placeholder)"
              />
              <el-input
                v-else
                v-model="values[f.key] as string"
                :type="f.kind.type === 'password' ? 'password' : 'text'"
                :show-password="f.kind.type === 'password'"
                :placeholder="f.secret && editing ? $t('connection:form.secretKeptFem') : f.key === 'port' ? String(driver.default_port || '') : tb(f.placeholder)"
              >
                <template v-if="f.kind.type === 'file'" #append>
                  <el-button @click="browse(f)"><el-icon><ei-folder-opened /></el-icon></el-button>
                </template>
              </el-input>
              <div v-if="f.help" class="cd-help">{{ tb(f.help) }}</div>
            </el-form-item>
          </div>
          <div v-if="section !== 'general' && !fieldsIn(section).length" class="cd-empty">{{ $t('tunnel:tabs.empty') }}</div>
        </template>
        <el-checkbox v-if="section === 'general' && secretFields.length" v-model="savePassword">
          {{ $t('connection:form.savePassword', { fields: secretFields.map((f) => tb(f.label).toLowerCase()).join($t('connection:form.and')) }) }}
        </el-checkbox>
      </div>
      <div v-if="testResult && !testResult.ok" class="cd-test-error" role="alert">
        <el-icon><ei-circle-close-filled /></el-icon>
        <div><strong>{{ $t('connection:form.couldNotConnect') }}</strong><p>{{ testResult.message }}</p></div>
      </div>
    </el-form>

    </div>
    <footer class="cv-foot">
      <div v-if="step === 'form'" class="cd-footer">
        <el-button :loading="testing" @click="test">
          <el-icon v-if="!testing"><ei-connection /></el-icon><span>{{ $t('connection:form.testConnection') }}</span>
        </el-button>
        <span v-if="testResult?.ok" class="cd-test-ok" :title="testResult.message">
          <el-icon><ei-circle-check-filled /></el-icon>{{ $t('connection:form.connectionOk') }}<span class="cd-test-msg">· {{ testResult.message }}</span>
        </span>
        <span class="cd-spacer" />
        <el-button @click="close">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" :loading="saving" @click="save">{{ editing ? $t('common:save') : $t('connection:form.saveAndConnect') }}</el-button>
      </div>
      <div v-else class="cd-footer">
        <span class="cd-footer-hint">{{ $t('connection:pick.available', { n: conns.drivers.length }) }}</span>
        <span class="cd-spacer" />
        <el-button @click="close">{{ $t('common:cancel') }}</el-button>
      </div>
    </footer>
  </div>
</template>

<style scoped>
.cd-tag-dot { display: inline-block; width: 8px; height: 8px; border-radius: 50%; margin-right: 8px; vertical-align: 0; }
/* the tab */
.cv { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--ide-editor); }
.cv-body { flex: 1; min-height: 0; overflow: auto; width: 100%; padding: 18px 24px 12px; box-sizing: border-box; display: flex; flex-direction: column; }
.cv-title { margin: 0 0 14px; font-size: 17px; font-weight: 600; color: var(--nm-text-strong); }
.cv-foot { flex: none; border-top: 1px solid var(--nm-border); padding: 10px 24px; }


/* step 1 */
.cd-pick { display: flex; gap: 14px; flex: 1; min-height: 360px; }
.cd-rail { display: flex; flex-direction: column; gap: 1px; width: 168px; flex-shrink: 0; overflow-y: auto; }
.cd-rail-item {
  display: flex; align-items: center; justify-content: space-between; gap: 8px;
  padding: 6px 10px; border: 0; border-radius: 3px; background: none; cursor: pointer;
  color: var(--nm-text); font: inherit; text-align: left;
}
.cd-rail-item:hover:not(:disabled) { background: var(--ide-hover); }
.cd-rail-item.active { background: var(--ide-selection); color: var(--nm-text-strong); }
.cd-rail-item:disabled { color: var(--nm-text-muted); cursor: default; }
.cd-rail-n { font-size: 11px; color: var(--nm-text-muted); font-variant-numeric: tabular-nums; }
.cd-pick-main { flex: 1; min-width: 0; display: flex; flex-direction: column; }
.cd-groups { flex: 1; overflow-y: auto; overflow-x: hidden; margin-top: 10px; padding-right: 4px; }
.cd-group { margin-bottom: 14px; }
.cd-group .nm-section-title { margin-bottom: 6px; }
.cd-grid { display: grid; grid-template-columns: repeat(auto-fill, minmax(176px, 1fr)); gap: 6px; }
.cd-engine {
  display: flex; align-items: center; gap: 10px; padding: 8px 10px; min-width: 0;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev);
  color: var(--nm-text); cursor: pointer; text-align: left; font: inherit;
  transition: border-color 0.12s, background 0.12s;
}
.cd-engine:hover { border-color: var(--ide-focus); background: var(--ide-hover); }
.cd-engine:focus-visible { outline: 1px solid var(--ide-focus); outline-offset: 1px; }
.cd-engine-text { display: flex; flex-direction: column; min-width: 0; line-height: 1.25; }
.cd-engine-name { color: var(--nm-text-strong); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.cd-engine-sub { font-size: 11px; color: var(--nm-text-dim); }
.cd-none { display: flex; flex-direction: column; align-items: center; gap: 8px; padding: 50px 0; color: var(--nm-text-dim); }

/* step 2 */
.cd-head {
  display: flex; align-items: center; gap: 12px; margin: -4px 0 16px; padding: 10px 12px;
  border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev);
}
.cd-head-text { display: flex; flex-direction: column; line-height: 1.3; flex: 1; min-width: 0; }
.cd-head-text strong { color: var(--nm-text-strong); font-size: 14px; font-weight: 600; }
.cd-head-text span { font-size: 11.5px; color: var(--nm-text-dim); }
.cd-change span { margin-left: 4px; }
/* tabs: General, SSH, SSL, Advanced */
.cd-tabs { display: flex; gap: 2px; border-bottom: 1px solid var(--nm-border); margin: 0 0 14px; }
.cd-tab {
  position: relative; display: inline-flex; align-items: center; gap: 6px; padding: 7px 14px; border: 0; background: none;
  color: var(--nm-text-dim); font: inherit; font-size: 13px; cursor: pointer; border-bottom: 2px solid transparent; margin-bottom: -1px;
}
.cd-tab:hover { color: var(--nm-text-strong); }
.cd-tab.active { color: var(--nm-text-strong); border-bottom-color: var(--ide-focus, var(--el-color-primary)); }
.cd-tab-dot { width: 6px; height: 6px; border-radius: 50%; background: var(--nm-success); }
.cd-panel { max-width: 1100px; }
.cd-empty { color: var(--nm-text-dim); font-size: 12.5px; padding: 12px 0; }
.cd-fields { display: grid; grid-template-columns: 1fr 1fr; column-gap: 16px; }
.cd-wide { grid-column: 1 / -1; }
.cd-bool { margin-bottom: 4px; }
.cd-help { font-size: 11.5px; color: var(--nm-text-dim); line-height: 1.4; margin-top: 2px; }
.cd-colors { display: flex; gap: 6px; }
.cd-color {
  width: 20px; height: 20px; border-radius: 50%; border: 1px solid var(--nm-border);
  cursor: pointer; color: var(--nm-text-dim); font-size: 11px; padding: 0;
}
.cd-color.active { outline: 2px solid #fff; outline-offset: 1px; }
.cd-test-error {
  display: flex; gap: 8px; margin-top: 12px; padding: 8px 10px; border-radius: 3px;
  border: 1px solid color-mix(in srgb, var(--nm-danger) 50%, transparent);
  background: color-mix(in srgb, var(--nm-danger) 10%, transparent);
}
.cd-test-error .el-icon { color: var(--nm-danger); margin-top: 2px; flex-shrink: 0; }
.cd-test-error strong { color: var(--nm-text-strong); font-weight: 600; }
.cd-test-error p { margin: 2px 0 0; color: var(--nm-text); white-space: pre-wrap; word-break: break-word; user-select: text; cursor: text; }

/* footer */
.cd-footer { display: flex; align-items: center; gap: 8px; }
.cd-footer .el-button { margin: 0; min-width: 84px; }
.cd-footer .el-button span { margin-left: 4px; }
.cd-footer .el-button .el-icon + span { margin-left: 5px; }
.cd-spacer { flex: 1; }
.cd-footer-hint { font-size: 11.5px; color: var(--nm-text-muted); }
.cd-test-ok {
  display: inline-flex; align-items: center; gap: 5px; min-width: 0; max-width: 300px;
  color: var(--nm-success); font-size: 12px; white-space: nowrap; overflow: hidden;
}
.cd-test-msg { color: var(--nm-text-dim); overflow: hidden; text-overflow: ellipsis; }
</style>
