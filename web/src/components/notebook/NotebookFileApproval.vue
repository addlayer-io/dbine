<script setup lang="ts">
import { computed } from 'vue';
import type { DataFile, FileApprovalRequest, FileDecision } from '../../api/types';
import { locale } from '../../i18n';

// "Este notebook quiere leer estos archivos" (docs/notebooks.md › Reading
// local files › Approval): every file a run reads that nobody allowed yet,
// in full (canonical path, size, modification time), the files of a glob
// under their pattern. One answer covers them all: Permitir (this run),
// Permitir siempre (this notebook, on this machine; not for an unsaved
// notebook) or Denegar. A file read with its sidecars (a shapefile's .shx,
// .dbf…) shows "+ N archivos asociados", listed in its tooltip; the same
// answer covers them. A URL or a cloud pattern (s3://…/*.parquet) shows its
// host first, in bold, and the URL in full below; nothing is fetched
// before the answer. The backend checks the answer against the files it
// resolved; this dialog only relays it.

const props = defineProps<{ request: FileApprovalRequest }>();
const emit = defineEmits<{ decide: [decision: FileDecision] }>();

/** "940 B", "4,1 KB", "12,3 MB". */
function formatBytes(b: number): string {
  if (b < 1024) return `${b.toLocaleString(locale())} B`;
  const units = ['KB', 'MB', 'GB', 'TB'];
  let v = b / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i++; }
  return `${v.toLocaleString(locale(), { maximumFractionDigits: 1 })} ${units[i]}`;
}
const when = (ms: number) => new Date(ms).toLocaleString(locale(), { day: '2-digit', month: '2-digit', year: '2-digit', hour: '2-digit', minute: '2-digit' });
const detail = (f: DataFile) => `${formatBytes(f.size)} · ${when(f.mtime_ms)}`;
/** A file's sidecars (a shapefile's .shx, .dbf…), one per line with their details. */
const sidecarTip = (f: DataFile) => (f.sidecars ?? []).map((s) => `${s.path} · ${detail(s)}`).join('\n');

/** The local files written out first, then each glob with its files. */
const groups = computed(() => {
  const local = props.request.files.filter((f) => !f.host);
  const plain = local.filter((f) => !f.pattern);
  const globs = new Map<string, DataFile[]>();
  for (const f of local) if (f.pattern) globs.set(f.pattern, [...(globs.get(f.pattern) ?? []), f]);
  return [
    ...(plain.length ? [{ pattern: null as string | null, files: plain }] : []),
    ...[...globs].map(([pattern, files]) => ({ pattern: pattern as string | null, files })),
  ];
});
/** URLs and cloud patterns (docs/notebooks.md › Reading files from the web
 *  and cloud storage): nothing was fetched yet, so the host goes first and
 *  big, then the URL in full; no size or time. */
