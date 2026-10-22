<script setup lang="ts">
/* eslint-disable @typescript-eslint/no-explicit-any -- Leaflet is loaded without types (see shims.d.ts) */
import { computed, onBeforeUnmount, onMounted, ref, shallowRef, watch } from 'vue';
import { useTranslation } from 'i18next-vue';
import * as L from 'leaflet';
import 'leaflet/dist/leaflet.css';
import type { Cell, ResultColumn } from '../api/types';
import { locale } from '../i18n';
import { detectSpatial, toFeatures, type Geometry, type MapSource, type SpatialColumns } from '../composables/geo';
import { loadBasemap, type Basemap, type Place } from '../composables/basemap';
import { DEFAULT_TILES, TILE_SETTINGS, tileLayerSpec, type TileSettings } from '../composables/mapTiles';
import { useSettingsStore } from '../stores/settings';
import { useUiStore } from '../stores/ui';
import type { MapSettings } from '../composables/chartSettings';

// The results' "Mapa" view: the result's geometries (a spatial column, or a
// latitude / longitude pair) over a base map. Everything is drawn here from
// the cell values: no row data leaves the machine. The base map is the
// bundled Natural Earth; online tiles only when the user turned them on in
// Configuración › Mapas (composables/mapTiles.ts).

const props = defineProps<{
  columns: ResultColumn[];
  rows: Cell[][];
  /** What detectSpatial found (the parent computed it to show the toggle). */
  detected?: SpatialColumns | null;
  /** The source to start from (a notebook cell keeps it), by column name;
   *  what's detected where it doesn't fit. */
  settings?: MapSettings | null;
}>();
/** The user picked another source (by column name). */
const emit = defineEmits<{ 'update:settings': [settings: MapSettings] }>();

const { t } = useTranslation();
const settingsStore = useSettingsStore();
const ui = useUiStore();

const found = computed(() => props.detected ?? detectSpatial(props.columns, props.rows));

// -- source: a geometry column or a latitude / longitude pair ------------------------
const LATLON = -1;
const at = (n: string | null | undefined) => (n ? props.columns.findIndex((c) => c.name === n) : -1);
const keptGeometry = at(props.settings?.column);
const keptLat = at(props.settings?.lat);
const keptLon = at(props.settings?.lon);
const keptLatLon = !props.settings?.column && keptLat >= 0 && keptLon >= 0;
const column = ref<number>(found.value.geometry.includes(keptGeometry) ? keptGeometry : keptLatLon ? LATLON : found.value.geometry[0] ?? LATLON);
const latCol = ref<number | null>(keptLatLon ? keptLat : found.value.lat);
const lonCol = ref<number | null>(keptLatLon ? keptLon : found.value.lon);
watch([column, latCol, lonCol], ([c, la, lo]) => {
  const nm = (i: number | null) => (i === null || i < 0 ? null : props.columns[i]?.name ?? null);
  emit('update:settings', c === LATLON ? { column: null, lat: nm(la), lon: nm(lo) } : { column: nm(c), lat: null, lon: null });
});
const numericCols = computed(() => props.columns.map((c, i) => ({ c, i })));
const source = computed<MapSource | null>(() => {
  if (column.value !== LATLON) return { kind: 'geometry', column: column.value };
  if (latCol.value === null || lonCol.value === null) return null;
  return { kind: 'latlon', lat: latCol.value, lon: lonCol.value };
});
const result = computed(() => (source.value ? toFeatures(props.columns, props.rows, source.value) : null));

const n = (v: number) => v.toLocaleString(locale());
const notices = computed(() => {
  const r = result.value;
  if (!r) return [];
  const out: string[] = [];
  if (r.overCap) out.push(t('map:notice.cap', { cap: n(r.features.length), rest: n(r.overCap) }));
  for (const [srid, count] of r.skippedSrid) out.push(t('map:notice.srid', { count, rows: n(count), srid: srid ?? '?' }));
  if (r.outOfRange) out.push(t('map:notice.range', { count: r.outOfRange, rows: n(r.outOfRange) }));
  if (r.unreadable) out.push(t('map:notice.unreadable', { count: r.unreadable, rows: n(r.unreadable) }));
  return out;
});

