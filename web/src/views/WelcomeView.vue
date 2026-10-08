<script setup lang="ts">
import { computed } from 'vue';
import { useTranslation } from 'i18next-vue';
import { FAMILY_LABELS, type Family } from '../api/types';
import { useConnectionsStore } from '../stores/connections';
import { useUiStore } from '../stores/ui';

// Shown when no tab is open.

const conns = useConnectionsStore();
const ui = useUiStore();
const { t } = useTranslation();

const families = computed(() => {
  const m = new Map<Family, string[]>();
  for (const d of conns.drivers) (m.get(d.family) ?? m.set(d.family, []).get(d.family)!).push(d.name);
  return [...m.entries()].map(([f, names]) => ({ label: t(`workbench:welcome.family.${f}`, FAMILY_LABELS[f] ?? f), names }));
});
</script>

<template>
  <div class="wv nm-content">
    <div class="wv-hero">
      <div>
        <h1><img class="wv-logo" src="/brand/dbine-logo-horizontal-dark.svg" alt="DBine" /></h1>
        <p class="nm-muted">{{ $t('workbench:welcome.tagline') }} · AddLayer</p>
      </div>
    </div>
    <div class="wv-actions">
      <el-button type="primary" @click="ui.newConnection()"><el-icon><ei-plus /></el-icon>&nbsp;{{ $t('workbench:welcome.newConnection') }}</el-button>
    </div>
    <div class="nm-card wv-tips">
      <div class="nm-section-title">{{ $t('workbench:welcome.howTo') }}</div>
      <ul>
        <li><i18next :translation="$t('workbench:welcome.tip1')"><template #queries><strong>{{ $t('workbench:welcome.queries') }}</strong></template></i18next></li>
        <li><i18next :translation="$t('workbench:welcome.tip2')"><template #newQuery><strong>{{ $t('workbench:welcome.newQuery') }}</strong></template></i18next></li>
        <li>{{ $t('workbench:welcome.tip3') }}</li>
        <li><kbd>⌘↵</kbd> {{ $t('workbench:welcome.keyRun') }} · <kbd>⌘B</kbd> {{ $t('workbench:welcome.keyExplorer') }} · <kbd>⌘J</kbd> {{ $t('workbench:welcome.keyOutput') }} · <kbd>⌘W</kbd> {{ $t('workbench:welcome.keyClose') }}.</li>
      </ul>
    </div>
    <div class="nm-card">
      <div class="nm-section-title">{{ $t('workbench:welcome.engines', { n: conns.drivers.length }) }}</div>
      <div v-for="f in families" :key="f.label" class="wv-family">
        <span class="wv-family-label">{{ f.label }}</span>
        <span class="nm-muted">{{ f.names.join(' · ') }}</span>
      </div>
    </div>
  </div>
</template>

<style scoped>
.wv { max-width: 860px; margin: 0 auto; padding-top: 48px; display: flex; flex-direction: column; gap: 16px; overflow: auto; }
.wv-hero { display: flex; align-items: center; gap: 16px; }
.wv-hero h1 { margin: 0; font-size: 26px; line-height: 0; }
.wv-logo { height: 64px; width: auto; }
.wv-hero p { margin: 4px 0 0; }
.wv-tips ul { margin: 0; padding-left: 18px; line-height: 1.8; }
.wv kbd { font-family: var(--nm-mono); font-size: 11px; padding: 0 4px; border: 1px solid var(--nm-border); border-radius: 3px; }
.wv-family { display: flex; gap: 12px; padding: 3px 0; }
.wv-family-label { width: 140px; flex-shrink: 0; color: var(--nm-text-strong); }
</style>