const remote = computed(() => props.request.files.filter((f) => f.host));
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="remote.length ? $t('notebook:fileAccess.titleRemote') : $t('notebook:fileAccess.title')"
    width="600px"
    append-to-body
    :close-on-click-modal="false"
    :close-on-press-escape="false"
    :show-close="false"
    class="nfa-dialog"
  >
    <p class="nfa-lead">{{ $t('notebook:fileAccess.lead', { count: request.files.length }) }}</p>
    <div class="nfa-list">
      <div v-for="(g, i) in groups" :key="g.pattern ?? `plain-${i}`" class="nfa-group">
        <div v-if="g.pattern" class="nfa-pattern">
          <span class="nfa-tag">{{ $t('notebook:fileAccess.pattern') }}</span>
          <code class="nm-selectable">{{ g.pattern }}</code>
        </div>
        <div v-for="f in g.files" :key="f.path" class="nfa-file" :class="{ nested: !!g.pattern }">
          <code class="nm-selectable">{{ f.path }}</code>
          <el-tooltip v-if="f.sidecars?.length" placement="top" :show-after="200" popper-class="nfa-sidecar-tip">
            <template #content><div class="nfa-sidecar-list">{{ sidecarTip(f) }}</div></template>
            <span class="nfa-tag nfa-sidecars">{{ $t('notebook:fileAccess.sidecars', { count: f.sidecars.length }) }}</span>
          </el-tooltip>
          <span class="nfa-meta">{{ detail(f) }}</span>
        </div>
      </div>
      <div v-for="f in remote" :key="f.path" class="nfa-group nfa-remote">
        <div class="nfa-host">
          <span class="nfa-host-name nm-selectable">{{ f.host }}</span>
          <span v-if="f.pattern" class="nfa-tag">{{ $t('notebook:fileAccess.pattern') }}</span>
          <span class="nfa-tag">{{ $t('notebook:fileAccess.remoteTag') }}</span>
        </div>
        <div class="nfa-file nested">
          <code class="nm-selectable">{{ f.path }}</code>
        </div>
      </div>
    </div>
    <p v-if="remote.length" class="nfa-note">{{ $t('notebook:fileAccess.remoteNote') }}</p>
    <p class="nfa-note">{{ request.saved ? $t('notebook:fileAccess.noteSaved') : $t('notebook:fileAccess.noteUnsaved') }}</p>
    <template #footer>
      <div class="nfa-footer">
        <el-button @click="emit('decide', 'deny')">{{ $t('notebook:fileAccess.deny') }}</el-button>
        <div class="nm-spacer" />
        <el-tooltip :content="request.saved ? $t('notebook:fileAccess.alwaysTip') : $t('notebook:fileAccess.alwaysUnsaved')" placement="top" :show-after="200">
          <!-- A disabled button gets no hover: the span carries the tooltip. -->
          <span class="nfa-always">
            <el-button :disabled="!request.saved" @click="emit('decide', 'always')">{{ $t('notebook:fileAccess.always') }}</el-button>
          </span>
        </el-tooltip>
        <el-button type="primary" @click="emit('decide', 'once')">{{ $t('notebook:fileAccess.once') }}</el-button>
      </div>
    </template>
  </el-dialog>
</template>

<style scoped>
.nfa-lead { margin: 0 0 10px; color: var(--nm-text-strong); font-size: 13px; }
.nfa-list {
  max-height: 320px; overflow: auto; border: 1px solid var(--nm-border); border-radius: 4px;
  background: var(--nm-overlay-row); padding: 4px 0;
}
.nfa-group + .nfa-group { border-top: 1px solid var(--nm-border); margin-top: 4px; padding-top: 4px; }
.nfa-pattern, .nfa-file { display: flex; align-items: baseline; gap: 8px; padding: 4px 10px; min-width: 0; }
.nfa-file.nested { padding-left: 26px; }
.nfa-pattern code, .nfa-file code {
  font-family: var(--nm-mono); font-size: 12px; color: var(--nm-text-strong);
  overflow-wrap: anywhere; flex: 1; min-width: 0;
}
.nfa-file.nested code { color: var(--nm-text); }
.nfa-meta { flex-shrink: 0; color: var(--nm-text-dim); font-size: 11.5px; white-space: nowrap; }
.nfa-tag {
  flex-shrink: 0; font-size: 11px; padding: 0 6px; border-radius: 3px;
  background: var(--nm-overlay-code); color: var(--nm-text-dim);
}
.nfa-note { margin: 10px 0 0; color: var(--nm-text-dim); font-size: 12px; line-height: 1.5; }
.nfa-footer { display: flex; align-items: center; gap: 8px; }
.nfa-footer .el-button + .el-button { margin-left: 0; }
.nfa-always { display: inline-flex; }
.nfa-sidecars { cursor: default; white-space: nowrap; }
.nfa-host { display: flex; align-items: center; gap: 8px; padding: 6px 10px 0; min-width: 0; }
.nfa-host-name { font-size: 14px; font-weight: 600; color: var(--nm-text-strong); overflow-wrap: anywhere; min-width: 0; }
.nfa-sidecar-list { white-space: pre-line; font-family: var(--nm-mono); font-size: 11.5px; }
</style>