// -- map --------------------------------------------------------------------------
const el = ref<HTMLDivElement | null>(null);
let map: any = null;
const renderer = L.canvas({ padding: 0.5 });
let baseGroup: any = null;
let detailGroup: any = null;
let labelGroup: any = null;
let shapesLayer: any = null;
let pointLayer: any = null;
let tileLayer: any = null;
const basemap = shallowRef<Basemap | null>(null);
const baseFailed = ref(false);

interface Pt { lat: number; lng: number; row: number }
let points: Pt[] = [];

const dark = ref(!document.documentElement.classList.contains('light'));
const themeObserver = new MutationObserver(() => {
  dark.value = !document.documentElement.classList.contains('light');
});

function color(name: string, fallback: string): string {
  const v = el.value ? getComputedStyle(el.value).getPropertyValue(name).trim() : '';
  return v || fallback;
}
function palette() {
  return {
    land: color('--map-land', '#2b2d30'),
    border: color('--map-border', '#4b5057'),
    admin1: color('--map-admin1', '#3c4046'),
    coast: color('--map-coast', '#59626d'),
    water: color('--map-water', '#1a2330'),
    feature: color('--map-feature', '#3794ff'),
    featureFill: color('--map-feature-fill', '#3794ff'),
    point: color('--map-point', '#3794ff'),
    pointStroke: color('--map-point-stroke', '#ffffff'),
  };
}

const tiles = computed<TileSettings>(() => ({ ...DEFAULT_TILES, ...settingsStore.get<Partial<TileSettings>>(TILE_SETTINGS, {}) }));
const tileSpec = computed(() => tileLayerSpec(tiles.value, dark.value));
const online = computed(() => !!tileSpec.value);

function baseStyles() {
  const p = palette();
  return {
    countries: { renderer, interactive: false, color: p.border, weight: 0.7, fillColor: p.land, fillOpacity: 1 },
    lakes: { renderer, interactive: false, color: p.coast, weight: 0.4, fillColor: p.water, fillOpacity: 1 },
    coast: { renderer, interactive: false, color: p.coast, weight: 0.9, fill: false },
    admin1: { renderer, interactive: false, color: p.admin1, weight: 0.6, dashArray: '3 3', fill: false },
  };
}

function buildBase() {
  if (!map || !basemap.value) return;
  baseGroup?.remove();
  detailGroup?.remove();
  const s = baseStyles();
  const b = basemap.value;
  baseGroup = L.layerGroup([
    L.geoJSON({ type: 'FeatureCollection', features: b.countries }, { style: () => s.countries, ...s.countries }),
    L.geoJSON({ type: 'FeatureCollection', features: b.lakes }, { style: () => s.lakes, ...s.lakes }),
    L.geoJSON({ type: 'FeatureCollection', features: b.coast }, { style: () => s.coast, ...s.coast }),
  ]);
  detailGroup = L.geoJSON({ type: 'FeatureCollection', features: b.admin1 }, { style: () => s.admin1, ...s.admin1 });
  // Under online tiles the vector base stays as a fallback (tiles cover it);
  // its detail and labels go.
  baseGroup.addTo(map);
  baseGroup.eachLayer((l: any) => l.bringToBack());
  updateDetail();
}

/** Admin-1 lines from zoom 3 and place names by Natural Earth's min_zoom,
 *  only in the offline base map. */
function updateDetail() {
  if (!map) return;
  const z = map.getZoom();
  if (detailGroup) {
    const want = !online.value && z >= 3;
    if (want && !map.hasLayer(detailGroup)) detailGroup.addTo(map);
    if (!want && map.hasLayer(detailGroup)) detailGroup.remove();
  }
  labelGroup.clearLayers();
  if (online.value || !basemap.value) return;
  const bounds = map.getBounds().pad(0.1);
  // Most important first; a label that would overlap a placed one is skipped.
  const placed: [number, number, number, number][] = [];
  let shown = 0;
  for (const p of basemap.value.places as Place[]) {
    if (p.z > z + 1 || !bounds.contains([p.lat, p.lng])) continue;
    const c = map.latLngToContainerPoint([p.lat, p.lng]);
    const box: [number, number, number, number] = [c.x - 3, c.y - 9, c.x + 8 + p.name.length * 6.3, c.y + 7];
    if (placed.some((b) => box[0] < b[2] && box[2] > b[0] && box[1] < b[3] && box[3] > b[1])) continue;
    placed.push(box);
    L.marker([p.lat, p.lng], {
      interactive: false,
      keyboard: false,
      icon: L.divIcon({ className: `mv-place r${Math.min(p.r, 6)}`, html: `<i></i><span>${escapeHtml(p.name)}</span>`, iconSize: null }),
    }).addTo(labelGroup);
    if (++shown >= 220) break;
  }
}

