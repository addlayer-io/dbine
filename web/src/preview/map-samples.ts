// view=map[&set=points|shapes|latlon][&theme=light]: the results' map view
// (MapView.vue) with sample spatial data over Argentina, on the bundled
// offline base map. Points come as EWKB hex (as PostGIS sends them), shapes
// as EWKT, and `set=latlon` as latitude / longitude columns.
import { h } from 'vue';
import ResultsPane from '../components/ResultsPane.vue';
import type { Cell, ResultColumn } from '../api/types';

const CITIES: [string, string, number, number, number][] = [
  ['Buenos Aires', 'CABA', -34.6037, -58.3816, 3121707],
  ['Córdoba', 'Córdoba', -31.4201, -64.1888, 1565112],
  ['Rosario', 'Santa Fe', -32.9442, -60.6505, 1276000],
  ['Mendoza', 'Mendoza', -32.8895, -68.8458, 1115041],
  ['San Miguel de Tucumán', 'Tucumán', -26.8083, -65.2176, 903279],
  ['La Plata', 'Buenos Aires', -34.9214, -57.9544, 893000],
  ['Mar del Plata', 'Buenos Aires', -38.0055, -57.5426, 682605],
  ['Salta', 'Salta', -24.7821, -65.4232, 618375],
  ['Santa Fe', 'Santa Fe', -31.6333, -60.7, 525093],
  ['San Juan', 'San Juan', -31.5375, -68.5364, 471389],
  ['Resistencia', 'Chaco', -27.4606, -58.9839, 387158],
  ['Neuquén', 'Neuquén', -38.9516, -68.0591, 341301],
  ['Santiago del Estero', 'Santiago del Estero', -27.7951, -64.2615, 360923],
  ['Corrientes', 'Corrientes', -27.4692, -58.8306, 358223],
  ['Posadas', 'Misiones', -27.3671, -55.8961, 357119],
  ['Bahía Blanca', 'Buenos Aires', -38.7183, -62.2663, 301572],
  ['Paraná', 'Entre Ríos', -31.7319, -60.5238, 268000],
  ['San Salvador de Jujuy', 'Jujuy', -24.1858, -65.2995, 265249],
  ['San Luis', 'San Luis', -33.295, -66.3356, 204019],
  ['Río Cuarto', 'Córdoba', -33.1232, -64.3493, 158298],
  ['Comodoro Rivadavia', 'Chubut', -45.8641, -67.4966, 182631],
  ['San Carlos de Bariloche', 'Río Negro', -41.1335, -71.3103, 135755],
  ['Río Gallegos', 'Santa Cruz', -51.6226, -69.2181, 113267],
  ['Ushuaia', 'Tierra del Fuego', -54.8019, -68.303, 82615],
  ['Santa Rosa', 'La Pampa', -36.6203, -64.2906, 126000],
  ['Rawson', 'Chubut', -43.3002, -65.1023, 31787],
];

/** EWKB hex of a point with SRID 4326 (PostGIS's text output). */
function ewkbPoint(lon: number, lat: number): string {
  const b = new DataView(new ArrayBuffer(25));
  b.setUint8(0, 1);
  b.setUint32(1, 0x20000001, true);
  b.setUint32(5, 4326, true);
  b.setFloat64(9, lon, true);
  b.setFloat64(17, lat, true);
  return Array.from(new Uint8Array(b.buffer), (x) => x.toString(16).padStart(2, '0')).join('').toUpperCase();
}

/** Deterministic noise, so screenshots don't change. */
function rng(seed: number) {
  return () => {
    seed = (seed * 1664525 + 1013904223) % 4294967296;
    return seed / 4294967296;
  };
}

function points(): { columns: ResultColumn[]; rows: Cell[][] } {
  const r = rng(7);
  const rows: Cell[][] = [];
  let id = 1;
  for (const [name, prov, lat, lon, pop] of CITIES) {
    rows.push([id++, name, prov, pop, ewkbPoint(lon, lat)]);
    // Customers around each city, more around the big ones.
    const n = Math.round(Math.sqrt(pop) / 6);
    for (let i = 0; i < n; i++) {
      const d = 0.6 * Math.sqrt(-2 * Math.log(r() + 1e-9));
      const a = 2 * Math.PI * r();
      rows.push([id++, `Cliente ${id}`, prov, null, ewkbPoint(lon + d * Math.cos(a), lat + 0.8 * d * Math.sin(a))]);
    }
  }
  return {
    columns: [
      { name: 'id', type_name: 'int4' },
      { name: 'nombre', type_name: 'text' },
      { name: 'provincia', type_name: 'text' },
      { name: 'poblacion', type_name: 'int4' },
      { name: 'ubicacion', type_name: '' },
    ],
    rows,
  };
}

