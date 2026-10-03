<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { open as openDialog } from '@tauri-apps/plugin-dialog';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { projectsApi } from '../api/projects';
import type { ProjectInfo, ProjectTarget } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useProjectsStore } from '../stores/projects';
import ContextMenu, { type MenuItem } from './ContextMenu.vue';
import EngineIcon from './EngineIcon.vue';
import ProjectChanges from './ProjectChanges.vue';
import ProjectFileTree from './ProjectFileTree.vue';

// The Proyectos sidebar: the git repos of scripts linked on this machine.
// Each project shows its branch and how far it is from the remote, its
// active base (where its files run: a connection › database, directly or
// through the environments of its .dbine.json), its files and its changes.

const projects = useProjectsStore();
const conns = useConnectionsStore();
const { t } = useTranslation();

const STATUS_EVERY_MS = 15_000;
let timer: ReturnType<typeof setInterval> | null = null;
const onFocus = () => { void projects.refreshAllStatus(); };
onMounted(async () => {
  await projects.load();
  void projects.refreshAllStatus();
  for (const p of projects.list) {
    if (!projects.expandedOf(p.id).open) continue;
    void projects.loadDir(p.id, '');
    for (const d of projects.expandedOf(p.id).dirs) void projects.loadDir(p.id, d);
  }
  timer = setInterval(() => { if (document.visibilityState === 'visible') void projects.refreshAllStatus(); }, STATUS_EVERY_MS);
  window.addEventListener('focus', onFocus);
});
onBeforeUnmount(() => {
  if (timer) clearInterval(timer);
  window.removeEventListener('focus', onFocus);
});

// -- "Mostrar en Proyectos" (Explorer, tabs, editor): open and scroll to it ---------------
const listEl = ref<HTMLElement | null>(null);
watch(() => projects.focus?.seq, async () => {
  const id = projects.focus?.id;
  if (!id) return;
  void projects.loadDir(id, '');
  await nextTick();
  listEl.value?.querySelector(`[data-project="${id}"]`)?.scrollIntoView({ block: 'nearest' });
  flash.value = id;
  setTimeout(() => { if (flash.value === id) flash.value = null; }, 1200);
}, { immediate: true });
const flash = ref<string | null>(null);

function toggle(p: ProjectInfo) {
  const e = projects.expandedOf(p.id);
  e.open = !e.open;
  projects.saveExpanded();
  if (e.open) { void projects.loadDir(p.id, ''); void projects.refreshStatus(p.id); }
}
function toggleSection(p: ProjectInfo, s: 'files' | 'changes') {
  const e = projects.expandedOf(p.id);
  e[s] = !e[s];
  projects.saveExpanded();
}

const status = (id: string) => projects.status[id]?.data ?? null;
function branchLabel(id: string): string {
  const s = status(id);
  if (!s || !s.git || !s.is_repo) return '';
  if (s.detached) return t('projects:git.detached', { head: s.head ?? '' });
  return s.branch ?? '';
}
const targetLabel = (x: ProjectTarget | null | undefined) =>
  x ? `${conns.byId(x.connection_id)?.name ?? t('projects:base.missingConnection')}${x.database ? ` › ${x.database}` : ''}` : '';
const driverOfTarget = (x: ProjectTarget | null | undefined) => (x ? conns.byId(x.connection_id)?.config.driver ?? '' : '');

function pickBase(p: ProjectInfo, alias: string | null) {
  projects.dialog = { kind: 'target', projectId: p.id, alias };
}
async function chooseEnv(p: ProjectInfo, alias: string) {
  if (!p.binding.environments[alias]) { pickBase(p, alias); return; }
  await projects.setEnvironment(p.id, alias);
}

// -- header and per-project actions --------------------------------------------------------
function link() { projects.dialog = { kind: 'link', mode: 'link', binding: null, target: null }; }
function clone() { projects.dialog = { kind: 'link', mode: 'clone', binding: null, target: null }; }
const refreshing = ref(false);
async function refreshAll() {
  refreshing.value = true;
  try {
    await projects.load(true);
    await projects.refreshAllStatus(true);
    for (const p of projects.list) for (const d of ['', ...projects.expandedOf(p.id).dirs]) if (projects.dirOf(p.id, d)) void projects.loadDir(p.id, d, true);
  } finally {
    refreshing.value = false;
  }
}

