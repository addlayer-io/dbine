<script setup lang="ts">
import { computed, onMounted, ref } from 'vue';
import { lintApi, type LintGroup, type LintRule } from '../api/lint';
import { LINT_DISABLED, LINT_ENABLED, LOCAL_LINT_RULES } from '../composables/lint';
import { useSettingsStore } from '../stores/settings';

// Configuración › Calidad de código: the linter on/off and each rule.

const settings = useSettingsStore();
const rules = ref<LintRule[]>([]);
const failed = ref(false);

onMounted(async () => {
  try {
    rules.value = [...(await lintApi.rules()), ...LOCAL_LINT_RULES];
  } catch {
    rules.value = [...LOCAL_LINT_RULES];
    failed.value = true;
  }
});

const enabled = computed({
  get: () => settings.get<boolean>(LINT_ENABLED, true),
  set: (v) => settings.set(LINT_ENABLED, v),
});
const disabled = computed(() => new Set(settings.get<string[]>(LINT_DISABLED, [])));

function toggle(id: string, on: boolean) {
  const next = new Set(disabled.value);
  if (on) next.delete(id);
  else next.add(id);
  settings.set(LINT_DISABLED, [...next].sort());
}

/** Rules under the first group they apply to, in the catalog's order. */
const grouped = computed(() => {
  const out: { group: LintGroup; rules: LintRule[] }[] = [];
  for (const r of rules.value) {
    const g = r.groups[0];
    let entry = out.find((e) => e.group === g);
    if (!entry) out.push((entry = { group: g, rules: [] }));
    entry.rules.push(r);
  }
  return out;
});
</script>

<template>
  <div class="ls">
    <h3>{{ $t('lint:nav') }}</h3>
    <p class="ls-muted">{{ $t('lint:settings.help') }}</p>
    <div class="ls-row">
      <div><strong>{{ $t('lint:settings.enabled') }}</strong><span>{{ $t('lint:settings.enabledHelp') }}</span></div>
      <el-switch v-model="enabled" />
    </div>

    <div class="ls-head">
      <h4>{{ $t('lint:settings.rules') }}</h4>
      <el-button v-if="disabled.size" size="small" text :disabled="!enabled" @click="settings.set(LINT_DISABLED, [])">{{ $t('lint:settings.allOn') }}</el-button>
    </div>
    <p v-if="failed" class="ls-muted">{{ $t('lint:settings.loadError') }}</p>
    <section v-for="g in grouped" :key="g.group" class="ls-group">
      <h5>{{ $t(`lint:groups.${g.group}`) }}</h5>
      <div v-for="r in g.rules" :key="r.id" class="ls-rule" :class="{ off: !enabled }">
        <el-switch size="small" :model-value="!disabled.has(r.id)" :disabled="!enabled" @update:model-value="(v: string | number | boolean) => toggle(r.id, !!v)" />
        <div class="ls-text">
          <div class="ls-title">
            <span class="ls-sev" :class="r.severity">{{ $t(`lint:severity.${r.severity}`) }}</span>
            <strong>{{ $t(`lint:rules.${r.id}.title`) }}</strong>
          </div>
          <span>{{ $t(`lint:rules.${r.id}.why`) }}</span>
          <span v-if="r.groups.length > 1" class="ls-engines">
            {{ $t('lint:settings.engines') }}: {{ r.groups.map((x) => $t(`lint:groups.${x}`)).join(' · ') }}
          </span>
        </div>
      </div>
    </section>
  </div>
</template>

<style scoped>
.ls h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.ls h4 { margin: 0; font-size: 13px; color: var(--nm-text-strong); }
.ls h5 { margin: 14px 0 4px; font-size: 12px; font-weight: 600; color: var(--nm-text-dim); text-transform: uppercase; letter-spacing: 0.04em; }
.ls-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.ls-row { display: flex; align-items: center; justify-content: space-between; gap: 20px; padding: 12px 0; border-bottom: 1px solid var(--nm-border); }
.ls-row > div { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
.ls-row strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 500; }
.ls-row span { font-size: 12px; color: var(--nm-text-dim); }
.ls-head { display: flex; align-items: center; justify-content: space-between; margin: 18px 0 2px; }
.ls-rule { display: flex; align-items: flex-start; gap: 10px; padding: 7px 0; border-bottom: 1px solid var(--nm-border-soft); }
.ls-rule.off { opacity: 0.55; }
.ls-rule .el-switch { flex: none; margin-top: 1px; }
.ls-text { display: flex; flex-direction: column; gap: 2px; min-width: 0; }
.ls-title { display: flex; align-items: center; gap: 8px; }
.ls-title strong { font-size: 12.5px; font-weight: 500; color: var(--nm-text-strong); }
.ls-text > span { font-size: 11.5px; line-height: 1.45; color: var(--nm-text-dim); }
.ls-text > span.ls-engines { color: var(--nm-text-muted); }
.ls-sev { font-size: 10.5px; padding: 0 5px; border-radius: 3px; border: 1px solid currentColor; line-height: 16px; flex: none; }
.ls-sev.error { color: var(--nm-danger); }
.ls-sev.warning { color: var(--nm-warning); }
.ls-sev.info { color: var(--nm-info); }
</style>