function shapes(): { columns: ResultColumn[]; rows: Cell[][] } {
  const poly = (ring: [number, number][]) => `SRID=4326;POLYGON((${[...ring, ring[0]].map(([x, y]) => `${x} ${y}`).join(', ')}))`;
  return {
    columns: [
      { name: 'id', type_name: 'int4' },
      { name: 'zona', type_name: 'text' },
      { name: 'tipo', type_name: 'text' },
      { name: 'superficie_km2', type_name: 'numeric' },
      { name: 'geom', type_name: 'geometry' },
    ],
    rows: [
      [1, 'Zona núcleo', 'Región', '96500.5', poly([[-64.2, -31.3], [-60.4, -31.0], [-59.0, -33.6], [-60.6, -35.3], [-63.9, -34.6], [-64.8, -32.9]])],
      [2, 'Área metropolitana', 'Región', '13285.0', poly([[-59.1, -34.25], [-58.2, -34.2], [-57.85, -34.75], [-58.3, -35.05], [-59.05, -34.95]])],
      [3, 'Cuyo', 'Región', '404906.0', poly([[-70.2, -28.4], [-66.8, -28.6], [-65.3, -33.0], [-66.6, -35.9], [-69.8, -37.5], [-70.6, -33.2]])],
      [4, 'Valle de Río Negro', 'Región', '12500.0', 'SRID=4326;MULTIPOLYGON(((-68.3 -38.85, -67.0 -38.95, -66.6 -39.3, -67.9 -39.25, -68.3 -38.85)),((-65.4 -39.9, -64.6 -40.0, -64.8 -40.45, -65.5 -40.3, -65.4 -39.9)))'],
      [5, 'Ruta Nacional 9 (Buenos Aires–Córdoba)', 'Ruta', null, 'SRID=4326;LINESTRING(-58.38 -34.6, -58.9 -34.3, -59.65 -33.9, -60.65 -32.94, -61.6 -32.6, -62.65 -32.4, -63.25 -32.4, -64.19 -31.42)'],
      [6, 'Ruta Nacional 3 (tramo sur)', 'Ruta', null, 'SRID=4326;LINESTRING(-62.27 -38.72, -62.8 -40.8, -65.1 -43.3, -67.5 -45.86, -67.8 -48.0, -69.22 -51.62)'],
      [7, 'Obelisco', 'Punto', null, 'SRID=4326;POINT(-58.3816 -34.6037)'],
      [8, 'Aconcagua', 'Punto', null, 'POINT(-70.0109 -32.6532)'],
    ],
  };
}

function latLon(): { columns: ResultColumn[]; rows: Cell[][] } {
  return {
    columns: [
      { name: 'ciudad', type_name: 'varchar' },
      { name: 'provincia', type_name: 'varchar' },
      { name: 'latitud', type_name: 'float8' },
      { name: 'longitud', type_name: 'float8' },
      { name: 'poblacion', type_name: 'int4' },
    ],
    rows: CITIES.map(([n, p, lat, lon, pop]) => [n, p, lat, lon, pop]),
  };
}

export function mapPreview(params: URLSearchParams) {
  if (params.get('theme') === 'light') {
    document.documentElement.classList.remove('dark');
    document.documentElement.classList.add('light');
  }
  const set = params.get('set') ?? 'shapes';
  const data = set === 'points' ? points() : set === 'latlon' ? latLon() : shapes();
  // The pane opens on the grid; the preview presses "Mapa" for the screenshot.
  setTimeout(() => document.querySelector<HTMLButtonElement>('.rp-mode button:last-child')?.click(), 50);
  return h('div', { style: `width: ${params.get('w') ?? 1200}px; height: ${params.get('h') ?? 760}px; display: flex; flex-direction: column; background: var(--ide-editor)` }, [
    h(ResultsPane, {
      running: false, title: 'mapa',
      outcome: { results: [{ ...data, total_rows: data.rows.length, truncated: false, rows_affected: null }], messages: [], error: null, elapsed_ms: 21, plans: [] },
    }),
  ]);
}
