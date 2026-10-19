<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { SecretBrowseEntry, SecretBrowseListing } from '../api/types';
import { askApproveSecretSource } from '../composables/secretSourceApproval';
import { askMasterPassword, unlockOf } from '../composables/secretSourceUnlock';
import { BROWSE, wholeValueAllowed, type DraftKind } from '../composables/secretSources';

// "Examinar" (docs/secret-sources.md): the source's groups and items, then
// the chosen item's field names. Names only: the backend never sends a
// value. A master password asked here is for these calls only (kept in this
// dialog while it's open, never saved).

const props = defineProps<{
  kind: DraftKind;
  /** The form's definition (its item input may be empty). */
  source: Record<string, unknown>;
  /** Tokens typed in the form, by slot. */
  tokens: Record<string, string>;
  field: string;
  connectionId: string | null;
  /** The connection a new form was copied from ("Duplicar conexión"). */
  duplicateOf?: string | null;
  driver: string;
  /** The entry gives the user and the password: its field step assigns
   *  each (starting from these). */
  roles?: { user: string; password: string };
}>();
const emit = defineEmits<{ pick: [entry: SecretBrowseEntry, field: string | null, userField?: string, fields?: string[]]; close: [] }>();
const { t } = useTranslation();

const open = ref(true);
const loading = ref(false);
const error = ref('');
const listing = ref<SecretBrowseListing | null>(null);
/** Lazy providers: the groups walked so far (`id` is what's listed). */
const trail = ref<{ id: string; name: string }[]>([]);
/** Complete listings: the group shown, `/`-separated. */
const cwd = ref('');
const search = ref('');
const chosen = ref<SecretBrowseEntry | null>(null);
/** With `roles`: the fields chosen for the user and the password. */
const userPick = ref(props.roles?.user ?? '');
const passPick = ref(props.roles?.password ?? '');
const fields = ref<string[] | null>(null);
/** A master password asked for this browse. */
const asked = ref('');

const hasFields = computed(() => !!BROWSE[props.kind]?.field || props.kind === 'onepassword');
const lazyPath = computed(() => trail.value.at(-1)?.id ?? '');

async function call(at: { path?: string; item?: string; query?: string }): Promise<SecretBrowseListing | null> {
  loading.value = true;
  error.value = '';
  try {
    const tokens = { ...props.tokens };
    if (asked.value) tokens[''] = asked.value;
    return await api.browseSecretSource(props.connectionId, props.driver, props.field, props.source, tokens, at, props.duplicateOf ?? null);
  } catch (e) {
    loading.value = false;
    if (await askApproveSecretSource(e)) return call(at);
    const unlock = unlockOf(e);
    if (unlock) {
      const typed = await askMasterPassword(unlock);
      if (typed !== null) {
        asked.value = typed;
        return call(at);
      }
    }
    error.value = errorMessage(e);
    return null;
  } finally {
    loading.value = false;
  }
}

async function load() {
  const l = await call({ path: lazyPath.value, query: search.value.trim() });
  if (l) listing.value = l;
}
onMounted(load);

// Providers that search (Delinea): ask again as the text changes.
let timer: ReturnType<typeof setTimeout> | undefined;
watch(search, () => {
  if (!listing.value?.search) return;
  clearTimeout(timer);
  timer = setTimeout(load, 350);
});

const entries = computed(() => listing.value?.entries ?? []);
/** What the list shows: one level (with its subgroups), or every match of the search. */
const rows = computed<SecretBrowseEntry[]>(() => {
  const l = listing.value;
  if (!l) return [];
  const q = search.value.trim().toLowerCase();
  if (!l.complete) return l.search ? entries.value : entries.value.filter((e) => !q || e.name.toLowerCase().includes(q) || (e.detail ?? '').toLowerCase().includes(q));
  if (q && !l.search) {
    return entries.value.filter((e) => e.kind === 'item' && [e.name, e.path ?? '', e.detail ?? '', e.id].some((s) => s.toLowerCase().includes(q))).slice(0, 500);
  }
  if (l.search) return entries.value;
  const here = cwd.value;
  const groups = new Set<string>();
  const items: SecretBrowseEntry[] = [];
  for (const e of entries.value) {
    const p = e.path ?? '';
    if (p === here) items.push(e);
    else if (here === '' || p.startsWith(`${here}/`)) groups.add(p.slice(here ? here.length + 1 : 0).split('/')[0]);
  }
  const sub: SecretBrowseEntry[] = [...groups].sort((a, b) => a.localeCompare(b)).map((g) => ({ id: here ? `${here}/${g}` : g, name: g, kind: 'group' }));
  return [...sub, ...items];
});
/** Show where each item is (search results, flat lists). */
const showPath = computed(() => !!listing.value?.complete && (!!search.value.trim() || !!listing.value.search));
const crumbs = computed(() => {
  if (listing.value && !listing.value.complete) return trail.value.map((g, i) => ({ name: g.name, i }));
  return cwd.value ? cwd.value.split('/').map((name, i) => ({ name, i })) : [];
});

