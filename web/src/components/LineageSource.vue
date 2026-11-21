<script setup lang="ts">
// The detail panel's part for a card of another system (a lineage's grey card): a source outside
// the database (which saved connection is it? "Ver su linaje en…", a picker remembered for next
// time, or "Crear conexión desde la fuente…") or a Power BI report / dashboard that reads the
// object (its link). The diagram (or the database view) does what "follow" asks.
import { computed, ref, watch } from 'vue';
import { ElMessage } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import { api, errorMessage } from '../api/client';
import type { ConnectionConfig, ExternalRef, SourceCandidate, SourceMatch } from '../api/types';
import { openExternalLink } from '../composables/externalLink';
import { useConnectionsStore } from '../stores/connections';
import { useTabsStore } from '../stores/tabs';
import EngineIcon from './EngineIcon.vue';

const props = defineProps<{
  /** The connection whose lineage shows the card. */
  connectionId: string;
  source: ExternalRef;
  /** The parent is following a link right now. */
  busy?: boolean;
}>();
const emit = defineEmits<{ follow: [candidate: SourceCandidate] }>();
const { t } = useTranslation();
const conns = useConnectionsStore();
const tabs = useTabsStore();

/** A report or dashboard that reads the object: it has a link, not a connection. */
const consumer = computed(() => /^Power BI (report|dashboard)$/i.test(props.source.system ?? ''));
const link = computed(() => (/^https?:\/\//i.test(props.source.detail ?? '') ? props.source.detail! : null));

const match = ref<SourceMatch | null>(null);
const loading = ref(false);
const error = ref('');
const chosen = ref('');
let seq = 0;

async function resolve() {
  if (consumer.value) return;
  const mine = ++seq;
  loading.value = true;
  error.value = '';
  try {
    const m = await api.lineageResolveSource(props.connectionId, props.source);
    if (mine !== seq) return;
    match.value = m;
    chosen.value = m.candidates[0]?.connection_id ?? '';
  } catch (e) {
    if (mine !== seq) return;
    match.value = null;
    error.value = errorMessage(e);
  } finally {
    if (mine === seq) loading.value = false;
  }
}
watch(() => [props.connectionId, props.source], () => { match.value = null; void resolve(); }, { immediate: true });
// A connection created from the source: look again, it may be the one now.
watch(() => conns.list.length, () => void resolve());

const one = computed(() => (match.value?.status === 'one' ? match.value.candidates[0] : null));
const via = (c: SourceCandidate) => t(`dependencies:cross.via.${c.via}`);

async function pick() {
  const m = match.value;
  const c = m?.candidates.find((x) => x.connection_id === chosen.value);
  if (!m || !c) return;
  try {
    // Several matched: the choice is remembered, so it isn't asked again.
    await api.lineageLinkSource(m.signature, c.via === 'enterprise' ? null : c.connection_id);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
  emit('follow', c);
}
async function unlink() {
  if (!match.value) return;
  try {
    await api.lineageLinkSource(match.value.signature, null);
  } catch (e) {
    ElMessage.error(errorMessage(e));
  }
  await resolve();
}
function create(prefill: ConnectionConfig) {
  tabs.openConnectionForm({ prefill });
}
</script>

<template>
  <div class="ls">
    <template v-if="consumer">
      <div class="ls-dim">{{ $t('dependencies:cross.reader') }}</div>
      <el-button v-if="link" size="small" @click="openExternalLink(link)">{{ $t('dependencies:cross.openLink') }}</el-button>
      <div v-if="source.detail && !link" class="ls-dim">{{ source.detail }}</div>
    </template>
    <template v-else>
      <div v-if="loading" class="ls-dim"><el-icon class="is-loading"><ei-loading /></el-icon> {{ $t('dependencies:cross.resolving') }}</div>
      <div v-else-if="error" class="ls-dim">{{ $t('dependencies:lineage.error', { error }) }}</div>
      <template v-else-if="match">
        <template v-if="one">
          <div class="ls-conn"><EngineIcon :id="one.driver" :name="one.driver" :size="14" /> <strong>{{ one.name }}</strong> <span class="ls-dim">· {{ via(one) }}</span></div>
          <div class="ls-actions">
            <el-button size="small" type="primary" :loading="busy" @click="emit('follow', one)">{{ $t('dependencies:cross.follow', { name: one.name }) }}</el-button>
            <el-button v-if="one.via === 'link'" size="small" @click="unlink">{{ $t('dependencies:cross.unlink') }}</el-button>
          </div>
        </template>
        <template v-else-if="match.status === 'many'">
          <div class="ls-dim">{{ $t('dependencies:cross.many') }}</div>
          <el-select v-model="chosen" size="small" class="ls-pick" :aria-label="$t('dependencies:cross.pick')">
            <el-option v-for="c in match.candidates" :key="c.connection_id" :value="c.connection_id" :label="`${c.name} · ${via(c)}`">
              <span class="ls-opt"><EngineIcon :id="c.driver" :name="c.driver" :size="14" /> {{ c.name }}</span>
            </el-option>
          </el-select>
          <div class="ls-actions">
            <el-button size="small" type="primary" :loading="busy" :disabled="!chosen" @click="pick">{{ $t('dependencies:cross.followPicked') }}</el-button>
          </div>
          <div class="ls-dim">{{ $t('dependencies:cross.remembered') }}</div>
        </template>
        <template v-else>
          <div class="ls-dim">{{ $t('dependencies:cross.none') }}</div>
          <div v-if="match.prefill" class="ls-actions">
            <el-button size="small" @click="create(match.prefill)">{{ $t('dependencies:cross.create') }}</el-button>
          </div>
        </template>
      </template>
    </template>
  </div>
</template>

<style scoped>
.ls { display: flex; flex-direction: column; gap: 6px; margin: 8px 0; font-size: 12px; }
.ls-dim { color: var(--nm-text-dim); }
.ls-conn { display: flex; align-items: center; gap: 6px; color: var(--nm-text-strong); }
.ls-actions { display: flex; gap: 6px; flex-wrap: wrap; }
.ls-actions :deep(.el-button) { margin: 0; }
.ls-pick { width: 100%; }
.ls-opt { display: inline-flex; align-items: center; gap: 6px; }
</style>
