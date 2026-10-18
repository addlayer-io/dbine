import { ref, watch } from 'vue';
import { useSettingsStore } from '../stores/settings';
import { readJson, writeJson } from '../stores/storage';

// Appearance: the colour theme (dark, light or the OS's), the UI zoom and
// the editor / grid font sizes. The preferences live in the settings store
// (they travel with the sync); a copy in localStorage applies them before
// the settings load, so the window never opens in the wrong theme or size.
//
// The theme is a class on <html>: `dark` (Element Plus's dark variables plus
// ours, global.scss) or `light`. Code that can't use CSS variables (the
// editor's highlighting, ECharts, exported SVGs) watches `resolvedTheme`.

export type ThemePref = 'dark' | 'light' | 'system';
export type Theme = 'dark' | 'light';

export const THEME_KEY = 'ui.theme';
export const ZOOM_KEY = 'ui.zoom';
export const EDITOR_FONT_KEY = 'editor.fontSize';
export const GRID_FONT_KEY = 'grid.fontSize';

export const DEFAULT_EDITOR_FONT = 13;
export const DEFAULT_GRID_FONT = 12;
export const FONT_SIZES = [10, 11, 12, 13, 14, 15, 16, 18, 20, 22, 24];

/** The zoom steps (as a browser's ⌘+ / ⌘−). */
export const ZOOM_LEVELS = [0.5, 0.67, 0.75, 0.8, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2];

const CACHE = 'dbine.appearance';
type Cache = { theme?: ThemePref; zoom?: number; editorFont?: number; gridFont?: number };

const cache = readJson<Cache>(CACHE, {});
const isThemePref = (v: unknown): v is ThemePref => v === 'dark' || v === 'light' || v === 'system';
const validZoom = (v: unknown) => (typeof v === 'number' && v >= ZOOM_LEVELS[0] && v <= ZOOM_LEVELS[ZOOM_LEVELS.length - 1] ? v : 1);
const validFont = (v: unknown, def: number) => (typeof v === 'number' && v >= 8 && v <= 32 ? v : def);

/** What the user picked. */
export const themePref = ref<ThemePref>(isThemePref(cache.theme) ? cache.theme : 'system');
/** What is shown (`system` resolved). */
export const resolvedTheme = ref<Theme>('dark');
export const zoom = ref(validZoom(cache.zoom));
export const editorFontSize = ref(validFont(cache.editorFont, DEFAULT_EDITOR_FONT));
export const gridFontSize = ref(validFont(cache.gridFont, DEFAULT_GRID_FONT));

const inTauri = () => '__TAURI_INTERNALS__' in window;
const media = typeof matchMedia === 'function' ? matchMedia('(prefers-color-scheme: dark)') : null;
/** The OS theme as Tauri reports it (more reliable than the media query on
 *  Linux); null until known. */
let osTheme: Theme | null = null;

/** Sets the classes and the colour scheme (Element Plus, scrollbars, form
 *  controls) for `t`. */
export function applyThemeClass(t: Theme) {
  const root = document.documentElement;
  root.classList.toggle('dark', t === 'dark');
  root.classList.toggle('light', t === 'light');
  root.style.colorScheme = t;
  resolvedTheme.value = t;
}

function systemTheme(): Theme {
  if (osTheme) return osTheme;
  return media && !media.matches ? 'light' : 'dark';
}

function resolve() {
  applyThemeClass(themePref.value === 'system' ? systemTheme() : themePref.value);
}

async function currentWindow() {
  if (!inTauri()) return null;
  try { return (await import('@tauri-apps/api/window')).getCurrentWindow(); } catch { return null; }
}

/** The native title bar follows the theme; `system` hands it back to the OS
 *  (which also makes the webview's prefers-color-scheme the OS's again). */
async function syncWindowTheme() {
  const win = await currentWindow();
  if (!win) return;
  const pref = themePref.value;
  try {
    await win.setTheme(pref === 'system' ? null : pref);
    if (pref === 'system') {
      const t = await win.theme();
      if (t) osTheme = t;
      resolve();
    }
  } catch { /* the window keeps its theme */ }
}

function applyFonts() {
  const s = document.documentElement.style;
  s.setProperty('--nm-editor-font-size', `${editorFontSize.value}px`);
  s.setProperty('--nm-grid-font-size', `${gridFontSize.value}px`);
}

/** Tauri zooms the whole webview (pop-ups and menus included); in a plain
 *  browser (the component preview) CSS zoom does it. */
async function applyZoom() {
  if (inTauri()) {
    try {
      const { getCurrentWebview } = await import('@tauri-apps/api/webview');
      await getCurrentWebview().setZoom(zoom.value);
      return;
    } catch { /* fall back to CSS */ }
  }
  document.documentElement.style.setProperty('zoom', zoom.value === 1 ? '' : String(zoom.value));
}