function escapeHtml(s: string): string {
  return s.replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c] as string);
}

function updateTiles() {
  if (!map) return;
  tileLayer?.remove();
  tileLayer = null;
  const spec = tileSpec.value;
  if (spec) {
    tileLayer = L.tileLayer(spec.url, { maxZoom: 19, referrerPolicy: 'strict-origin-when-cross-origin' });
    tileLayer.addTo(map);
  }
  updateAttribution();
  updateDetail();
}

/** The legend in the corner: the base map's source, or the tiles' attribution. */
let attribution = '';
function updateAttribution() {
  if (!map) return;
  const a = map.attributionControl;
  if (attribution) a.removeAttribution(attribution);
  const spec = tileSpec.value;
  attribution = escapeHtml(spec ? spec.attribution || t('map:attribution.custom') : t('map:attribution.offline'));
  a.addAttribution(attribution);
}

// -- features ---------------------------------------------------------------------
function flattenPoints(g: Geometry, row: number, out: Pt[]): Geometry | null {
  switch (g.type) {
    case 'Point': out.push({ lng: g.coordinates[0], lat: g.coordinates[1], row }); return null;
    case 'MultiPoint': for (const c of g.coordinates) out.push({ lng: c[0], lat: c[1], row }); return null;
    case 'GeometryCollection': {
      const rest = g.geometries.map((x) => flattenPoints(x, row, out)).filter((x): x is Geometry => !!x);
      return rest.length ? { type: 'GeometryCollection', geometries: rest } : null;
    }
    default: return g;
  }
}

function drawFeatures(fit: boolean) {
  if (!map) return;
  shapesLayer?.remove();
  pointLayer?.clearLayers();
  points = [];
  const r = result.value;
  if (!r) return;
  const p = palette();
  const shapes: unknown[] = [];
  for (const f of r.features) {
    const rest = flattenPoints(f.geometry, f.row, points);
    if (rest) shapes.push({ type: 'Feature', geometry: rest, properties: { row: f.row } });
  }
  shapesLayer = L.geoJSON({ type: 'FeatureCollection', features: shapes }, {
    renderer,
    style: () => ({ color: p.feature, weight: 2, fillColor: p.featureFill, fillOpacity: 0.28 }),
    onEachFeature: (feat: { properties: { row: number } }, layer: any) => {
      layer.on('click', (e: any) => showRow([feat.properties.row], e.latlng));
    },
  }).addTo(map);
  renderPoints();
  if (fit) fitAll();
}

const CLUSTER_FROM = 250;
const CELL = 64;