async function prompt(title: string, label: string, value = '', placeholder = ''): Promise<string | null> {
  try {
    const r = await ElMessageBox.prompt(label, title, {
      inputValue: value, inputPlaceholder: placeholder, confirmButtonText: t('common:accept'), cancelButtonText: t('common:cancel'),
      inputValidator: (v: string) => !!v?.trim() || t('projects:menu.required'),
    });
    return r.value.trim();
  } catch {
    return null;
  }
}
async function rename(p: ProjectInfo) {
  const name = await prompt(t('projects:menu.renameTitle'), t('projects:menu.renameLabel'), p.name);
  if (!name || name === p.name) return;
  try { await projects.rename(p.id, name); } catch (e) { ElMessage.error(errorMessage(e)); }
}
async function addRemote(p: ProjectInfo) {
  const url = await prompt(t('projects:menu.remoteTitle'), t('projects:menu.remoteLabel'), '', 'git@github.com:empresa/scripts.git');
  if (!url) return;
  try {
    await projectsApi.setRemote(p.id, url);
    ElMessage.success({ message: t('projects:menu.remoteSet'), duration: 2000 });
    void projects.refreshStatus(p.id);
  } catch (e) { ElMessage.error(errorMessage(e)); }
}
async function relocate(p: ProjectInfo) {
  let picked: string | string[] | null = null;
  try { picked = await openDialog({ directory: true, title: t('projects:menu.relocateTitle', { name: p.name }) }); } catch { return; }
  if (typeof picked !== 'string') return;
  try { await projects.relocate(p.id, picked); } catch (e) { ElMessage.error(errorMessage(e)); }
}
/** The folder lost its .git: start a new repository there (linked again, same name and base). */
async function initRepo(p: ProjectInfo) {
  try {
    await ElMessageBox.confirm(t('projects:banner.initAsk', { path: p.path }), t('projects:banner.initTitle'), {
      confirmButtonText: t('projects:banner.init'), cancelButtonText: t('common:cancel'),
    });
  } catch { return; }
  try {
    await projectsApi.unlink(p.id);
    projects.list = projects.list.filter((x) => x.id !== p.id);
    await projects.link({ path: p.path, name: p.name, init: true, binding: p.binding });
  } catch (e) { ElMessage.error(errorMessage(e)); void projects.load(true); }
}
function commitFocus(p: ProjectInfo) {
  const e = projects.expandedOf(p.id);
  e.open = true;
  e.changes = true;
  projects.saveExpanded();
  nextTick(() => listEl.value?.querySelector<HTMLTextAreaElement>(`[data-project="${p.id}"] .pc-commit textarea`)?.focus());
}

const menu = ref<{ x: number; y: number; items: MenuItem[] } | null>(null);
function openMenu(ev: MouseEvent, p: ProjectInfo) {
  ev.preventDefault();
  ev.stopPropagation();
  const s = status(p.id);
  const busy = !!projects.busy[p.id];
  const remoteOff = busy || !s?.has_remote || !!s?.detached;
  const items: MenuItem[] = [
    { label: t('projects:op.pull'), disabled: remoteOff, action: () => projects.remoteOp(p.id, 'pull') },
    { label: t('projects:op.push'), disabled: remoteOff, action: () => projects.remoteOp(p.id, 'push') },
    { label: t('projects:op.fetch'), disabled: busy || !s?.has_remote, action: () => projects.remoteOp(p.id, 'fetch') },
    { label: t('projects:menu.commit'), action: () => commitFocus(p) },
    { label: t('projects:menu.openFolder'), divided: true, action: () => projectsApi.reveal(p.id).catch((e) => ElMessage.error(errorMessage(e))) },
    { label: t('projects:menu.rename'), action: () => rename(p) },
    { label: t('projects:menu.environments'), action: () => { projects.dialog = { kind: 'environments', projectId: p.id }; } },
    { label: s?.has_remote ? t('projects:menu.changeRemote') : t('projects:menu.addRemote'), action: () => addRemote(p) },
    { label: t('projects:menu.relocate'), action: () => relocate(p) },
    { label: t('projects:menu.unlink'), danger: true, divided: true, action: () => projects.unlink(p.id) },
  ];
  menu.value = { x: ev.clientX, y: ev.clientY, items };
}

const changesOf = (id: string) => status(id)?.changes.length ?? 0;
const empty = computed(() => projects.loaded && !projects.list.length);
</script>