function remember() {
  writeJson(CACHE, {
    theme: themePref.value, zoom: zoom.value, editorFont: editorFontSize.value, gridFont: gridFontSize.value,
  } satisfies Cache);
}

export function setThemePref(p: ThemePref) {
  useSettingsStore().set(THEME_KEY, p);
}
export function setEditorFontSize(n: number) {
  useSettingsStore().set(EDITOR_FONT_KEY, n);
}
export function setGridFontSize(n: number) {
  useSettingsStore().set(GRID_FONT_KEY, n);
}
function setZoom(z: number) {
  useSettingsStore().set(ZOOM_KEY, Math.round(z * 100) / 100);
}

/** One step up (`1`), down (`-1`) or back to 100 % (`0`). */
export function zoomStep(d: 1 | -1 | 0) {
  if (d === 0) { setZoom(1); return; }
  const cur = zoom.value;
  const next = d > 0 ? ZOOM_LEVELS.find((z) => z > cur + 0.001) : [...ZOOM_LEVELS].reverse().find((z) => z < cur - 0.001);
  if (next !== undefined) setZoom(next);
}

/** ⌘/Ctrl with "+", "−" or "0" (also on the numeric keypad). */
export function zoomKey(e: KeyboardEvent): 1 | -1 | 0 | null {
  if (!(e.metaKey || e.ctrlKey) || e.altKey) return null;
  if (e.key === '+' || e.key === '=' || e.code === 'NumpadAdd') return 1;
  if (e.key === '-' || e.key === '_' || e.code === 'NumpadSubtract') return -1;
  if (!e.shiftKey && (e.key === '0' || e.code === 'Numpad0')) return 0;
  return null;
}

let installed = false;

/** Applies the cached appearance at once, then follows the settings and the
 *  OS. Call once, after Pinia is installed and before mounting. */
export function installAppearance() {
  if (installed) return;
  installed = true;
  resolve();
  applyFonts();
  applyZoom();

  const settings = useSettingsStore();
  watch(() => settings.get<unknown>(THEME_KEY, null), (v) => {
    if (!settings.loaded && v === null) return;
    themePref.value = isThemePref(v) ? v : 'system';
  });
  watch(() => settings.get<unknown>(ZOOM_KEY, null), (v) => {
    if (!settings.loaded && v === null) return;
    zoom.value = validZoom(v);
  });
  watch(() => settings.get<unknown>(EDITOR_FONT_KEY, null), (v) => {
    if (!settings.loaded && v === null) return;
    editorFontSize.value = validFont(v, DEFAULT_EDITOR_FONT);
  });
  watch(() => settings.get<unknown>(GRID_FONT_KEY, null), (v) => {
    if (!settings.loaded && v === null) return;
    gridFontSize.value = validFont(v, DEFAULT_GRID_FONT);
  });
  // Settings loaded without these keys: the defaults (not this machine's cache).
  watch(() => settings.loaded, (l) => {
    if (!l) return;
    themePref.value = isThemePref(settings.values[THEME_KEY]) ? settings.values[THEME_KEY] as ThemePref : 'system';
    zoom.value = validZoom(settings.values[ZOOM_KEY]);
    editorFontSize.value = validFont(settings.values[EDITOR_FONT_KEY], DEFAULT_EDITOR_FONT);
    gridFontSize.value = validFont(settings.values[GRID_FONT_KEY], DEFAULT_GRID_FONT);
  });

  watch(themePref, () => { resolve(); syncWindowTheme(); remember(); });
  watch(zoom, () => { applyZoom(); remember(); });
  watch([editorFontSize, gridFontSize], () => { applyFonts(); remember(); });
  syncWindowTheme();

  media?.addEventListener('change', () => {
    osTheme = null;
    if (themePref.value === 'system') resolve();
  });
  currentWindow().then((win) => win?.onThemeChanged(({ payload }) => {
    if (themePref.value !== 'system') return;
    osTheme = payload;
    resolve();
  })).catch(() => {});

  // ⌘/Ctrl + "+" / "−" / "0", and ⌘/Ctrl + wheel, zoom the whole UI. Canvases
  // that zoom themselves (plans, diagrams) take the wheel first.
  window.addEventListener('keydown', (e) => {
    const d = zoomKey(e);
    if (d === null || e.defaultPrevented) return;
    e.preventDefault();
    zoomStep(d);
  });
  let wheel = 0;
  window.addEventListener('wheel', (e) => {
    if (!(e.ctrlKey || e.metaKey) || e.defaultPrevented) return;
    e.preventDefault();
    // A trackpad sends many small deltas: one step per ~100px of scrolling.
    wheel += e.deltaMode === 0 ? e.deltaY : e.deltaY * 40;
    if (Math.abs(wheel) < 100) return;
    zoomStep(wheel < 0 ? 1 : -1);
    wheel = 0;
  }, { passive: false });
}