/** Points, clustered on a pixel grid at the current zoom when there are many. */
function renderPoints() {
  if (!map) return;
  pointLayer.clearLayers();
  const p = palette();
  const dot = (pt: Pt) => L.circleMarker([pt.lat, pt.lng], {
    renderer, radius: 5, color: p.pointStroke, weight: 1.2, fillColor: p.point, fillOpacity: 0.9,
  }).on('click', (e: any) => showRow([pt.row], e.latlng));
  if (points.length <= CLUSTER_FROM) {
    for (const pt of points) dot(pt).addTo(pointLayer);
    return;
  }
  const z = map.getZoom();
  const view = map.getPixelBounds().pad(0.25);
  const cells = new Map<string, { x: number; y: number; members: Pt[] }>();
  for (const pt of points) {
    const px = map.project([pt.lat, pt.lng], z);
    if (!view.contains(px)) continue;
    const key = `${Math.floor(px.x / CELL)}:${Math.floor(px.y / CELL)}`;
    const c = cells.get(key);
    if (c) { c.x += px.x; c.y += px.y; c.members.push(pt); } else cells.set(key, { x: px.x, y: px.y, members: [pt] });
  }
  // Neighbouring cells whose clusters would overlap are merged, biggest first.
  const groups = [...cells.values()].map((c) => ({ x: c.x / c.members.length, y: c.y / c.members.length, members: c.members }));
  groups.sort((a, b) => b.members.length - a.members.length);
  const merged: typeof groups = [];
  for (const g of groups) {
    const near = merged.find((m) => m.members.length > 1 && g.members.length > 1 && Math.hypot(m.x - g.x, m.y - g.y) < CELL * 0.75);
    if (!near) { merged.push(g); continue; }
    const k = near.members.length + g.members.length;
    near.x = (near.x * near.members.length + g.x * g.members.length) / k;
    near.y = (near.y * near.members.length + g.y * g.members.length) / k;
    near.members.push(...g.members);
  }
  for (const c of merged) {
    if (c.members.length === 1) { dot(c.members[0]).addTo(pointLayer); continue; }
    const k = c.members.length;
    const at = map.unproject([c.x, c.y], z);
    const size = k < 10 ? 26 : k < 100 ? 32 : k < 1000 ? 38 : 44;
    L.marker(at, {
      keyboard: false,
      icon: L.divIcon({ className: 'mv-cluster', html: `<span>${k >= 1000 ? `${Math.round(k / 100) / 10}k` : k}</span>`, iconSize: [size, size] }),
    }).on('click', (e: any) => {
      const b = L.latLngBounds(c.members.map((m) => [m.lat, m.lng]));
      // Same spot (or as close as the map goes): the rows, not a zoom.
      if (map.getZoom() >= map.getMaxZoom() || b.getNorthEast().equals(b.getSouthWest(), 1e-9)) showRow(c.members.map((m) => m.row), e.latlng);
      else map.fitBounds(b, { padding: [30, 30], maxZoom: map.getMaxZoom() });
    }).addTo(pointLayer);
  }
}

function bounds() {
  let b: any = null;
  const add = (x: any) => { if (x?.isValid()) b = b ? b.extend(x) : L.latLngBounds(x.getSouthWest(), x.getNorthEast()); };
  if (shapesLayer && shapesLayer.getLayers().length) add(shapesLayer.getBounds());
  if (points.length) add(L.latLngBounds(points.map((pt) => [pt.lat, pt.lng])));
  return b;
}

function fitAll() {
  if (!map) return;
  const b = bounds();
  if (b) map.fitBounds(b, { padding: [24, 24], maxZoom: 15 });
  else map.setView([10, 0], 2);
}

// -- the row popup ----------------------------------------------------------------
const VALUE_CHARS = 160;
function showRow(rows: number[], at: any) {
  const box = document.createElement('div');
  box.className = 'mv-pop';
  const row = rows[0];
  const head = document.createElement('div');
  head.className = 'mv-pop-head';
  head.textContent = rows.length > 1
    ? t('map:popup.rows', { row: n(row + 1), count: rows.length - 1, more: n(rows.length - 1) })
    : t('map:popup.row', { row: n(row + 1) });
  box.append(head);
  const table = document.createElement('table');
  const src = source.value;
  props.columns.forEach((c, i) => {
    const tr = document.createElement('tr');
    const th = document.createElement('th');
    th.textContent = c.name;
    const td = document.createElement('td');
    const v = props.rows[row]?.[i] ?? null;
    if (src?.kind === 'geometry' && src.column === i) {
      td.textContent = t('map:popup.geometry');
      td.className = 'dim';
    } else if (v === null) {
      td.textContent = 'NULL';
      td.className = 'dim';
    } else {
      const s = typeof v === 'string' ? v : String(v);
      td.textContent = s.length > VALUE_CHARS ? `${s.slice(0, VALUE_CHARS)}…` : s;
      if (s.length > VALUE_CHARS) td.title = s.slice(0, 2000);
    }
    tr.append(th, td);
    table.append(tr);
  });
  box.append(table);
  L.popup({ maxWidth: 420, minWidth: 220, autoPanPadding: [20, 20] }).setLatLng(at).setContent(box).openOn(map);
}

