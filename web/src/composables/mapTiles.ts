// Online map tiles for the results' map view: off by default (the bundled
// Natural Earth base map needs no network). Configuración › Mapas turns
// them on with a provider; the provider then sees which area is being
// looked at (the tiles requested), never the rows: geometries are always
// drawn locally from the result values.
//
// The settings (provider, URL template, attribution) travel with the sync;
// an API key stays on this machine (localStorage), out of the state file and
// the sync. Tile keys for these providers are browser keys, meant to be
// visible to the client and restricted on the provider's side.

export type TileProvider = 'osm' | 'maptiler' | 'stadia' | 'custom';

export interface TileSettings {
  enabled: boolean;
  provider: TileProvider;
  /** Custom provider: XYZ template with {z}, {x}, {y} and optionally {key}. */
  url: string;
  /** Custom provider: attribution text shown on the map. */
  attribution: string;
}

export const TILE_SETTINGS = 'map.tiles';
/** The privacy notice was shown and accepted once. */
export const TILE_NOTICE = 'map.tiles.noticeSeen';

export const DEFAULT_TILES: TileSettings = { enabled: false, provider: 'osm', url: '', attribution: '' };

interface Preset {
  url: string;
  /** Variant for the dark theme. */
  dark?: string;
  attribution: string;
  needsKey: boolean;
}

export const PRESETS: Record<Exclude<TileProvider, 'custom'>, Preset> = {
  osm: {
    url: 'https://tile.openstreetmap.org/{z}/{x}/{y}.png',
    attribution: '© OpenStreetMap contributors',
    needsKey: false,
  },
  maptiler: {
    url: 'https://api.maptiler.com/maps/streets-v2/256/{z}/{x}/{y}.png?key={key}',
    dark: 'https://api.maptiler.com/maps/streets-v2-dark/256/{z}/{x}/{y}.png?key={key}',
    attribution: '© MapTiler © OpenStreetMap contributors',
    needsKey: true,
  },
  stadia: {
    url: 'https://tiles.stadiamaps.com/tiles/alidade_smooth/{z}/{x}/{y}.png?api_key={key}',
    dark: 'https://tiles.stadiamaps.com/tiles/alidade_smooth_dark/{z}/{x}/{y}.png?api_key={key}',
    attribution: '© Stadia Maps © OpenMapTiles © OpenStreetMap contributors',
    needsKey: true,
  },
};

const KEY_STORAGE = 'dbine.map.key.';

export function tileKey(provider: TileProvider): string {
  try { return localStorage.getItem(KEY_STORAGE + provider) ?? ''; } catch { return ''; }
}

export function setTileKey(provider: TileProvider, key: string) {
  try {
    if (key) localStorage.setItem(KEY_STORAGE + provider, key);
    else localStorage.removeItem(KEY_STORAGE + provider);
  } catch { /* storage off: the key lasts this run only */ }
}

/** An XYZ template the map accepts: https (or http on this machine, for a
 *  local tile server) with {z}, {x} and {y}. */
export function validTemplate(url: string): boolean {
  let u: URL;
  try {
    u = new URL(url.replace(/\{[a-z]\}/g, '0').replace('{key}', 'k'));
  } catch {
    return false;
  }
  const local = ['localhost', '127.0.0.1', '[::1]'].includes(u.hostname);
  return (u.protocol === 'https:' || (u.protocol === 'http:' && local)) && ['{z}', '{x}', '{y}'].every((p) => url.includes(p));
}

/** The tile layer to draw, or null (tiles off, or not usable yet: a missing
 *  key or an invalid template). */
export function tileLayerSpec(s: TileSettings, dark: boolean): { url: string; attribution: string } | null {
  if (!s.enabled) return null;
  const preset = s.provider === 'custom' ? null : PRESETS[s.provider];
  const template = preset ? (dark && preset.dark) || preset.url : s.url.trim();
  if (!template || !validTemplate(template)) return null;
  const key = tileKey(s.provider);
  if (template.includes('{key}') && !key) return null;
  return {
    url: template.replace('{key}', encodeURIComponent(key)),
    attribution: preset ? preset.attribution : s.attribution.trim(),
  };
}
