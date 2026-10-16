<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { tb } from '../backend';
import { api, errorText, type HealthView } from '../api';

// A database's health check (the desktop's "Chequeo de salud"): problems
// first, then what was checked and is fine. It reads a lot, so it runs when
// asked, not on every refresh.

const props = defineProps<{ connectionId: string }>();
const dbs = ref<string[]>([]);
const db = ref('');
const view = ref<HealthView | null>(null);
const busy = ref(false);
const error = ref('');
const showOk = ref(false);

const ORDER = { critical: 0, warning: 1, info: 2, ok: 3 } as const;
const problems = computed(() => (view.value?.checks ?? []).filter((c) => c.severity !== 'ok').sort((a, b) => ORDER[a.severity] - ORDER[b.severity]));
const fine = computed(() => (view.value?.checks ?? []).filter((c) => c.severity === 'ok'));

async function run() {
  busy.value = true;
  error.value = '';
  try {
    view.value = await api.health(props.connectionId, db.value);
  } catch (e) {
    error.value = errorText(e);
  } finally {
    busy.value = false;
  }
}

onMounted(async () => {
  try {
    dbs.value = await api.databases(props.connectionId);
    db.value = dbs.value[0] ?? '';
  } catch {
    /* engines without databases: the connection itself */
  }
});
</script>

<template>
  <div class="hp">
    <select v-if="dbs.length > 1" v-model="db" class="db">
      <option v-for="d in dbs" :key="d" :value="d">{{ d }}</option>
    </select>
    <button class="primary" :disabled="busy" @click="run">{{ busy ? $t('health.running') : $t('health.run') }}</button>
    <p v-if="error" class="error">{{ error }}</p>
    <template v-if="view">
      <p v-if="!problems.length" class="okmsg">{{ $t('health.allGood') }}</p>
      <div v-for="(c, i) in problems" :key="i" class="card chk" :class="c.severity">
        <span class="cat">{{ tb(c.category) }}</span>
        <strong>{{ tb(c.title) }}</strong>
        <p v-if="c.detail" class="muted">{{ tb(c.detail) }}</p>
        <p v-for="(o, j) in c.objects.slice(0, 5)" :key="j" class="mono obj">{{ o }}</p>
      </div>
      <button v-if="fine.length" @click="showOk = !showOk">{{ $t('health.fine', { count: fine.length }) }}</button>
      <template v-if="showOk">
        <div v-for="(c, i) in fine" :key="'ok' + i" class="card chk ok">
          <span class="cat">{{ tb(c.category) }}</span>
          <span>{{ tb(c.title) }}</span>
        </div>
      </template>
      <p v-for="(s, i) in view.skipped" :key="'s' + i" class="muted">{{ tb(s) }}</p>
    </template>
  </div>
</template>

<style scoped>
.hp { display: flex; flex-direction: column; gap: 10px; }
.db { font: inherit; padding: 12px; border-radius: 8px; border: 1px solid var(--border); background: var(--bg-elev); color: var(--text-strong); }
.chk { display: flex; flex-direction: column; gap: 4px; border-left: 3px solid var(--text-dim); }
.chk p { margin: 0; }
.chk.critical { border-left-color: var(--danger); }
.chk.warning { border-left-color: var(--warning); }
.chk.info { border-left-color: var(--accent); }
.chk.ok { border-left-color: var(--success); }
.cat { font-size: 12px; text-transform: uppercase; letter-spacing: .5px; color: var(--text-dim); }
.obj { font-size: 12px; color: var(--text); word-break: break-word; }
.okmsg { color: var(--success); }
</style>