// -- lifecycle --------------------------------------------------------------------
let resize: ResizeObserver | null = null;
onMounted(() => {
  if (!el.value) return;
  map = L.map(el.value, {
    zoomControl: true,
    attributionControl: true,
    worldCopyJump: true,
    minZoom: 1,
    maxZoom: 19,
    preferCanvas: true,
    renderer,
  });
  map.attributionControl.setPrefix(false);
  labelGroup = L.layerGroup().addTo(map);
  pointLayer = L.layerGroup().addTo(map);
  map.on('zoomend moveend', () => { updateDetail(); if (points.length > CLUSTER_FROM) renderPoints(); });
  map.setView([10, 0], 2);
  updateTiles();
  drawFeatures(true);
  loadBasemap()
    .then((b) => { basemap.value = b; buildBase(); })
    .catch(() => { baseFailed.value = true; });
  themeObserver.observe(document.documentElement, { attributes: true, attributeFilter: ['class'] });
  resize = new ResizeObserver(() => map?.invalidateSize());
  resize.observe(el.value);
});
onBeforeUnmount(() => {
  themeObserver.disconnect();
  resize?.disconnect();
  map?.remove();
  map = null;
});

watch(result, () => drawFeatures(true));
watch(tileSpec, (a, b) => { if (a?.url !== b?.url || a?.attribution !== b?.attribution) updateTiles(); });
watch(dark, () => { buildBase(); drawFeatures(false); });
watch(() => found.value, (f) => {
  if (column.value !== LATLON && !f.geometry.includes(column.value)) column.value = f.geometry[0] ?? LATLON;
});
</script>

<template>
  <div class="mv">
    <div class="mv-bar">
      <label class="mv-field">
        <span>{{ $t('map:source') }}</span>
        <el-select v-model="column" size="small" class="mv-select">
          <el-option v-for="ci in found.geometry" :key="ci" :value="ci" :label="columns[ci].name" />
          <el-option :value="LATLON" :label="$t('map:latLon')" />
        </el-select>
      </label>
      <template v-if="column === LATLON">
        <label class="mv-field">
          <span>{{ $t('map:lat') }}</span>
          <el-select v-model="latCol" size="small" class="mv-select" :placeholder="$t('map:pick')">
            <el-option v-for="o in numericCols" :key="o.i" :value="o.i" :label="o.c.name" />
          </el-select>
        </label>
        <label class="mv-field">
          <span>{{ $t('map:lon') }}</span>
          <el-select v-model="lonCol" size="small" class="mv-select" :placeholder="$t('map:pick')">
            <el-option v-for="o in numericCols" :key="o.i" :value="o.i" :label="o.c.name" />
          </el-select>
        </label>
      </template>
      <span v-if="result" class="mv-count">{{ $t('map:count', { count: result.features.length, shown: n(result.features.length) }) }}</span>
      <div class="nm-spacer" />
      <button class="mv-btn" :title="$t('map:fit')" @click="fitAll"><el-icon><ei-full-screen /></el-icon> {{ $t('map:fit') }}</button>
      <button class="mv-btn" :title="$t('map:openSettings')" @click="ui.openSettings('maps')"><el-icon><ei-setting /></el-icon></button>
    </div>
    <div v-for="(m, i) in notices" :key="i" class="mv-notice">{{ m }}</div>
    <div v-if="baseFailed" class="mv-notice">{{ $t('map:notice.basemap') }}</div>
    <div v-if="online" class="mv-notice mv-privacy"><el-icon><ei-info-filled /></el-icon> {{ $t('map:privacy') }}</div>
    <div ref="el" class="mv-map" />
  </div>
</template>

