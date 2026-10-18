<script setup lang="ts">
import { computed, ref } from 'vue';
import { ElMessageBox } from 'element-plus';
import { useTranslation } from 'i18next-vue';
import {
  DEFAULT_TILES, PRESETS, TILE_NOTICE, TILE_SETTINGS, setTileKey, tileKey, tileLayerSpec, validTemplate,
  type TileProvider, type TileSettings,
} from '../composables/mapTiles';
import { useSettingsStore } from '../stores/settings';

// Configuración › Mapas: the map view's base map. Offline (the bundled
// Natural Earth) by default; online tiles only when turned on here, after a
// one-line notice that the provider sees the area being looked at.

const { t } = useTranslation();
const settings = useSettingsStore();

const tiles = computed<TileSettings>(() => ({ ...DEFAULT_TILES, ...settings.get<Partial<TileSettings>>(TILE_SETTINGS, {}) }));
function patch(p: Partial<TileSettings>) {
  settings.set(TILE_SETTINGS, { ...tiles.value, ...p });
}

const enabled = computed({
  get: () => tiles.value.enabled,
  set: async (on: boolean) => {
    if (on && !settings.get<boolean>(TILE_NOTICE, false)) {
      try {
        await ElMessageBox.confirm(t('map:privacy'), t('map:settings.noticeTitle'), {
          confirmButtonText: t('map:settings.noticeOk'),
          cancelButtonText: t('common:cancel'),
          type: 'info',
        });
      } catch {
        return;
      }
      settings.set(TILE_NOTICE, true);
    }
    patch({ enabled: on });
  },
});

const PROVIDERS: TileProvider[] = ['osm', 'maptiler', 'stadia', 'custom'];
const provider = computed({
  get: () => tiles.value.provider,
  set: (v: TileProvider) => { patch({ provider: v }); key.value = tileKey(v); },
});
const needsKey = computed(() => (tiles.value.provider === 'custom' ? tiles.value.url.includes('{key}') : PRESETS[tiles.value.provider].needsKey));

/** Kept on this machine only (localStorage), never in the synced settings. */
const key = ref(tileKey(tiles.value.provider));
function saveKey() { setTileKey(tiles.value.provider, key.value.trim()); bump.value++; }
const bump = ref(0);

const url = computed({ get: () => tiles.value.url, set: (v: string) => patch({ url: v }) });
const attribution = computed({ get: () => tiles.value.attribution, set: (v: string) => patch({ attribution: v }) });
const urlError = computed(() => tiles.value.provider === 'custom' && tiles.value.url.trim() !== '' && !validTemplate(tiles.value.url.trim()));

/** Why the map would still draw offline with tiles on. */
const problem = computed(() => {
  void bump.value;
  if (!tiles.value.enabled || tileLayerSpec(tiles.value, false)) return '';
  if (tiles.value.provider === 'custom' && !validTemplate(tiles.value.url.trim())) return t('map:settings.needUrl');
  return t('map:settings.needKey');
});
</script>

<template>
  <div class="ms">
    <h3>{{ $t('map:settings.title') }}</h3>
    <p class="ms-muted">{{ $t('map:settings.help') }}</p>
    <div class="ms-row">
      <div><strong>{{ $t('map:settings.offline') }}</strong><span>{{ $t('map:settings.offlineHelp') }}</span></div>
    </div>
    <div class="ms-row">
      <div><strong>{{ $t('map:settings.online') }}</strong><span>{{ $t('map:settings.onlineHelp') }}</span></div>
      <el-switch v-model="enabled" />
    </div>
    <template v-if="tiles.enabled">
      <p class="ms-privacy"><el-icon><ei-info-filled /></el-icon> {{ $t('map:privacy') }}</p>
      <div class="ms-row">
        <div><strong>{{ $t('map:settings.provider') }}</strong></div>
        <el-select v-model="provider" style="width: 260px">
          <el-option v-for="p in PROVIDERS" :key="p" :value="p" :label="$t(`map:settings.providers.${p}`)" />
        </el-select>
      </div>
      <p v-if="tiles.provider === 'osm'" class="ms-note">{{ $t('map:settings.osmPolicy') }}</p>
      <template v-if="tiles.provider === 'custom'">
        <div class="ms-row ms-col">
          <div><strong>{{ $t('map:settings.url') }}</strong><span>{{ $t('map:settings.urlHelp') }}</span></div>
          <el-input v-model.lazy="url" placeholder="https://tiles.example.com/{z}/{x}/{y}.png" spellcheck="false" />
          <span v-if="urlError" class="ms-err">{{ $t('map:settings.urlInvalid') }}</span>
        </div>
        <div class="ms-row ms-col">
          <div><strong>{{ $t('map:settings.attribution') }}</strong><span>{{ $t('map:settings.attributionHelp') }}</span></div>
          <el-input v-model.lazy="attribution" placeholder="© …" />
        </div>
      </template>
      <div v-if="needsKey" class="ms-row ms-col">
        <div><strong>{{ $t('map:settings.key') }}</strong><span>{{ $t('map:settings.keyHelp') }}</span></div>
        <el-input v-model="key" type="password" show-password autocomplete="off" spellcheck="false" @change="saveKey" @blur="saveKey" />
      </div>
      <p v-if="problem" class="ms-err">{{ problem }}</p>
    </template>
  </div>
</template>

<style scoped>
.ms h3 { margin: 0 0 4px; font-size: 16px; color: var(--nm-text-strong); }
.ms-muted { color: var(--nm-text-dim); font-size: 12.5px; line-height: 1.5; margin: 0 0 10px; }
.ms-row { display: flex; align-items: center; justify-content: space-between; gap: 20px; padding: 12px 0; border-bottom: 1px solid var(--nm-border); }
.ms-row > div { display: flex; flex-direction: column; gap: 3px; min-width: 0; }
.ms-row strong { font-size: 13px; color: var(--nm-text-strong); font-weight: 500; }
.ms-row span { font-size: 12px; color: var(--nm-text-dim); }
.ms-col { flex-direction: column; align-items: stretch; gap: 8px; }
.ms-privacy { display: flex; align-items: center; gap: 6px; margin: 10px 0 0; font-size: 12.5px; color: var(--nm-info); }
.ms-note { margin: 6px 0 0; font-size: 12px; line-height: 1.5; color: var(--nm-text-dim); }
.ms-err { margin: 6px 0 0; font-size: 12px; color: var(--nm-warning); }
</style>
