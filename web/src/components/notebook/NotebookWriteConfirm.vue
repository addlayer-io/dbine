<script setup lang="ts">
import { computed, ref } from 'vue';
import type { WriteMode, WritePlan } from '../../api/types';
import { locale } from '../../i18n';
import { tb } from '../../i18n/backend';

// "Confirmar la escritura": asked before EVERY write of a write cell, also
// in "Ejecutar todo" (which stops here and can be cancelled whole). It
// says where (connection › database › table), how, how many rows and by
// which key. A production connection asks to type the table's name, as
// DBine's other destructive dialogs do. The backend only writes with the
// token this plan came with.

const props = defineProps<{
  plan: WritePlan;
  mode: WriteMode;
  /** A production connection (tagged prod, or a confirm_run environment). */
  prod: boolean;
  /** Inside "Ejecutar todo": "Cancelar todo" stops the whole run. */
  runAll: boolean;
}>();
const emit = defineEmits<{ decide: [choice: 'write' | 'cancel' | 'stop'] }>();

const n = (x: number) => x.toLocaleString(locale());
const typed = ref('');
const confirmed = computed(() => !props.prod || typed.value.trim() === props.plan.table);
const showDdl = ref(false);
</script>

<template>
  <el-dialog
    :model-value="true"
    :title="$t('notebook:write.confirm.title')"
    width="560px"
    append-to-body
    :close-on-click-modal="false"
    class="nwc"
    @update:model-value="(v: boolean) => { if (!v) emit('decide', 'cancel'); }"
  >
    <p class="nwc-lead">{{ $t('notebook:write.confirm.lead', { count: plan.rows, rows: n(plan.rows) }) }}</p>
    <div class="nwc-where" :class="{ prod }">
      <span>{{ plan.connection }}</span>
      <template v-if="plan.database"><span class="nwc-sep">›</span><span>{{ plan.database }}</span></template>
      <span class="nwc-sep">›</span><strong>{{ plan.table }}</strong>
      <span v-if="prod" class="nwc-prod">PROD</span>
    </div>
    <dl class="nwc-facts">
      <dt>{{ $t('notebook:write.mode') }}</dt>
      <dd>{{ $t(`notebook:write.modes.${mode}`) }}</dd>
      <template v-if="plan.keys.length">
        <dt>{{ $t('notebook:write.keys') }}</dt>
        <dd><code v-for="k in plan.keys" :key="k" class="nwc-key">{{ k }}</code></dd>
      </template>
      <dt>{{ $t('notebook:write.confirm.rows') }}</dt>
      <dd>{{ n(plan.rows) }}</dd>
      <dt>{{ $t('notebook:write.columns') }}</dt>
      <dd>{{ plan.columns.map((c) => c.target).join(', ') }}</dd>
    </dl>
    <div v-if="mode === 'replace'" class="nwc-alert danger">{{ $t('notebook:write.confirm.replace') }}</div>
    <div v-if="plan.create" class="nwc-alert">
      {{ $t('notebook:write.confirm.create') }}
      <button class="nwc-link" @click="showDdl = !showDdl">{{ showDdl ? $t('notebook:write.confirm.hideDdl') : $t('notebook:write.confirm.showDdl') }}</button>
      <pre v-if="showDdl" class="nwc-ddl nm-selectable">{{ plan.create }}</pre>
    </div>
    <p class="nwc-note">{{ plan.atomic ? $t('notebook:write.confirm.atomic') : $t('notebook:write.confirm.notAtomic') }}</p>
    <ul v-if="plan.warnings.length" class="nwc-warn">
      <li v-for="(x, i) in plan.warnings" :key="i">{{ tb(x) }}</li>
    </ul>
    <div v-if="prod" class="nwc-prod-confirm">
      <label for="nwc-typed">{{ $t('notebook:write.confirm.prod', { name: plan.table }) }}</label>
      <el-input id="nwc-typed" v-model="typed" :placeholder="plan.table" />
    </div>

    <template #footer>
      <el-button v-if="runAll" @click="emit('decide', 'stop')">{{ $t('notebook:write.confirm.stop') }}</el-button>
      <el-button @click="emit('decide', 'cancel')">{{ runAll ? $t('notebook:write.confirm.skip') : $t('common:cancel') }}</el-button>
      <el-button type="danger" :disabled="!confirmed" @click="emit('decide', 'write')">{{ $t('notebook:write.confirm.write', { count: plan.rows, rows: n(plan.rows) }) }}</el-button>
    </template>
  </el-dialog>
</template>

<style scoped>
.nwc-lead { margin: 0 0 8px; color: var(--nm-text); }
.nwc-where {
  display: flex; flex-wrap: wrap; align-items: center; gap: 6px; padding: 8px 10px; border-radius: 4px; font-size: 13.5px; color: var(--nm-text-strong);
  border: 1px solid var(--nm-border); background: var(--nm-overlay-code);
}
.nwc-where.prod { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.nwc-sep { color: var(--nm-text-muted); }
.nwc-prod { margin-left: auto; font-size: 10px; font-weight: 700; padding: 0 5px; border-radius: 3px; color: #fff; background: var(--nm-danger); }
.nwc-facts { display: grid; grid-template-columns: max-content 1fr; gap: 4px 14px; margin: 12px 0; font-size: 12.5px; }
.nwc-facts dt { color: var(--nm-text-dim); }
.nwc-facts dd { margin: 0; color: var(--nm-text); overflow-wrap: anywhere; }
.nwc-key { margin-right: 6px; padding: 0 5px; border-radius: 3px; font-family: var(--nm-mono); font-size: 11.5px; background: var(--nm-overlay-code); }
.nwc-alert { margin: 8px 0; padding: 6px 10px; border-radius: 4px; font-size: 12.5px; color: var(--nm-text); border: 1px solid color-mix(in srgb, var(--nm-warning) 55%, transparent); background: color-mix(in srgb, var(--nm-warning) 10%, transparent); }
.nwc-alert.danger { border-color: color-mix(in srgb, var(--nm-danger) 60%, transparent); background: color-mix(in srgb, var(--nm-danger) 12%, transparent); }
.nwc-link { margin-left: 6px; border: 0; padding: 0; background: none; font: inherit; color: var(--nm-accent); cursor: pointer; }
.nwc-ddl { margin: 6px 0 0; padding: 6px 8px; max-height: 160px; overflow: auto; font-family: var(--nm-mono); font-size: 11.5px; white-space: pre-wrap; background: var(--nm-overlay-code); border-radius: 3px; }
.nwc-note { margin: 8px 0; font-size: 12px; color: var(--nm-text-dim); line-height: 1.5; }
.nwc-warn { margin: 6px 0; padding-left: 18px; font-size: 12px; color: var(--nm-warning); }
.nwc-prod-confirm { display: flex; flex-direction: column; gap: 6px; margin-top: 10px; font-size: 12.5px; color: var(--nm-danger); }
</style>