<style>
/* Map tokens: dark first (the app's default), light under html.light. */
:root {
  --map-water: #18202b;
  --map-land: #2a2c2f;
  --map-border: #4a4f56;
  --map-admin1: #3d4248;
  --map-coast: #58616c;
  --map-label: #a3a3a3;
  --map-label-strong: #d4d4d4;
  --map-label-halo: #18202b;
  --map-feature: #3794ff;
  --map-feature-fill: #3794ff;
  --map-point: #3794ff;
  --map-point-stroke: #e8f1ff;
  --map-cluster: rgba(55, 148, 255, 0.85);
  --map-cluster-ring: rgba(55, 148, 255, 0.3);
  --map-cluster-text: #ffffff;
}
html.light {
  --map-water: #cddceb;
  --map-land: #f3f1ec;
  --map-border: #b9b4aa;
  --map-admin1: #cfc9bf;
  --map-coast: #9fb4c8;
  --map-label: #5f5f5f;
  --map-label-strong: #2b2b2b;
  --map-label-halo: #f3f1ec;
  --map-feature: #0063c6;
  --map-feature-fill: #2f86e0;
  --map-point: #0063c6;
  --map-point-stroke: #ffffff;
  --map-cluster: rgba(0, 99, 198, 0.85);
  --map-cluster-ring: rgba(0, 99, 198, 0.25);
  --map-cluster-text: #ffffff;
}
.mv .leaflet-container { background: var(--map-water); font: 11px var(--nm-font); }
.mv .leaflet-control-attribution {
  background: color-mix(in srgb, var(--nm-bg) 80%, transparent); color: var(--nm-text-dim); font-size: 10.5px;
}
.mv .leaflet-bar a { background: var(--nm-bg-elev); color: var(--nm-text); border-color: var(--nm-border); }
.mv .leaflet-bar a:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.mv .leaflet-bar { border-color: var(--nm-border) !important; box-shadow: none !important; }
.mv .leaflet-popup-content-wrapper, .mv .leaflet-popup-tip { background: var(--nm-bg-elev); color: var(--nm-text); border: 1px solid var(--nm-border); box-shadow: 0 4px 14px rgba(0, 0, 0, 0.35); border-radius: 4px; }
.mv .leaflet-popup-content { margin: 8px 10px; font: 12px var(--nm-font); }
.mv .leaflet-container a.leaflet-popup-close-button { color: var(--nm-text-dim); }
.mv-pop-head { font-weight: 600; color: var(--nm-text-strong); margin-bottom: 4px; }
.mv-pop table { border-collapse: collapse; max-height: 260px; display: block; overflow: auto; }
.mv-pop th { text-align: left; font-weight: 500; color: var(--nm-text-dim); padding: 1px 10px 1px 0; vertical-align: top; white-space: nowrap; }
.mv-pop td { font-family: var(--nm-mono); font-size: 11.5px; padding: 1px 0; word-break: break-word; user-select: text; }
.mv-pop td.dim { color: var(--nm-text-muted); font-family: var(--nm-font); font-style: italic; }
.mv-cluster { display: flex; align-items: center; justify-content: center; border-radius: 50%; background: var(--map-cluster-ring); }
.mv-cluster span {
  display: flex; align-items: center; justify-content: center; width: calc(100% - 8px); height: calc(100% - 8px); border-radius: 50%;
  background: var(--map-cluster); color: var(--map-cluster-text); font-weight: 600; font-size: 11px; font-variant-numeric: tabular-nums;
}
.mv-place { white-space: nowrap; pointer-events: none; }
.mv-place i { position: absolute; left: -2px; top: -2px; width: 4px; height: 4px; border-radius: 50%; background: var(--map-label); }
.mv-place span {
  position: absolute; left: 5px; top: -8px; color: var(--map-label); font-size: 10.5px;
  text-shadow: 0 0 2px var(--map-label-halo), 0 0 2px var(--map-label-halo), 0 0 3px var(--map-label-halo);
}
.mv-place.r0 span, .mv-place.r1 span, .mv-place.r2 span { color: var(--map-label-strong); font-size: 11.5px; font-weight: 500; }
</style>

<style scoped>
.mv { flex: 1; min-height: 0; display: flex; flex-direction: column; }
.mv-bar { display: flex; align-items: center; gap: 10px; padding: 4px 10px; border-bottom: 1px solid var(--nm-border-soft); font-size: 12px; }
.mv-field { display: inline-flex; align-items: center; gap: 6px; color: var(--nm-text-dim); }
.mv-select { width: 170px; }
.mv-count { color: var(--nm-text-dim); font-variant-numeric: tabular-nums; }
.mv-btn { display: inline-flex; align-items: center; gap: 4px; padding: 2px 8px; border: 1px solid var(--nm-border); border-radius: 3px; background: transparent; color: var(--nm-text); cursor: pointer; font: inherit; }
.mv-btn:hover { background: var(--ide-hover); color: var(--nm-text-strong); }
.mv-notice { padding: 3px 10px; font-size: 11.5px; color: var(--nm-warning); border-bottom: 1px solid var(--nm-border-soft); }
.mv-privacy { color: var(--nm-text-dim); display: flex; align-items: center; gap: 5px; }
.mv-map { flex: 1; min-height: 160px; }
</style>
