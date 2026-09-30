<script setup lang="ts">
import { computed } from 'vue';
import { ElMessage, ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { errorMessage } from '../api/client';
import { locksApi, type BlockedSession } from '../api/locks';
import { locale } from '../i18n';
import { tb } from '../i18n/backend';

// The Monitor's locks panel (docs/bloqueos.md): the blocking chains as a tree
// (each head, then who waits for it), and ending a session.

const props = defineProps<{ connectionId: string; sessions: BlockedSession[]; canKill: boolean; error: string | null; killDenied?: string }>();
const emit = defineEmits<{ killed: [] }>();
const { t } = useTranslation();

interface Row { s: BlockedSession; depth: number; waiting: number }

/** Heads first (nobody they wait for is in the list), each with its chain below. */
const rows = computed<Row[]>(() => {
  const byId = new Map(props.sessions.map((s) => [s.id, s]));
  const children = new Map<string, BlockedSession[]>();
  for (const s of props.sessions) {
    if (s.blocked_by && byId.has(s.blocked_by)) (children.get(s.blocked_by) ?? children.set(s.blocked_by, []).get(s.blocked_by)!).push(s);
  }
  const count = (id: string, seen = new Set<string>()): number => {
    if (seen.has(id)) return 0;
    seen.add(id);
    return (children.get(id) ?? []).reduce((n, c) => n + 1 + count(c.id, seen), 0);
  };
  const out: Row[] = [];
  const seen = new Set<string>();
  const walk = (s: BlockedSession, depth: number) => {
    if (seen.has(s.id)) return;
    seen.add(s.id);
    out.push({ s, depth, waiting: count(s.id) });
    for (const c of children.get(s.id) ?? []) walk(c, depth + 1);
  };
  const heads = props.sessions.filter((s) => !s.blocked_by || !byId.has(s.blocked_by));
  heads.sort((a, b) => count(b.id) - count(a.id));
  for (const h of heads) walk(h, 0);
  // A cycle (a deadlock the engine hasn't resolved yet) has no head.
  for (const s of props.sessions) walk(s, 0);
  return out;
});

const blockedCount = computed(() => props.sessions.filter((s) => s.blocked_by).length);

function waited(ms: number | null): string {
  if (ms === null) return '';
  return ms < 1000 ? `${ms} ms` : `${(ms / 1000).toLocaleString(locale(), { maximumFractionDigits: 1 })} s`;
}

async function kill(s: BlockedSession) {
  try {
    await ElMessageBox.confirm(
      t('locks:killConfirm', { id: s.id, user: s.user ?? '?' }) + (s.sql ? `\n\n${s.sql.slice(0, 400)}` : ''),
      t('locks:killTitle'),
      { confirmButtonText: t('locks:kill'), cancelButtonText: t('common:cancel'), type: 'warning', customStyle: { whiteSpace: 'pre-wrap' } },
    );
  } catch {
    return;
  }
  try {
    await locksApi.kill(props.connectionId, s.id);
    ElMessage.success(t('locks:killed', { id: s.id }));
    emit('killed');
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
}
</script>

<template>
  <section class="bp">
    <header class="bp-head">
      <strong>{{ $t('locks:title') }}</strong>
      <span v-if="error" class="bp-err" :title="error">{{ error }}</span>
      <span v-else-if="!sessions.length" class="bp-ok"><el-icon><ei-circle-check-filled /></el-icon>{{ $t('locks:none') }}</span>
      <span v-else class="bp-warn"><el-icon><ei-warning-filled /></el-icon>{{ $t('locks:blocked', { count: blockedCount }) }}</span>
    </header>
    <div v-if="sessions.length" class="bp-wrap">
      <table class="bp-table">
        <thead>
          <tr>
            <th>{{ $t('locks:session') }}</th><th>{{ $t('locks:user') }}</th><th>{{ $t('locks:database') }}</th>
            <th>{{ $t('locks:wait') }}</th><th>{{ $t('locks:time') }}</th><th>{{ $t('locks:object') }}</th><th>{{ $t('locks:sql') }}</th><th />
          </tr>
        </thead>
        <tbody>
          <tr v-for="r in rows" :key="r.s.id" :class="{ head: r.depth === 0 && r.waiting }">
            <td class="bp-id" :style="{ paddingLeft: `${8 + r.depth * 16}px` }">
              <span v-if="r.depth" class="bp-arrow">↳</span>{{ r.s.id }}
              <span v-if="r.depth === 0 && r.waiting" class="bp-badge" :title="$t('locks:blocksTitle')">{{ $t('locks:blocks', { count: r.waiting }) }}</span>
            </td>
            <td>{{ [r.s.user, r.s.client].filter(Boolean).join(' · ') }}</td>
            <td>{{ r.s.database }}</td>
            <td>{{ r.s.wait ? tb(r.s.wait) : '' }}</td>
            <td class="bp-num">{{ waited(r.s.waited_ms) }}</td>
            <td>{{ r.s.object }}</td>
            <td class="bp-sql" :title="r.s.sql ?? ''">{{ r.s.sql }}</td>
            <td class="bp-act">
              <span v-if="canKill" :title="killDenied">
                <el-button size="small" text type="danger" :disabled="!!killDenied" @click="kill(r.s)">{{ $t('locks:kill') }}</el-button>
              </span>
            </td>
          </tr>
        </tbody>
      </table>
    </div>
  </section>
</template>

<style scoped>
.bp { border: 1px solid var(--nm-border); border-radius: 4px; margin-bottom: 14px; }
.bp-head { display: flex; align-items: center; gap: 10px; padding: 8px 10px; font-size: 12.5px; }
.bp-head strong { color: var(--nm-text-strong); }
.bp-ok, .bp-warn { display: inline-flex; align-items: center; gap: 5px; }
.bp-ok { color: var(--nm-success); }
.bp-warn { color: var(--nm-warning); }
.bp-err { color: var(--nm-danger); overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.bp-wrap { max-height: 320px; overflow: auto; border-top: 1px solid var(--nm-border); }
.bp-table { width: 100%; border-collapse: collapse; font-size: 12px; }
.bp-table th { position: sticky; top: 0; background: var(--nm-bg-elev); text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 5px 8px; white-space: nowrap; }
.bp-table td { padding: 4px 8px; border-top: 1px solid var(--nm-border-soft); color: var(--nm-text); white-space: nowrap; }
.bp-table tr.head td { background: color-mix(in srgb, var(--nm-warning) 8%, transparent); }
.bp-id { font-variant-numeric: tabular-nums; }
.bp-arrow { color: var(--nm-text-dim); margin-right: 4px; }
.bp-badge { margin-left: 6px; padding: 0 6px; border-radius: 8px; font-size: 10.5px; background: color-mix(in srgb, var(--nm-warning) 25%, transparent); color: var(--nm-text-strong); }
.bp-num { text-align: right; font-variant-numeric: tabular-nums; }
.bp-sql { max-width: 420px; overflow: hidden; text-overflow: ellipsis; font-family: var(--nm-mono); }
.bp-act { text-align: right; }
</style>
