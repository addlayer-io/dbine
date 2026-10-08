<script setup lang="ts">
import { computed, onMounted, onUnmounted, ref } from 'vue';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorKind, errorMessage } from '../api/client';
import { optimizerApi, type Analysis, type Candidate, type IndexHint, type Measure, type OptimizerNote } from '../api/optimizer';
import type { Plan } from '../api/types';
import { language, locale } from '../i18n';
import { tb } from '../i18n/backend';
import { newQuery } from '../composables/actions';
import CodeEditor from '../components/CodeEditor.vue';
import PlanView from '../components/PlanView.vue';
import { useConnectionsStore } from '../stores/connections';
import { editorBridge, useAiStore } from '../stores/ai';
import { useTabsStore, type OptimizerTab } from '../stores/tabs';
import { useUiStore } from '../stores/ui';

// "Optimizar consulta" (docs/optimizar-consulta.md): the rules' rewrites,
// the AI's and the user's own alternatives, all verified the same way by
// "Comparar" (on a read-only session, only when the user clicks it); and
// index suggestions from the plan. Nothing here changes the database:
// scripts and versions only open in the editor.

const props = defineProps<{ tab: OptimizerTab }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const tabs = useTabsStore();
const ui = useUiStore();
const ai = useAiStore();

const driver = computed(() => conns.driverOf(props.tab.connectionId));
const analysis = ref<Analysis | null>(null);
const error = ref<string | null>(null);
const analyzing = ref(false);
/** Rule candidates come with the analysis; the AI's and the user's are added here. */
const extra = ref<Candidate[]>([]);
const candidates = computed(() => [...(analysis.value?.candidates ?? []), ...extra.value]);
const open = ref<Set<string>>(new Set());

const runs = ref(3);
const maxRows = ref(100000);
const measures = ref<Record<string, Measure>>({});
const comparing = ref(false);
const compareTotal = ref(0);
const compared = ref(false);

const aiRunning = ref(false);
const aiInfo = ref<{ kind: 'none' | 'nothing' | 'sent'; text: string } | null>(null);
const aiMissing = ref(false);

const ownOpen = ref(false);
const ownText = ref('');
const planShown = ref<{ name: string; plans: Plan[] } | null>(null);

let analyzeId = '';
let aiId = '';
let compareId = '';
let unlisten: UnlistenFn | null = null;

const ORIGINAL = 'original';

function newId(kind: string) {
  return `${props.tab.id}-${kind}-${Date.now()}`;
}

async function analyze() {
  if (analyzing.value) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  analyzing.value = true;
  error.value = null;
  analyzeId = newId('a');
  try {
    analysis.value = await optimizerApi.analyze(props.tab.connectionId, props.tab.database, props.tab.sql, analyzeId);
    measures.value = {};
    compared.value = false;
  } catch (e) {
    if (errorKind(e) !== 'cancelled') error.value = errorMessage(e);
  } finally {
    analyzing.value = false;
  }
}

onMounted(async () => {
  try {
    unlisten = await listen<{ run_id: string; measure: Measure }>('optimizer-progress', (e) => {
      if (e.payload.run_id === compareId) measures.value = { ...measures.value, [e.payload.measure.id]: e.payload.measure };
    });
  } catch { /* outside Tauri */ }
  analyze();
});
onUnmounted(() => {
  unlisten?.();
  for (const id of [analyzeId, aiId, compareId]) if (id) optimizerApi.cancel(id).catch(() => {});
});

// -- labels ---------------------------------------------------------------------------------

