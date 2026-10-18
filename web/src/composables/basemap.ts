// The map view's offline base map: Natural Earth (public domain,
// naturalearthdata.com, v5.1.2), shipped with the app in
// public/basemap/natural-earth.topo.json and drawn as vector layers. Nothing
// is fetched from the network.
//
// The file is TopoJSON built with mapshaper from the 1:50m countries,
// coastline and lakes, the 1:10m admin-1 (provinces / states) boundary
// lines and populated places (scalerank ≤ 6 or a capital), simplified and
// quantized; scripts/basemap/build.sh rebuilds it.

import type { Geometry, Position } from './geo';

interface Topology {
  type: 'Topology';
  arcs: number[][][];
  transform?: { scale: [number, number]; translate: [number, number] };
  objects: Record<string, { type: 'GeometryCollection'; geometries: TopoGeometry[] }>;
}
type TopoGeometry =
  | { type: 'Point'; coordinates: number[]; properties?: Record<string, unknown> }
  | { type: 'MultiPoint'; coordinates: number[][]; properties?: Record<string, unknown> }
  | { type: 'LineString'; arcs: number[]; properties?: Record<string, unknown> }
  | { type: 'MultiLineString'; arcs: number[][]; properties?: Record<string, unknown> }
  | { type: 'Polygon'; arcs: number[][]; properties?: Record<string, unknown> }
  | { type: 'MultiPolygon'; arcs: number[][][]; properties?: Record<string, unknown> }
  | { type: null; properties?: Record<string, unknown> };

export interface BaseFeature {
  type: 'Feature';
  geometry: Geometry;
  properties: Record<string, unknown>;
}

export interface Place {
  name: string;
  lat: number;
  lng: number;
  /** Natural Earth's min_zoom: the zoom from which it's labelled. */
  z: number;
  /** scalerank: 0 the most important. */
  r: number;
}

export interface Basemap {
  countries: BaseFeature[];
  admin1: BaseFeature[];
  coast: BaseFeature[];
  lakes: BaseFeature[];
  places: Place[];
}

/** TopoJSON → GeoJSON features, per object (a small topojson-client). */
export function decodeTopology(topo: Topology): Record<string, BaseFeature[]> {
  const [sx, sy] = topo.transform?.scale ?? [1, 1];
  const [tx, ty] = topo.transform?.translate ?? [0, 0];
  const quantized = !!topo.transform;
  // Arcs are delta-encoded when quantized.
  const arcs: Position[][] = topo.arcs.map((arc) => {
    let x = 0;
    let y = 0;
    return arc.map(([dx, dy]) => {
      if (quantized) { x += dx; y += dy; } else { x = dx; y = dy; }
      return [x * sx + tx, y * sy + ty] as Position;
    });
  });
  const pt = (c: number[]): Position => [c[0] * sx + tx, c[1] * sy + ty];
  const line = (ids: number[]): Position[] => {
    const out: Position[] = [];
    ids.forEach((id, k) => {
      const a = id < 0 ? arcs[~id].slice().reverse() : arcs[id];
      out.push(...(k ? a.slice(1) : a));
    });
    return out;
  };
  const out: Record<string, BaseFeature[]> = {};
  for (const [name, obj] of Object.entries(topo.objects)) {
    out[name] = [];
    for (const g of obj.geometries) {
      let geometry: Geometry | null = null;
      switch (g.type) {
        case 'Point': geometry = { type: 'Point', coordinates: pt(g.coordinates) }; break;
        case 'MultiPoint': geometry = { type: 'MultiPoint', coordinates: g.coordinates.map(pt) }; break;
        case 'LineString': geometry = { type: 'LineString', coordinates: line(g.arcs) }; break;
        case 'MultiLineString': geometry = { type: 'MultiLineString', coordinates: g.arcs.map(line) }; break;
        case 'Polygon': geometry = { type: 'Polygon', coordinates: g.arcs.map(line) }; break;
        case 'MultiPolygon': geometry = { type: 'MultiPolygon', coordinates: g.arcs.map((p) => p.map(line)) }; break;
      }
      if (geometry) out[name].push({ type: 'Feature', geometry, properties: g.properties ?? {} });
    }
  }
  return out;
}

let loading: Promise<Basemap> | null = null;

/** The base map, loaded once (from the app's own files). */
export function loadBasemap(): Promise<Basemap> {
  loading ??= fetch(`${import.meta.env.BASE_URL}basemap/natural-earth.topo.json`)
    .then((r) => {
      if (!r.ok) throw new Error(`basemap ${r.status}`);
      return r.json() as Promise<Topology>;
    })
    .then((topo) => {
      const o = decodeTopology(topo);
      const places: Place[] = (o.places ?? []).flatMap((f) => (f.geometry.type === 'Point'
        ? [{
          name: String(f.properties.name ?? ''),
          lng: f.geometry.coordinates[0],
          lat: f.geometry.coordinates[1],
          z: Number(f.properties.z ?? 5),
          r: Number(f.properties.r ?? 9),
        }]
        : []));
      places.sort((a, b) => a.r - b.r || a.z - b.z);
      return { countries: o.countries ?? [], admin1: o.admin1 ?? [], coast: o.coast ?? [], lakes: o.lakes ?? [], places };
    })
    .catch((e) => {
      loading = null;
      throw e;
    });
  return loading;
}