<template>
  <div class="ps">
    <div class="ps-header">
      <span class="ps-title">{{ $t('projects:title') }}</span>
      <div style="flex: 1" />
      <button class="ps-icon" :title="$t('projects:link.action')" @click="link"><el-icon><ei-plus /></el-icon></button>
      <button class="ps-icon" :title="$t('projects:clone.action')" @click="clone"><el-icon><ei-download /></el-icon></button>
      <button class="ps-icon" :title="$t('projects:refreshAll')" @click="refreshAll">
        <el-icon :class="{ 'is-loading': refreshing }"><ei-refresh /></el-icon>
      </button>
    </div>

    <div ref="listEl" class="ps-list">
      <div v-if="projects.loadError" class="ps-banner err">{{ projects.loadError }}</div>
      <div v-if="empty" class="ps-empty">
        <p>{{ $t('projects:empty.lead') }}</p>
        <p class="ps-muted">{{ $t('projects:empty.help') }}</p>
        <el-button size="small" type="primary" @click="link">{{ $t('projects:link.action') }}</el-button>
        <el-button size="small" @click="clone">{{ $t('projects:clone.action') }}</el-button>
      </div>

      <section v-for="p in projects.list" :key="p.id" class="ps-proj" :class="{ flash: flash === p.id }" :data-project="p.id">
        <div class="ps-row" :title="p.path" @click="toggle(p)" @contextmenu="openMenu($event, p)">
          <el-icon class="ps-caret" :class="{ open: projects.expandedOf(p.id).open }"><ei-arrow-right /></el-icon>
          <span class="ps-name">{{ p.name }}</span>
          <span v-if="branchLabel(p.id)" class="ps-branch" :class="{ warn: status(p.id)?.detached }"><el-icon><ei-share /></el-icon>{{ branchLabel(p.id) }}</span>
          <span v-if="status(p.id)?.ahead" class="ps-ab" :title="$t('projects:git.aheadTip', { count: status(p.id)!.ahead })">↑{{ status(p.id)!.ahead }}</span>
          <span v-if="status(p.id)?.behind" class="ps-ab" :title="$t('projects:git.behindTip', { count: status(p.id)!.behind })">↓{{ status(p.id)!.behind }}</span>
          <el-icon v-if="projects.busy[p.id]" class="ps-busy is-loading" :title="$t(`projects:op.${projects.busy[p.id]}`)"><ei-loading /></el-icon>
          <span class="ps-acts">
            <button
              class="ps-icon"
              :disabled="!!projects.busy[p.id] || !status(p.id)?.has_remote || !!status(p.id)?.detached"
              :title="$t('projects:op.sync')"
              @click.stop="projects.remoteOp(p.id, 'sync')"
            ><el-icon><ei-refresh /></el-icon></button>
            <button class="ps-icon" :title="$t('projects:menu.more')" @click.stop="openMenu($event, p)"><el-icon><ei-more-filled /></el-icon></button>
          </span>
          <span v-if="changesOf(p.id)" class="ps-badge" :title="$t('projects:changes.badgeTip', { count: changesOf(p.id) })">{{ changesOf(p.id) }}</span>
        </div>

        <template v-if="projects.expandedOf(p.id).open">
          <!-- Where its files run -->
          <div class="ps-base">
            <span class="ps-base-label">{{ $t('projects:base.label') }}</span>
            <div v-if="p.manifest?.environments.length" class="ps-envs" role="radiogroup">
              <div
                v-for="e in p.manifest.environments"
                :key="e.name"
                class="ps-env"
                :class="{ on: p.binding.active_environment === e.name }"
                role="radio"
                :aria-checked="p.binding.active_environment === e.name"
                :title="e.description || e.name"
                @click="chooseEnv(p, e.name)"
              >
                <span class="ps-radio" />
                <span class="ps-env-name" :class="{ warn: e.confirm_run }">{{ e.name }}</span>
                <span class="ps-arrow">→</span>
                <template v-if="p.binding.environments[e.name]">
                  <EngineIcon :id="driverOfTarget(p.binding.environments[e.name])" name="" :size="12" />
                  <span class="ps-target" :class="{ missing: !conns.byId(p.binding.environments[e.name].connection_id) }">{{ targetLabel(p.binding.environments[e.name]) }}</span>
                </template>
                <span v-else class="ps-target ps-muted">{{ $t('projects:base.unassigned') }}</span>
                <button class="ps-icon ps-pen" :title="$t('projects:base.mapEnv', { env: e.name })" @click.stop="pickBase(p, e.name)"><el-icon><ei-edit-pen /></el-icon></button>
              </div>
              <div v-if="projects.activeTarget(p.id).problem === 'missing-env'" class="ps-banner warn">
                {{ $t('projects:base.missingEnv', { env: p.binding.active_environment ?? '' }) }}
              </div>
            </div>
            <button v-else class="ps-direct" @click="pickBase(p, null)">
              <template v-if="p.binding.direct">
                <EngineIcon :id="driverOfTarget(p.binding.direct)" name="" :size="12" />
                <span class="ps-target" :class="{ missing: !conns.byId(p.binding.direct.connection_id) }">{{ targetLabel(p.binding.direct) }}</span>
              </template>
              <span v-else class="ps-target ps-pick">{{ $t('projects:base.choose') }}</span>
              <el-icon class="ps-pen"><ei-edit-pen /></el-icon>
            </button>
          </div>

          <!-- Problems -->
          <div v-if="status(p.id) && !status(p.id)!.git" class="ps-banner warn">{{ $t('projects:banner.noGit') }}</div>
          <div v-if="!p.exists" class="ps-banner err">
            {{ $t('projects:banner.missing') }}
            <div class="ps-banner-acts">
              <el-button size="small" @click="relocate(p)">{{ $t('projects:menu.relocate') }}</el-button>
              <el-button size="small" @click="projects.unlink(p.id)">{{ $t('projects:menu.unlink') }}</el-button>
            </div>
          </div>
          <div v-else-if="!p.is_repo" class="ps-banner warn">
            {{ $t('projects:banner.notRepo') }}
            <div class="ps-banner-acts">
              <el-button size="small" @click="initRepo(p)">{{ $t('projects:banner.init') }}</el-button>
              <el-button size="small" @click="projects.unlink(p.id)">{{ $t('projects:menu.unlink') }}</el-button>
            </div>
          </div>
          <div v-if="p.manifest_error" class="ps-banner err">{{ $t('projects:banner.manifestError', { error: p.manifest_error }) }}</div>
          <div v-for="w in p.manifest_warnings" :key="w" class="ps-banner warn">{{ w }}</div>
          <div v-if="status(p.id)?.fetch_error" class="ps-banner warn">{{ $t('projects:banner.fetchError', { error: status(p.id)!.fetch_error }) }}</div>
          <div v-if="projects.status[p.id]?.error" class="ps-banner err">{{ projects.status[p.id]!.error }}</div>

          <template v-if="p.exists">
            <div class="ps-sec" @click="toggleSection(p, 'files')">
              <el-icon class="ps-caret" :class="{ open: projects.expandedOf(p.id).files }"><ei-arrow-right /></el-icon>
              {{ $t('projects:files.title') }}
            </div>
            <ProjectFileTree v-if="projects.expandedOf(p.id).files" :project-id="p.id" />

            <template v-if="p.is_repo && status(p.id)?.git !== false">
              <div class="ps-sec" @click="toggleSection(p, 'changes')">
                <el-icon class="ps-caret" :class="{ open: projects.expandedOf(p.id).changes }"><ei-arrow-right /></el-icon>
                {{ $t('projects:changes.title', { count: changesOf(p.id) }) }}
                <span v-if="status(p.id)?.operation" class="ps-op">{{ $t(`projects:operation.${status(p.id)!.operation}`) }}</span>
              </div>
              <ProjectChanges v-if="projects.expandedOf(p.id).changes" :project-id="p.id" />
              <div v-if="status(p.id)?.last_commit" class="ps-last" :title="$t('projects:git.lastCommit')">{{ status(p.id)!.last_commit }}</div>
            </template>
          </template>
        </template>
      </section>
    </div>
    <ContextMenu v-if="menu" :x="menu.x" :y="menu.y" :items="menu.items" @close="menu = null" />
  </div>