function title(c: Candidate): string {
  if (c.rule) return t(`optimizer:rules.${c.rule}.title`);
  return c.title || t(`optimizer:source.${c.source}`);
}
function explanation(c: Candidate): string {
  if (c.rule) return t(`optimizer:rules.${c.rule}.explain`, c.params);
  return c.explanation ?? '';
}
function noteText(n: OptimizerNote): string {
  return t(`optimizer:notesText.${n.rule}`, n.params);
}
function hintText(h: IndexHint): string {
  return t(`optimizer:hint.${h.reason}`, { table: h.table, columns: h.columns.join(', ') });
}
const fmtMs = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale(), { maximumFractionDigits: v < 10 ? 2 : 1 })} ms`);
const fmtNum = (v: number | null) => (v == null ? '—' : v.toLocaleString(locale(), { maximumFractionDigits: 2 }));

function toggle(id: string) {
  const s = new Set(open.value);
  if (s.has(id)) s.delete(id);
  else s.add(id);
  open.value = s;
}

// -- comparison -------------------------------------------------------------------------------

const original = computed<Measure | undefined>(() => measures.value[ORIGINAL]);

/** How a version did against the original: its result and its speed. */
function verdict(id: string): { kind: 'same' | 'different' | 'unverified' | 'truncated' | 'error' | 'planOnly' | 'pending'; text: string } {
  const m = measures.value[id];
  if (!m) return { kind: 'pending', text: t('optimizer:result.pending') };
  if (m.error) return { kind: 'error', text: `${t('optimizer:result.error')}: ${tb(m.error)}` };
  if (!m.executed) return { kind: 'planOnly', text: t('optimizer:result.planOnly') };
  if (m.truncated) return { kind: 'truncated', text: t('optimizer:result.truncated', { count: maxRows.value.toLocaleString(locale()) }) };
  if (m.equivalent === true) return { kind: 'same', text: t('optimizer:result.same') };
  if (m.equivalent === false) return { kind: 'different', text: t('optimizer:result.different') };
  return { kind: 'unverified', text: t('optimizer:result.unverified') };
}

function speed(id: string): { text: string; better: boolean } | null {
  const m = measures.value[id];
  const o = original.value;
  if (!m || !o || id === ORIGINAL) return null;
  if (m.avg_ms && o.avg_ms) {
    const r = o.avg_ms / m.avg_ms;
    const x = (r >= 1 ? r : 1 / r).toLocaleString(locale(), { maximumFractionDigits: 1 });
    return { text: r >= 1 ? t('optimizer:faster', { x }) : t('optimizer:slower', { x }), better: r >= 1.1 };
  }
  if (!m.executed && m.cost && o.cost) {
    const r = o.cost / m.cost;
    return { text: t('optimizer:cheaper', { x: r.toLocaleString(locale(), { maximumFractionDigits: 1 }) }), better: r >= 1.1 };
  }
  return null;
}

/** The fastest equivalent version, clearly faster than the original. Never one that isn't proven equivalent. */
const recommended = computed<string | null>(() => {
  const o = original.value;
  if (!o?.avg_ms) return null;
  let best: Measure | null = null;
  for (const c of candidates.value) {
    const m = measures.value[c.id];
    if (!m || m.equivalent !== true || !m.avg_ms || m.avg_ms > o.avg_ms * 0.9) continue;
    if (!best || m.avg_ms < (best.avg_ms ?? Infinity)) best = m;
  }
  return best?.id ?? null;
});

const rows = computed(() => [
  { id: ORIGINAL, label: t('optimizer:source.original'), source: 'original' as const },
  ...candidates.value.map((c) => ({ id: c.id, label: title(c), source: c.source })),
]);

async function compare() {
  if (comparing.value || !candidates.value.length) return;
  if (!(await conns.ensureConnected(props.tab.connectionId))) return;
  comparing.value = true;
  error.value = null;
  measures.value = {};
  compareId = newId('c');
  const versions = [{ id: ORIGINAL, sql: props.tab.sql }, ...candidates.value.map((c) => ({ id: c.id, sql: c.sql }))];
  compareTotal.value = versions.length;
  try {
    const list = await optimizerApi.compare(props.tab.connectionId, props.tab.database, compareId, versions, runs.value, maxRows.value);
    measures.value = Object.fromEntries(list.map((m) => [m.id, m]));
    compared.value = true;
  } catch (e) {
    if (errorKind(e) !== 'cancelled') error.value = errorMessage(e);
  } finally {
    comparing.value = false;
  }
}

function cancelCompare() {
  if (compareId) optimizerApi.cancel(compareId).catch(() => {});
}

function showPlan(id: string, name: string) {
  const plans = id === ORIGINAL && !measures.value[ORIGINAL]?.plans.length ? analysis.value?.plans ?? [] : measures.value[id]?.plans ?? [];
  if (plans.length) planShown.value = { name, plans };
}
const hasPlan = (id: string) => !!measures.value[id]?.plans.length || (id === ORIGINAL && !!analysis.value?.plans.length);

// -- AI and the user's own alternatives -------------------------------------------------------

async function askAi() {
  if (aiRunning.value) return;
  aiInfo.value = null;
  await ai.init();
  const p = ai.provider;
  aiMissing.value = !p;
  if (!p) return;
  aiRunning.value = true;
  aiId = newId('ai');
  try {
    const r = await optimizerApi.ai(props.tab.connectionId, props.tab.database, props.tab.sql, aiId, p.kind, ai.model, analysis.value?.plans ?? [], language.value);
    const known = new Set(candidates.value.map((c) => c.sql.trim()));
    extra.value = [...extra.value, ...r.candidates.filter((c) => !known.has(c.sql.trim()))];
    const items = r.sent.map((s) => (s === 'plan' ? t('optimizer:aiPlan') : t('optimizer:aiTables', { count: Number.parseInt(s, 10) || 0 })));
    const what = new Intl.ListFormat(language.value, { type: 'conjunction' }).format(items);
    const provider = tb(p.label);
    const sent = what ? t('optimizer:aiSent', { provider, what }) : t('optimizer:aiSentOnly', { provider });
    const discarded = r.discarded ? ` ${t('optimizer:aiDiscarded', { count: r.discarded })}` : '';
    if (r.none) aiInfo.value = { kind: 'none', text: `${t('optimizer:aiNone')} ${sent}` };
    else if (!r.candidates.length) aiInfo.value = { kind: 'nothing', text: r.discarded ? t('optimizer:aiDiscarded', { count: r.discarded }) : t('optimizer:aiNothing') };
    else aiInfo.value = { kind: 'sent', text: `${sent}${discarded}` };
  } catch (e) {
    if (errorKind(e) !== 'cancelled') ElMessage.error(errorMessage(e));
  } finally {
    aiRunning.value = false;
  }
}

function addOwn() {
  ownText.value = props.tab.sql;
  ownOpen.value = true;
}
function saveOwn() {
  const sql = ownText.value.trim();
  if (!sql || sql === props.tab.sql.trim()) { ownOpen.value = false; return; }
  const n = extra.value.filter((c) => c.source === 'user').length + 1;
  extra.value = [...extra.value, { id: `user-${Date.now()}`, source: 'user', rule: null, params: {}, title: t('optimizer:ownName', { n }), explanation: null, sql, verify: true }];
  ownOpen.value = false;
}
function remove(c: Candidate) {
  extra.value = extra.value.filter((x) => x.id !== c.id);
  const { [c.id]: _, ...rest } = measures.value;
  measures.value = rest;
}

// -- using a version ---------------------------------------------------------------------------

/** The source editor still holds the query where it was. */
function sourceHolds(): boolean {
  const b = props.tab.sourceTabId ? editorBridge(props.tab.sourceTabId) : null;
  return !!b && b.text().slice(props.tab.from, props.tab.to) === props.tab.sql;
}

async function use(c: Candidate, how: 'new' | 'replace') {
  const v = verdict(c.id).kind;
  // A rule's rewrite is proven equivalent already; the rest asks until it's compared.
  const proven = c.source === 'rule' && !c.verify;
  if (v === 'different' || (v !== 'same' && v !== 'planOnly' && !proven)) {
    try {
      await ElMessageBox.confirm(t(v === 'different' ? 'optimizer:confirmUse.different' : 'optimizer:confirmUse.unverified'), t('optimizer:confirmUse.title'), {
        confirmButtonText: t('optimizer:confirmUse.go'),
        cancelButtonText: t('common:cancel'),
        type: 'warning',
      });
    } catch {
      return;
    }
  }
  if (how === 'replace') {
    const b = props.tab.sourceTabId ? editorBridge(props.tab.sourceTabId) : null;
    if (b && sourceHolds()) {
      const text = b.text();
      b.replace(text.slice(0, props.tab.from) + c.sql + text.slice(props.tab.to));
      tabs.activate(props.tab.sourceTabId!);
      ElMessage.success(t('optimizer:replaced'));
      return;
    }
    ElMessage.info({ message: t('optimizer:replaceGone'), duration: 5000 });
  }
  await newQuery(props.tab.connectionId, props.tab.database, c.sql, t('optimizer:queryName', { title: title(c).slice(0, 40) }));
}

function openHint(h: IndexHint) {
  if (h.script) newQuery(props.tab.connectionId, props.tab.database, h.script, t('optimizer:indexQueryName', { table: h.table }));
}
function openNote(n: OptimizerNote) {
  if (n.sql) newQuery(props.tab.connectionId, props.tab.database, n.sql, t('optimizer:queryName', { title: n.params.table ?? '' }));
}

const sourceLabel = (s: string) => t(`optimizer:source.${s}`);
</script>

<template>
  <div class="ov">
    <header class="ov-head">
      <strong>{{ $t('optimizer:title', { name: tab.database || conns.byId(tab.connectionId)?.name || '' }) }}</strong>
      <span v-if="analysis" class="nm-muted">{{ analysis.engine }}</span>
      <div class="ov-spacer" />
      <el-button size="small" :loading="analyzing" @click="analyze">{{ $t('optimizer:reanalyze') }}</el-button>
    </header>

    <div class="ov-body">
      <el-alert v-if="error" type="error" :title="error" :closable="false" show-icon class="ov-alert" />

      <section class="ov-sec">
        <h3 class="nm-section-title">{{ $t('optimizer:original') }}</h3>
        <pre class="ov-sql nm-selectable">{{ tab.sql }}</pre>
        <p v-if="analysis?.writes" class="ov-warn"><el-icon><ei-warning-filled /></el-icon>{{ $t('optimizer:writesNote') }}</p>
        <div v-if="analysis?.skipped.length" class="nm-muted ov-skipped">
          <div>{{ $t('optimizer:skipped') }}</div>
          <div v-for="s in analysis.skipped" :key="s">· {{ tb(s) }}</div>
        </div>
      </section>

      <div v-if="analyzing && !analysis" class="ov-empty"><el-icon class="is-loading" :size="22"><ei-loading /></el-icon><span>{{ $t('optimizer:analyzing') }}</span></div>

      <template v-if="analysis">
        <section class="ov-sec">
          <div class="ov-sec-head">
            <h3 class="nm-section-title">{{ $t('optimizer:alternatives') }}</h3>
            <div class="ov-spacer" />
            <el-button size="small" :loading="aiRunning" @click="askAi">
              <el-icon v-if="!aiRunning"><ei-magic-stick /></el-icon>&nbsp;{{ aiRunning ? $t('optimizer:askingAi') : $t('optimizer:askAi') }}
            </el-button>
            <el-button size="small" @click="addOwn">{{ $t('optimizer:addOwn') }}</el-button>
          </div>

          <el-alert v-if="aiMissing" type="info" :closable="false" show-icon class="ov-alert">
            <template #title>{{ $t('optimizer:aiMissing') }}</template>
            <el-button size="small" class="ov-alert-btn" @click="ui.aiOpen = true">{{ $t('optimizer:openAssistant') }}</el-button>
          </el-alert>
          <p v-if="aiInfo" class="nm-muted ov-ai-info">{{ aiInfo.text }}</p>

          <p v-if="!candidates.length" class="nm-muted ov-none">
            {{ analysis.language === 'sql' || analysis.language === 'json' ? $t('optimizer:noAlternatives') : $t('optimizer:noRulesEngine') }}
          </p>

          <template v-else>
            <div class="ov-compare-bar">
              <el-tooltip :content="$t('optimizer:runsTip')" placement="top" :show-after="300">
                <span class="nm-muted">{{ $t('optimizer:runs') }}</span>
              </el-tooltip>
              <el-input-number v-model="runs" size="small" :min="1" :max="20" controls-position="right" class="ov-num" :disabled="comparing" />
              <el-tooltip :content="$t('optimizer:maxRowsTip')" placement="top" :show-after="300">
                <span class="nm-muted">{{ $t('optimizer:maxRows') }}</span>
              </el-tooltip>
              <el-select v-model="maxRows" size="small" class="ov-rows" :disabled="comparing">
                <el-option v-for="n in [1000, 10000, 100000, 1000000]" :key="n" :label="n.toLocaleString(locale())" :value="n" />
              </el-select>
              <el-tooltip :content="$t('optimizer:compareTip')" placement="top" :show-after="300">
                <el-button v-if="!comparing" type="primary" size="small" @click="compare">{{ $t('optimizer:compare') }}</el-button>
                <el-button v-else type="danger" size="small" @click="cancelCompare">{{ $t('common:cancel') }}</el-button>
              </el-tooltip>
              <span v-if="comparing" class="nm-muted"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('optimizer:comparing', { done: Object.keys(measures).length, total: compareTotal }) }}</span>
            </div>

            <table v-if="comparing || compared || Object.keys(measures).length" class="ov-table">
              <thead>
                <tr>
                  <th>{{ $t('optimizer:table.version') }}</th>
                  <th class="num">{{ $t('optimizer:table.min') }}</th>
                  <th class="num">{{ $t('optimizer:table.avg') }}</th>
                  <th class="num">{{ $t('optimizer:table.rows') }}</th>
                  <th class="num">{{ $t('optimizer:table.cost') }}</th>
                  <th>{{ $t('optimizer:table.result') }}</th>
                  <th>{{ $t('optimizer:table.speed') }}</th>
                  <th />
                </tr>
              </thead>
              <tbody>
                <tr v-for="r in rows" :key="r.id" :class="{ best: r.id === recommended }">
                  <td><span class="ov-badge" :class="r.source">{{ sourceLabel(r.source) }}</span> {{ r.label }}</td>
                  <td class="num">{{ fmtMs(measures[r.id]?.min_ms ?? null) }}</td>
                  <td class="num">{{ fmtMs(measures[r.id]?.avg_ms ?? null) }}</td>
                  <td class="num">{{ fmtNum(measures[r.id]?.rows ?? null) }}</td>
                  <td class="num">{{ fmtNum(measures[r.id]?.cost ?? null) }}</td>
                  <td><span class="ov-verdict" :class="verdict(r.id).kind" :title="verdict(r.id).text">{{ r.id === ORIGINAL && verdict(r.id).kind === 'same' ? '—' : verdict(r.id).text }}</span></td>
                  <td>
                    <span v-if="r.id === recommended" class="ov-rec">{{ $t('optimizer:recommended') }}</span>
                    <span v-else-if="speed(r.id)" :class="{ good: speed(r.id)!.better }">{{ speed(r.id)!.text }}</span>
                  </td>
                  <td><el-button v-if="hasPlan(r.id)" link size="small" @click="showPlan(r.id, r.label)">{{ $t('optimizer:viewPlan') }}</el-button></td>
                </tr>
              </tbody>
            </table>

            <div class="ov-cards">
              <article v-for="c in candidates" :key="c.id" class="ov-card" :class="[verdict(c.id).kind, { best: c.id === recommended }]">
                <div class="ov-card-head">
                  <span class="ov-badge" :class="c.source">{{ sourceLabel(c.source) }}</span>
                  <strong>{{ title(c) }}</strong>
                  <span v-if="c.id === recommended" class="ov-rec">{{ $t('optimizer:recommended') }}</span>
                  <span v-else-if="measures[c.id]" class="ov-verdict" :class="verdict(c.id).kind">{{ verdict(c.id).text }}</span>
                  <div class="ov-spacer" />
                  <el-button v-if="c.source !== 'rule'" link size="small" @click="remove(c)">{{ $t('optimizer:remove') }}</el-button>
                </div>
                <p v-if="explanation(c)" class="ov-explain">{{ explanation(c) }}</p>
                <p v-if="c.verify && measures[c.id]?.equivalent !== true" class="ov-verify"><el-icon><ei-warning /></el-icon>{{ $t('optimizer:verify') }}</p>
                <button class="ov-toggle" @click="toggle(c.id)">
                  <el-icon class="ov-chev" :class="{ open: open.has(c.id) }"><ei-arrow-right /></el-icon>{{ open.has(c.id) ? $t('optimizer:hideSql') : $t('optimizer:showSql') }}
                </button>
                <pre v-if="open.has(c.id)" class="ov-sql nm-selectable">{{ c.sql }}</pre>
                <div class="ov-actions">
                  <el-button v-if="hasPlan(c.id)" size="small" @click="showPlan(c.id, title(c))">{{ $t('optimizer:viewPlan') }}</el-button>
                  <el-dropdown split-button size="small" trigger="click" @click="use(c, 'new')" @command="(how: 'new' | 'replace') => use(c, how)">
                    {{ $t('optimizer:use') }}
                    <template #dropdown>
                      <el-dropdown-menu>
                        <el-dropdown-item command="new">{{ $t('optimizer:useNew') }}</el-dropdown-item>
                        <el-dropdown-item command="replace" :disabled="!sourceHolds()">{{ $t('optimizer:useReplace') }}</el-dropdown-item>
                      </el-dropdown-menu>
                    </template>
                  </el-dropdown>
                </div>
              </article>
            </div>
          </template>
        </section>

        <section class="ov-sec">
          <h3 class="nm-section-title">{{ $t('optimizer:hints') }}</h3>
          <p v-if="!analysis.supports_explain" class="nm-muted ov-none">{{ $t('optimizer:hintsNoPlan') }}</p>
          <p v-else-if="!analysis.hints.length" class="nm-muted ov-none">{{ $t('optimizer:hintsNone') }}</p>
          <template v-else>
            <p class="nm-muted ov-hint-note">{{ $t('optimizer:hintsNote') }}</p>
            <div v-for="(h, i) in analysis.hints" :key="i" class="ov-hint">
              <div class="ov-hint-title">
                <el-icon><ei-collection /></el-icon>
                <span>{{ hintText(h) }}</span>
                <span v-if="h.impact != null" class="nm-muted">· {{ $t('optimizer:hint.impact', { impact: fmtNum(h.impact) }) }}</span>
                <span v-if="h.est_rows != null" class="nm-muted">· {{ $t('optimizer:hint.rows', { rows: fmtNum(Math.round(h.est_rows)) }) }}</span>
              </div>
              <template v-if="h.script">
                <pre class="ov-sql nm-selectable">{{ h.script }}</pre>
                <el-button size="small" @click="openHint(h)">{{ $t('optimizer:openScript') }}</el-button>
              </template>
            </div>
          </template>
        </section>

        <section v-if="analysis.notes.length || analysis.warnings.length" class="ov-sec">
          <h3 class="nm-section-title">{{ $t('optimizer:notes') }}</h3>
          <div v-for="(n, i) in analysis.notes" :key="`n${i}`" class="ov-note">
            <el-icon><ei-info-filled /></el-icon>
            <span>{{ noteText(n) }}</span>
            <el-button v-if="n.sql" link size="small" @click="openNote(n)">{{ $t('optimizer:notesText.select_starOpen') }}</el-button>
          </div>
          <template v-if="analysis.warnings.length">
            <div class="nm-muted ov-sub">{{ $t('optimizer:planWarnings') }}</div>
            <div v-for="(w, i) in analysis.warnings" :key="`w${i}`" class="ov-note">
              <el-icon><ei-warning /></el-icon>
              <span><code>{{ w.op }}{{ w.object ? ` · ${w.object}` : '' }}</code> {{ tb(w.text) }}</span>
            </div>
          </template>
        </section>
      </template>
    </div>

    <el-dialog :model-value="!!planShown" :title="$t('optimizer:planTitle', { name: planShown?.name ?? '' })" width="85%" top="5vh" append-to-body @close="planShown = null">
      <div class="ov-plan"><PlanView v-if="planShown" :plans="planShown.plans" /></div>
    </el-dialog>

    <el-dialog v-model="ownOpen" :title="$t('optimizer:ownTitle')" width="760px" append-to-body>
      <p class="nm-muted ov-own-hint">{{ $t('optimizer:ownHint') }}</p>
      <div class="ov-own"><CodeEditor v-model="ownText" :language="driver?.language" :dialect="driver?.dialect" /></div>
      <template #footer>
        <el-button @click="ownOpen = false">{{ $t('common:cancel') }}</el-button>
        <el-button type="primary" @click="saveOwn">{{ $t('optimizer:add') }}</el-button>
      </template>
    </el-dialog>
  </div>
</template>

<style scoped>
.ov { display: flex; flex-direction: column; height: 100%; min-height: 0; background: var(--nm-bg); }
.ov-head { display: flex; align-items: center; gap: 10px; padding: 10px 16px; border-bottom: 1px solid var(--nm-border-soft); }
.ov-head strong { color: var(--nm-text-strong); }
.ov-spacer { flex: 1; }
.ov-body { flex: 1; overflow: auto; padding: 8px 16px 24px; }
.ov-alert { margin: 8px 0; width: auto; }
.ov-alert-btn { margin-top: 6px; }
.ov-sec { margin: 10px 0 18px; }
.ov-sec h3 { margin: 6px 0; }
.ov-sec-head { display: flex; align-items: center; gap: 8px; }
.ov-sec-head .el-button { margin: 0; }
.ov-sql {
  margin: 4px 0 8px; padding: 8px 10px; max-height: 220px; overflow: auto; white-space: pre-wrap;
  font-family: var(--nm-mono); font-size: 12px; background: var(--nm-bg-elev); border: 1px solid var(--nm-border-soft); border-radius: 3px;
}
.ov-warn { display: flex; align-items: center; gap: 6px; margin: 4px 0; font-size: 12.5px; color: var(--nm-warning); }
.ov-skipped { font-size: 11.5px; margin-top: 4px; }
.ov-empty { display: flex; align-items: center; gap: 8px; justify-content: center; padding: 40px; color: var(--nm-text-dim); }
.ov-none, .ov-ai-info, .ov-hint-note { font-size: 12.5px; margin: 6px 0; }
.ov-compare-bar { display: flex; align-items: center; gap: 8px; margin: 8px 0; font-size: 12px; flex-wrap: wrap; }
.ov-compare-bar .el-button { margin: 0; }
.ov-num { width: 84px; }
.ov-rows { width: 120px; }
.ov-table { width: 100%; border-collapse: collapse; font-size: 12px; margin: 6px 0 12px; }
.ov-table th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 4px 8px; border-bottom: 1px solid var(--nm-border-soft); }
.ov-table td { padding: 5px 8px; border-bottom: 1px solid var(--nm-border-soft); color: var(--nm-text); }
.ov-table .num { text-align: right; font-variant-numeric: tabular-nums; white-space: nowrap; }
.ov-table tr.best td { background: color-mix(in srgb, var(--nm-success) 10%, transparent); }
.ov-badge { display: inline-block; font-size: 10.5px; padding: 0 6px; border-radius: 8px; background: var(--ide-hover); color: var(--nm-text-dim); margin-right: 4px; }
.ov-badge.ai { background: color-mix(in srgb, var(--nm-accent) 18%, transparent); color: var(--nm-text-strong); }
.ov-badge.user { background: color-mix(in srgb, var(--nm-info) 18%, transparent); color: var(--nm-text-strong); }
.ov-verdict { font-size: 11.5px; }
.ov-verdict.same { color: var(--nm-success); }
.ov-verdict.different { color: var(--nm-danger); font-weight: 600; }
.ov-verdict.error { color: var(--nm-danger); }
.ov-verdict.unverified, .ov-verdict.truncated, .ov-verdict.pending, .ov-verdict.planOnly { color: var(--nm-text-dim); }
.ov-rec { font-size: 11px; padding: 0 7px; border-radius: 8px; background: var(--nm-success); color: #fff; }
.good { color: var(--nm-success); }
.ov-cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(380px, 1fr)); gap: 10px; }
.ov-card { border: 1px solid var(--nm-border-soft); border-left-width: 3px; border-radius: 4px; padding: 8px 10px; }
.ov-card.same { border-left-color: var(--nm-success); }
.ov-card.different { border-left-color: var(--nm-danger); }
.ov-card.best { border-color: var(--nm-success); }
.ov-card-head { display: flex; align-items: center; gap: 6px; font-size: 12.5px; }
.ov-card-head strong { color: var(--nm-text-strong); }
.ov-explain { margin: 6px 0; font-size: 12.5px; color: var(--nm-text-dim); line-height: 1.45; }
.ov-verify { display: flex; align-items: center; gap: 5px; margin: 4px 0; font-size: 11.5px; color: var(--nm-warning); }
.ov-toggle { display: flex; align-items: center; gap: 4px; border: 0; padding: 2px 0; background: none; font: inherit; font-size: 12px; color: var(--nm-text-dim); cursor: pointer; }
.ov-chev { transition: transform 0.15s; }
.ov-chev.open { transform: rotate(90deg); }
.ov-actions { display: flex; align-items: center; gap: 8px; margin-top: 6px; }
.ov-actions .el-button { margin: 0; }
.ov-hint { border: 1px solid var(--nm-border-soft); border-radius: 4px; padding: 8px 10px; margin-bottom: 8px; }
.ov-hint-title { display: flex; align-items: center; gap: 6px; font-size: 12.5px; flex-wrap: wrap; }
.ov-note { display: flex; align-items: flex-start; gap: 6px; font-size: 12.5px; margin: 6px 0; line-height: 1.45; }
.ov-note .el-icon { margin-top: 3px; flex-shrink: 0; color: var(--nm-text-dim); }
.ov-note code { font-family: var(--nm-mono); font-size: 11.5px; }
.ov-sub { font-size: 11.5px; margin-top: 10px; }
.ov-plan { height: 70vh; min-height: 0; display: flex; flex-direction: column; }
.ov-own { height: 300px; border: 1px solid var(--nm-border-soft); }
.ov-own-hint { margin: 0 0 8px; font-size: 12.5px; }
</style>