function goTo(i: number) {
  if (listing.value && !listing.value.complete) {
    trail.value = trail.value.slice(0, i + 1);
    load();
  } else {
    cwd.value = i < 0 ? '' : cwd.value.split('/').slice(0, i + 1).join('/');
  }
}
function goTop() {
  if (listing.value && !listing.value.complete) {
    trail.value = [];
    load();
  } else cwd.value = '';
}

async function choose(e: SecretBrowseEntry) {
  if (e.kind === 'group') {
    search.value = '';
    if (listing.value?.complete) cwd.value = e.id;
    else {
      trail.value = [...trail.value, { id: e.id, name: e.name }];
      await load();
    }
    return;
  }
  if (!hasFields.value) {
    pick(e, null);
    return;
  }
  chosen.value = e;
  fields.value = null;
  // 1Password: the item's vault is the group being listed.
  const l = await call({ item: e.id, path: lazyPath.value });
  if (l) fields.value = l.fields ?? [];
  else chosen.value = null;
}

function pick(e: SecretBrowseEntry, field: string | null) {
  emit('pick', e, field, props.roles ? userPick.value : undefined, fields.value ?? undefined);
  open.value = false;
}

function back() {
  chosen.value = null;
  fields.value = null;
  error.value = '';
}
</script>

<template>
  <el-dialog v-model="open" :title="$t('connection:source.browse.title', { source: $t(`connection:source.kind.${kind}`) })" width="560px" append-to-body class="sb" @closed="emit('close')">
    <template v-if="!chosen">
      <div class="sb-bar">
        <el-input v-model="search" size="small" clearable :placeholder="listing?.search ? $t('connection:source.browse.searchServer') : $t('connection:source.browse.search')">
          <template #prefix><el-icon><ei-search /></el-icon></template>
        </el-input>
      </div>
      <div class="sb-crumbs">
        <button type="button" class="sb-crumb" @click="goTop"><el-icon><ei-house /></el-icon></button>
        <template v-for="c in crumbs" :key="c.i">
          <span class="sb-sep">/</span>
          <button type="button" class="sb-crumb" @click="goTo(c.i)">{{ c.name }}</button>
        </template>
      </div>
    </template>
    <div v-else class="sb-crumbs">
      <button type="button" class="sb-crumb" @click="back"><el-icon><ei-back /></el-icon></button>
      <span class="sb-chosen">{{ chosen.name }}</span>
      <span v-if="chosen.detail" class="sb-detail">{{ chosen.detail }}</span>
    </div>
    <div v-loading="loading" class="sb-list" role="listbox">
      <div v-if="error" class="sb-error" role="alert">{{ error }}</div>
      <template v-else-if="!chosen">
        <button v-for="e in rows" :key="`${e.kind}:${e.id}:${e.path}`" type="button" class="sb-row" :class="e.kind" role="option" @click="choose(e)">
          <el-icon class="sb-icon"><ei-folder v-if="e.kind === 'group'" /><ei-key v-else /></el-icon>
          <span class="sb-name">{{ e.name }}</span>
          <span v-if="showPath && e.path" class="sb-path">{{ e.path }}</span>
          <span v-if="e.kind === 'item' && e.detail" class="sb-detail" :title="e.detail">{{ e.detail }}</span>
          <el-icon v-if="e.kind === 'group'" class="sb-go"><ei-arrow-right /></el-icon>
        </button>
        <div v-if="listing && !rows.length && !loading" class="sb-empty">{{ $t('connection:source.browse.empty') }}</div>
        <div v-if="listing?.truncated" class="sb-note">{{ $t('connection:source.browse.truncated') }}</div>
      </template>
      <template v-else-if="fields">
        <div class="sb-hint">{{ roles ? $t('connection:source.browse.pickRoles') : $t('connection:source.browse.pickField') }}</div>
        <button v-if="wholeValueAllowed(kind)" type="button" class="sb-row" role="option" @click="pick(chosen, '')">
          <el-icon class="sb-icon"><ei-document /></el-icon>
          <span class="sb-name">{{ $t('connection:source.browse.wholeValue') }}</span>
        </button>
        <template v-if="roles">
          <div v-for="f in fields" :key="f" class="sb-row sb-role-row">
            <el-icon class="sb-icon"><ei-tickets /></el-icon>
            <span class="sb-name sb-mono">{{ f }}</span>
            <span class="sb-roles">
              <button type="button" class="sb-role" :class="{ on: userPick === f }" :aria-pressed="userPick === f" @click="userPick = f">{{ $t('connection:source.advanced.user') }}</button>
              <button type="button" class="sb-role" :class="{ on: passPick === f }" :aria-pressed="passPick === f" @click="passPick = f">{{ $t('connection:source.advanced.password') }}</button>
            </span>
          </div>
        </template>
        <template v-else>
          <button v-for="f in fields" :key="f" type="button" class="sb-row" role="option" @click="pick(chosen, f)">
            <el-icon class="sb-icon"><ei-tickets /></el-icon>
            <span class="sb-name sb-mono">{{ f }}</span>
          </button>
        </template>
        <div v-if="!fields.length && !wholeValueAllowed(kind)" class="sb-empty">{{ $t('connection:source.browse.noFields') }}</div>
      </template>
    </div>
    <template #footer>
      <span class="sb-foot">{{ $t('connection:source.browse.namesOnly') }}</span>
      <el-button v-if="chosen && fields && roles" type="primary" @click="pick(chosen, passPick)">{{ $t('connection:source.browse.useItem') }}</el-button>
      <el-button v-else-if="chosen && fields && !fields.length && !wholeValueAllowed(kind)" type="primary" @click="pick(chosen, null)">{{ $t('connection:source.browse.useItem') }}</el-button>
      <el-button @click="open = false">{{ t('common:cancel') }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.sb-bar { margin-bottom: 8px; }
.sb-crumbs { display: flex; align-items: center; gap: 2px; min-height: 24px; margin-bottom: 6px; font-size: 12px; color: var(--nm-text-dim); flex-wrap: wrap; }
.sb-crumb { border: 0; background: none; color: var(--nm-text); font: inherit; padding: 2px 4px; border-radius: 3px; cursor: pointer; display: inline-flex; align-items: center; }
.sb-crumb:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.sb-sep { color: var(--nm-text-muted); }
.sb-chosen { color: var(--nm-text-strong); font-weight: 600; margin-left: 4px; }
.sb-list { height: 320px; overflow-y: auto; border: 1px solid var(--nm-border-soft); border-radius: 4px; background: var(--nm-bg-elev); padding: 2px 0; }
.sb-row {
  display: flex; align-items: center; gap: 8px; width: 100%; padding: 5px 10px; border: 0; background: none; cursor: pointer;
  color: var(--nm-text); font: inherit; font-size: 12.5px; text-align: left; min-width: 0;
}
.sb-row:hover, .sb-row:focus-visible { background: var(--ide-hover); outline: none; }
.sb-icon { flex: none; color: var(--nm-text-dim); }
.sb-row.group .sb-icon { color: var(--el-color-warning); }
.sb-name { flex: none; max-width: 55%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-strong); }
.sb-mono { font-family: var(--nm-font-mono, ui-monospace, monospace); font-size: 12px; }
.sb-path { flex: none; max-width: 30%; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; color: var(--nm-text-dim); font-size: 11.5px; }
.sb-detail { flex: 1; min-width: 0; overflow: hidden; text-overflow: ellipsis; white-space: nowrap; text-align: right; color: var(--nm-text-muted); font-size: 11px; font-family: var(--nm-font-mono, ui-monospace, monospace); }
.sb-go { margin-left: auto; color: var(--nm-text-muted); }
.sb-empty, .sb-note, .sb-hint { padding: 10px 12px; font-size: 12px; color: var(--nm-text-dim); }
.sb-hint { padding: 6px 10px 4px; }
.sb-error { padding: 10px 12px; font-size: 12px; color: var(--nm-danger); white-space: pre-wrap; word-break: break-word; }
.sb-role-row { cursor: default; }
.sb-roles { margin-left: auto; display: inline-flex; gap: 4px; flex: none; }
.sb-role {
  border: 1px solid var(--nm-border); background: none; color: var(--nm-text-dim); font: inherit; font-size: 11px;
  padding: 1px 8px; border-radius: 10px; cursor: pointer;
}
.sb-role:hover { color: var(--nm-text-strong); border-color: var(--ide-focus); }
.sb-role.on { background: var(--el-color-primary); border-color: var(--el-color-primary); color: #fff; }
.sb-foot { float: left; font-size: 11.5px; color: var(--nm-text-muted); line-height: 32px; }
</style>