</template>

<style scoped>
.ps { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--ide-sidebar); font-size: 13px; }
.ps-header { display: flex; align-items: center; gap: 2px; height: 35px; padding: 0 8px 0 20px; flex: none; }
.ps-title { font-size: 11px; font-weight: 600; letter-spacing: 0.05em; text-transform: uppercase; color: var(--nm-text-dim); }
.ps-icon {
  display: inline-flex; align-items: center; justify-content: center; width: 22px; height: 22px; border: none; border-radius: 4px;
  background: transparent; color: var(--nm-text-dim); cursor: pointer; flex: none;
}
.ps-icon:hover:not(:disabled) { background: var(--ide-hover); color: var(--nm-text-strong); }
.ps-icon:disabled { opacity: 0.4; cursor: default; }
.ps-list { flex: 1; min-height: 0; overflow: auto; padding-bottom: 12px; }
.ps-empty { padding: 10px 16px; line-height: 1.5; color: var(--nm-text); }
.ps-empty p { margin: 0 0 8px; }
.ps-muted { color: var(--nm-text-dim); font-size: 12px; }
.ps-proj { border-top: 1px solid var(--nm-border); }
.ps-proj:first-of-type { border-top: 0; }
.ps-proj.flash > .ps-row { background: var(--ide-selection); transition: background 0.3s; }
.ps-row { display: flex; align-items: center; gap: 6px; height: 26px; padding: 0 8px 0 6px; cursor: pointer; user-select: none; }
.ps-row:hover { background: var(--ide-hover); }
.ps-caret { flex: none; font-size: 11px; color: var(--nm-text-dim); transition: transform 0.1s; }
.ps-caret.open { transform: rotate(90deg); }
.ps-name { flex: 0 1 auto; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-weight: 600; color: var(--nm-text-strong); }
.ps-branch { display: inline-flex; align-items: center; gap: 3px; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 11.5px; color: var(--nm-text-dim); }
.ps-branch.warn { color: var(--nm-warning); }
.ps-ab { flex: none; font-size: 11px; color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.ps-busy { flex: none; color: var(--nm-text-dim); }
.ps-acts { display: none; margin-left: auto; }
.ps-row:hover .ps-acts { display: inline-flex; }
.ps-badge {
  margin-left: auto; flex: none; min-width: 16px; height: 16px; padding: 0 5px; box-sizing: border-box; border-radius: 8px;
  background: var(--ide-focus, var(--nm-accent)); color: #fff; font-size: 10.5px; line-height: 16px; text-align: center;
}
.ps-row:hover .ps-acts + .ps-badge { margin-left: 0; }
.ps-base { padding: 2px 10px 6px 22px; }
.ps-base-label { display: block; font-size: 11px; color: var(--nm-text-dim); margin-bottom: 2px; }
.ps-envs { display: flex; flex-direction: column; gap: 1px; }
.ps-env { display: flex; align-items: center; gap: 5px; height: 22px; padding: 0 4px; border-radius: 3px; cursor: pointer; min-width: 0; }
.ps-env:hover { background: var(--ide-hover); }
.ps-env.on { background: color-mix(in srgb, var(--ide-focus, var(--nm-accent)) 16%, transparent); }
.ps-radio { flex: none; width: 9px; height: 9px; border-radius: 50%; border: 1px solid var(--nm-text-dim); box-sizing: border-box; }
.ps-env.on .ps-radio { border: 3px solid var(--ide-focus, var(--nm-accent)); }
.ps-env-name { flex: none; font-weight: 600; font-size: 12px; color: var(--nm-text-strong); }
.ps-env-name.warn { color: var(--nm-danger); }
.ps-arrow { flex: none; color: var(--nm-text-muted); font-size: 11px; }
.ps-target { min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; font-size: 12px; color: var(--nm-text); }
.ps-target.missing { color: var(--nm-danger); }
.ps-pick { color: var(--nm-accent); }
.ps-pen { margin-left: auto; opacity: 0; font-size: 12px; }
.ps-env:hover .ps-pen, .ps-direct:hover .ps-pen { opacity: 1; }
.ps-direct {
  display: flex; align-items: center; gap: 5px; width: 100%; height: 22px; padding: 0 4px; border: 0; border-radius: 3px;
  background: none; font: inherit; color: inherit; cursor: pointer; text-align: left;
}
.ps-direct:hover { background: var(--ide-hover); }
.ps-banner { margin: 2px 10px 6px 22px; padding: 5px 8px; border-radius: 3px; font-size: 12px; line-height: 1.4; color: var(--nm-text); word-break: break-word; }
.ps-banner.warn { border: 1px solid color-mix(in srgb, var(--nm-warning) 45%, transparent); background: color-mix(in srgb, var(--nm-warning) 10%, transparent); }
.ps-banner.err { border: 1px solid color-mix(in srgb, var(--nm-danger) 45%, transparent); background: color-mix(in srgb, var(--nm-danger) 10%, transparent); }
.ps-banner-acts { display: flex; gap: 6px; margin-top: 6px; }
.ps-sec {
  display: flex; align-items: center; gap: 5px; height: 22px; padding: 0 10px 0 12px; cursor: pointer; user-select: none;
  font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; color: var(--nm-text-dim);
}
.ps-sec:hover { color: var(--nm-text-strong); }
.ps-op { margin-left: auto; text-transform: none; letter-spacing: 0; color: var(--nm-danger); font-weight: 600; }
.ps-last { padding: 0 10px 8px 22px; font-size: 11px; color: var(--nm-text-muted); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
</style>
