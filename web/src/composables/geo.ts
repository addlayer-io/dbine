// Spatial values in result cells, for the results' map view (MapView.vue).
//
// Every engine hands its spatial values over in one of a few shapes, and the
// map reads them here, on this machine (nothing about the rows leaves it):
//
// - WKB / EWKB bytes, as `0x…` hex (most drivers) or bare hex (PostGIS text);
//   MySQL's internal format (a 4-byte SRID, then WKB); SQL Server's own
//   geometry / geography serialization; SpatiaLite and GeoPackage blobs.
// - WKT / EWKT text (`SRID=4326;POINT(…)`).
// - GeoJSON, as an object or as text (MongoDB, Elasticsearch, Snowflake…).
// - Points as `{lat, lon}` objects or `"lat,lon"` text (Elasticsearch
//   geo_point), `[lon, lat]` arrays and `{x, y, srid}` (Neo4j).
//
// Coordinates come out as GeoJSON positions: [x, y] = [longitude, latitude]
// once `toWgs84` ran. Only WGS 84 (4326, and the geographic systems that
// match it at map scale) and Web Mercator (3857) are drawn; other SRIDs are
// counted and reported.
//
// No imports on purpose: `node --test src/composables/geo.test.mjs` runs it.

export type Position = [number, number];
export type Geometry =
  | { type: 'Point'; coordinates: Position }
  | { type: 'MultiPoint'; coordinates: Position[] }
  | { type: 'LineString'; coordinates: Position[] }
  | { type: 'MultiLineString'; coordinates: Position[][] }
  | { type: 'Polygon'; coordinates: Position[][] }
  | { type: 'MultiPolygon'; coordinates: Position[][][] }
  | { type: 'GeometryCollection'; geometries: Geometry[] };

/** A decoded value. `geometry` null: an empty geometry (`POINT EMPTY`).
 *  `srid` null: the value didn't say. */
export interface Spatial {
  geometry: Geometry | null;
  srid: number | null;
}

export type CellValue = null | boolean | number | string | object;

/** Most positions one value may have to be drawn: past it the value is
 *  unreadable (a crafted value must not freeze the map). */
export const VALUE_POINT_CAP = 1_000_000;
/** Most positions drawn for a whole result: rows past it count as over the cap. */
export const RESULT_POINT_CAP = 5_000_000;
/** Deepest nesting of collections read (in every format). */
const MAX_DEPTH = 64;
/** Nested deeper than MAX_DEPTH: the whole value is unreadable. */
class TooDeep extends Error {}

/** How many positions `g` has, stopping once past `limit`. */
export function countPositions(g: Geometry, limit = Infinity): number {
  switch (g.type) {
    case 'Point': return 1;
    case 'MultiPoint': case 'LineString': return g.coordinates.length;
    case 'MultiLineString': case 'Polygon': {
      let n = 0;
      for (const r of g.coordinates) { n += r.length; if (n > limit) break; }
      return n;
    }
    case 'MultiPolygon': {
      let n = 0;
      for (const p of g.coordinates) { for (const r of p) n += r.length; if (n > limit) break; }
      return n;
    }
    case 'GeometryCollection': {
      let n = 0;
      for (const x of g.geometries) { n += countPositions(x, limit - n); if (n > limit) break; }
      return n;
    }
  }
}

// -- binary --------------------------------------------------------------------------

/** `0x0101…` or `0101…` (even length, hex only) as bytes; null otherwise. */
export function hexBytes(s: string): Uint8Array | null {
  const h = s.startsWith('0x') || s.startsWith('0X') ? s.slice(2) : s;
  if (h.length < 18 || h.length % 2 || !/^[0-9a-fA-F]+$/.test(h)) return null;
  const out = new Uint8Array(h.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = parseInt(h.substr(i * 2, 2), 16);
  return out;
}

class Reader {
  private dv: DataView;
  bytes: Uint8Array;
  pos: number;
  constructor(bytes: Uint8Array, pos = 0) {
    this.bytes = bytes;
    this.dv = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
    this.pos = pos;
  }
  get left() { return this.bytes.length - this.pos; }
  need(n: number) { if (n < 0 || this.left < n) throw new Error('short'); }
  u8() { this.need(1); return this.bytes[this.pos++]; }
  u32(le: boolean) { this.need(4); const v = this.dv.getUint32(this.pos, le); this.pos += 4; return v; }
  i32(le: boolean) { this.need(4); const v = this.dv.getInt32(this.pos, le); this.pos += 4; return v; }
  f64(le: boolean) { this.need(8); const v = this.dv.getFloat64(this.pos, le); this.pos += 8; return v; }
  /** A count of items of at least `size` bytes each: never more than what's left
   *  (random bytes must fail, not allocate gigabytes). */
  count(le: boolean, size: number) {
    const n = this.u32(le);
    if (n * size > this.left) throw new Error('count');
    return n;
  }
}

/** ISO / OGC type codes (WKB, and SpatiaLite's classes) with the dimensions. */
function baseType(t: number): { type: number; dims: number } {
  let z = false;
  let m = false;
  if (t & 0x80000000) z = true; // EWKB flags
  if (t & 0x40000000) m = true;
  t &= 0x0fffffff;
  if (t >= 3000) { z = m = true; t -= 3000; } else if (t >= 2000) { m = true; t -= 2000; } else if (t >= 1000) { z = true; t -= 1000; }
  return { type: t, dims: 2 + (z ? 1 : 0) + (m ? 1 : 0) };
}

function point(r: Reader, le: boolean, dims: number): Position {
  const x = r.f64(le);
  const y = r.f64(le);
  for (let i = 2; i < dims; i++) r.f64(le);
  return [x, y];
}
function points(r: Reader, le: boolean, dims: number): Position[] {
  const n = r.count(le, 8 * dims);
  const out: Position[] = new Array(n);
  for (let i = 0; i < n; i++) out[i] = point(r, le, dims);
  return out;
}

/** Lines of any curve kind as one LineString's points (arcs are drawn
 *  through their control points: close enough for a map overview). */
function asLine(g: Geometry | null): Position[] {
  if (!g) return [];
  if (g.type === 'LineString') return g.coordinates;
  if (g.type === 'MultiLineString') return g.coordinates.flat();
  return [];
}

/** One WKB / EWKB geometry from where `r` is. */
function readWkb(r: Reader, level = 0): Spatial {
  if (level > MAX_DEPTH) throw new Error('depth');
  const bo = r.u8();
  if (bo > 1) throw new Error('byte order');
  const le = bo === 1;
  const raw = r.u32(le);
  const srid = raw & 0x20000000 ? r.u32(le) : null;
  const { type, dims } = baseType(raw & ~0x20000000);
  const children = (): Geometry[] => {
    const n = r.count(le, 5);
    const out: Geometry[] = [];
    for (let i = 0; i < n; i++) {
      const g = readWkb(r, level + 1).geometry;
      if (g) out.push(g);
    }
    return out;
  };
  let g: Geometry | null;
  switch (type) {
    case 1: {
      const p = point(r, le, dims);
      g = Number.isNaN(p[0]) && Number.isNaN(p[1]) ? null : { type: 'Point', coordinates: p };
      break;
    }
    case 2: case 8: { // LineString, CircularString
      const c = points(r, le, dims);
      g = c.length ? { type: 'LineString', coordinates: c } : null;
      break;
    }
    case 3: case 17: { // Polygon, Triangle
      const n = r.count(le, 4);
      const rings: Position[][] = [];
      for (let i = 0; i < n; i++) rings.push(points(r, le, dims));
      g = rings.length ? { type: 'Polygon', coordinates: rings } : null;
      break;
    }
    case 9: { // CompoundCurve
      const c = children().flatMap(asLine);
      g = c.length ? { type: 'LineString', coordinates: c } : null;
      break;
    }
    case 10: { // CurvePolygon
      const rings = children().map(asLine).filter((x) => x.length);
      g = rings.length ? { type: 'Polygon', coordinates: rings } : null;
      break;
    }
    case 4: {
      const c = children().flatMap((x) => (x.type === 'Point' ? [x.coordinates] : []));
      g = c.length ? { type: 'MultiPoint', coordinates: c } : null;
      break;
    }
    case 5: case 11: { // MultiLineString, MultiCurve
      const c = children().map(asLine).filter((x) => x.length);
      g = c.length ? { type: 'MultiLineString', coordinates: c } : null;
      break;
    }
    case 6: case 12: case 15: case 16: { // MultiPolygon, MultiSurface, PolyhedralSurface, TIN
      const c = children().flatMap((x) => (x.type === 'Polygon' ? [x.coordinates] : x.type === 'MultiPolygon' ? x.coordinates : []));
      g = c.length ? { type: 'MultiPolygon', coordinates: c } : null;
      break;
    }
    case 7: {
      const c = children();
      g = c.length ? { type: 'GeometryCollection', geometries: c } : null;
      break;
    }
    default:
      throw new Error(`type ${type}`);
  }
  return { geometry: g, srid };
}

/** The whole buffer from `at` is one WKB value, or null. */
function wkbExact(bytes: Uint8Array, at = 0): Spatial | null {
  try {
    const r = new Reader(bytes, at);
    const v = readWkb(r);
    return r.left === 0 ? v : null;
  } catch {
    return null;
  }
}

/** WKB or EWKB (PostGIS, DuckDB's ST_AsWKB, HANA, TDengine…). */
export function parseWkb(bytes: Uint8Array): Spatial | null {
  return wkbExact(bytes);
}

/** MySQL / MariaDB: 4-byte little-endian SRID, then WKB. */
export function parseMysql(bytes: Uint8Array): Spatial | null {
  if (bytes.length < 9) return null;
  const v = wkbExact(bytes, 4);
  if (!v) return null;
  return { geometry: v.geometry, srid: new DataView(bytes.buffer, bytes.byteOffset).getUint32(0, true) };
}

/** GeoPackage blob: "GP", version, flags, SRS id, envelope, then WKB. */
export function parseGeoPackage(bytes: Uint8Array): Spatial | null {
  if (bytes.length < 8 || bytes[0] !== 0x47 || bytes[1] !== 0x50) return null;
  const flags = bytes[3];
  const le = (flags & 1) === 1;
  const env = [0, 32, 48, 48, 64][(flags >> 1) & 7];
  if (env === undefined) return null;
  const srid = new DataView(bytes.buffer, bytes.byteOffset).getInt32(4, le);
  if (flags & 0x10) return { geometry: null, srid };
  const v = wkbExact(bytes, 8 + env);
  return v ? { geometry: v.geometry, srid: srid > 0 ? srid : null } : null;
}

/** SpatiaLite blob: 0x00, byte order, SRID, MBR, 0x7C, class, body, 0xFE.
 *  Compressed geometries (CompressGeometry) aren't read. */
export function parseSpatiaLite(bytes: Uint8Array): Spatial | null {
  if (bytes.length < 44 || bytes[0] !== 0 || bytes[1] > 1 || bytes[38] !== 0x7c || bytes[bytes.length - 1] !== 0xfe) return null;
  const le = bytes[1] === 1;
  try {
    const r = new Reader(bytes, 2);
    const srid = r.i32(le);
    r.pos = 39;
    const body = (cls: number, level = 0): Geometry | null => {
      if (level > MAX_DEPTH) throw new Error('depth');
      const { type, dims } = baseType(cls);
      const kids = (): Geometry[] => {
        const n = r.count(le, 5);
        const out: Geometry[] = [];
        for (let i = 0; i < n; i++) {
          if (r.u8() !== 0x69) throw new Error('entity');
          const g = body(r.u32(le), level + 1);
          if (g) out.push(g);
        }
        return out;
      };
      switch (type) {
        case 1: return { type: 'Point', coordinates: point(r, le, dims) };
        case 2: return { type: 'LineString', coordinates: points(r, le, dims) };
        case 3: {
          const n = r.count(le, 4);
          const rings: Position[][] = [];
          for (let i = 0; i < n; i++) rings.push(points(r, le, dims));
          return { type: 'Polygon', coordinates: rings };
        }
        case 4: return { type: 'MultiPoint', coordinates: kids().flatMap((g) => (g.type === 'Point' ? [g.coordinates] : [])) };
        case 5: return { type: 'MultiLineString', coordinates: kids().flatMap((g) => (g.type === 'LineString' ? [g.coordinates] : [])) };
        case 6: return { type: 'MultiPolygon', coordinates: kids().flatMap((g) => (g.type === 'Polygon' ? [g.coordinates] : [])) };
        case 7: return { type: 'GeometryCollection', geometries: kids() };
        default: throw new Error('class');
      }
    };
    const g = body(r.u32(le));
    if (r.u8() !== 0xfe || r.left !== 0) return null;
    return { geometry: g, srid: srid > 0 ? srid : null };
  } catch {
    return null;
  }
}

/** SQL Server's geometry / geography serialization ([MS-SSCLRT]), versions 1
 *  and 2. Geography points are stored latitude first: `geography` swaps them
 *  to [longitude, latitude]. Arc segments are drawn through their points. */
export function parseSqlServer(bytes: Uint8Array, geography: boolean): Spatial | null {
  if (bytes.length < 6 || (bytes[4] !== 1 && bytes[4] !== 2)) return null;
  try {
    const le = true;
    const r = new Reader(bytes, 0);
    const srid = r.i32(le);
    const version = r.u8();
    const props = r.u8();
    const hasZ = (props & 0x01) !== 0;
    const hasM = (props & 0x02) !== 0;
    const single = (props & 0x08) !== 0;
    const segment = (props & 0x10) !== 0;
    const pt = (): Position => {
      const a = r.f64(le);
      const b = r.f64(le);
      return geography ? [b, a] : [a, b];
    };
    const skipZm = (n: number) => { for (let i = 0; i < n * ((hasZ ? 1 : 0) + (hasM ? 1 : 0)); i++) r.f64(le); };
    if (single || segment) {
      const pts = single ? [pt()] : [pt(), pt()];
      skipZm(pts.length);
      if (r.left !== 0) return null;
      return { geometry: single ? { type: 'Point', coordinates: pts[0] } : { type: 'LineString', coordinates: pts }, srid };
    }
    const nPoints = r.count(le, 16);
    const pts: Position[] = [];
    for (let i = 0; i < nPoints; i++) pts.push(pt());
    skipZm(nPoints);
    const nFigures = r.count(le, 5);
    const figures: number[] = [];
    for (let i = 0; i < nFigures; i++) {
      r.u8();
      // Point offsets: ascending, within the points.
      const off = r.i32(le);
      if (off < 0 || off > nPoints || (i > 0 && off < figures[i - 1])) return null;
      figures.push(off);
    }
    const nShapes = r.count(le, 9);
    const shapes: { parent: number; figure: number; type: number }[] = [];
    const children: number[][] = [];
    let lastFigure = 0;
    for (let i = 0; i < nShapes; i++) {
      const sh = { parent: r.i32(le), figure: r.i32(le), type: r.u8() };
      // The parent comes before its children (the first shape has none), and
      // the figure offsets are ascending, within the figures (-1: none).
      if (i === 0 ? sh.parent !== -1 : sh.parent < 0 || sh.parent >= i) return null;
      if (sh.figure !== -1) {
        if (sh.figure < lastFigure || sh.figure > nFigures) return null;
        lastFigure = sh.figure;
      }
      shapes.push(sh);
      children.push([]);
      if (i > 0) children[sh.parent].push(i);
    }
    if (version === 2 && r.left) {
      const nSeg = r.count(le, 1);
      for (let i = 0; i < nSeg; i++) r.u8();
    }
    if (r.left !== 0) return null;
    const figPoints = (f: number): Position[] => pts.slice(figures[f], f + 1 < figures.length ? figures[f + 1] : pts.length);
    // A shape's figures: from its offset to the next shape's that has any
    // (both validated above: within the figures, and in order).
    const figRange = (s: number): number[] => {
      const start = shapes[s].figure;
      if (start < 0) return [];
      let end = figures.length;
      for (let k = s + 1; k < shapes.length; k++) if (shapes[k].figure >= 0) { end = shapes[k].figure; break; }
      const out: number[] = [];
      for (let f = start; f < end; f++) out.push(f);
      return out;
    };
    const build = (s: number, level: number): Geometry | null => {
      if (level > MAX_DEPTH) throw new Error('depth');
      const sh = shapes[s];
      const kids = () => children[s].map((k) => build(k, level + 1)).filter((g): g is Geometry => !!g);
      switch (sh.type) {
        case 1: { const p = figRange(s).flatMap(figPoints)[0]; return p ? { type: 'Point', coordinates: p } : null; }
        case 2: case 8: case 9: { const c = figRange(s).flatMap(figPoints); return c.length ? { type: 'LineString', coordinates: c } : null; }
        case 3: case 10: { const rings = figRange(s).map(figPoints); return rings.length ? { type: 'Polygon', coordinates: rings } : null; }
        case 4: { const c = kids().flatMap((g) => (g.type === 'Point' ? [g.coordinates] : [])); return c.length ? { type: 'MultiPoint', coordinates: c } : null; }
        case 5: { const c = kids().flatMap((g) => (g.type === 'LineString' ? [g.coordinates] : [])); return c.length ? { type: 'MultiLineString', coordinates: c } : null; }
        case 6: { const c = kids().flatMap((g) => (g.type === 'Polygon' ? [g.coordinates] : [])); return c.length ? { type: 'MultiPolygon', coordinates: c } : null; }
        case 7: { const c = kids(); return c.length ? { type: 'GeometryCollection', geometries: c } : null; }
        default: return null; // FullGlobe and unknown kinds
      }
    };
    return { geometry: shapes.length ? build(0, 0) : null, srid };
  } catch {
    return null;
  }
}

/** Any of the binary formats above. `hint` is the column's type name. */
export function parseBinary(bytes: Uint8Array, hint = ''): Spatial | null {
  const h = hint.toLowerCase();
  const geography = h.includes('geography');
  return parseGeoPackage(bytes)
    ?? parseSpatiaLite(bytes)
    ?? parseWkb(bytes)
    ?? parseMysql(bytes)
    ?? parseSqlServer(bytes, geography);
}

// -- WKT -----------------------------------------------------------------------------

const WKT_START = /^\s*(SRID=-?\d+\s*;\s*)?(POINT|LINESTRING|POLYGON|MULTIPOINT|MULTILINESTRING|MULTIPOLYGON|GEOMETRYCOLLECTION|CIRCULARSTRING|COMPOUNDCURVE|CURVEPOLYGON|MULTICURVE|MULTISURFACE|TRIANGLE|TIN|POLYHEDRALSURFACE)(ZM|Z|M)?\b/i;

/** WKT or EWKT (`SRID=4326;POINT(1 2)`), with Z / M / ZM, EMPTY and curves. */
export function parseWkt(text: string): Spatial | null {
  if (!WKT_START.test(text)) return null;
  const toks = text.match(/[A-Za-z]+|[-+]?(?:\d+\.?\d*|\.\d+)(?:[eE][-+]?\d+)?|[(),;=]/g);
  if (!toks) return null;
  let i = 0;
  const peek = () => toks[i];
  const take = (t?: string) => {
    const v = toks[i++];
    if (v === undefined || (t && v.toUpperCase() !== t)) throw new Error('wkt');
    return v;
  };
  let srid: number | null = null;
  if (peek().toUpperCase() === 'SRID') {
    take(); take('=');
    srid = Number(take());
    take(';');
  }
  const isNum = (t: string | undefined) => t !== undefined && /^[-+.\d]/.test(t);
  const coord = (): Position => {
    const x = Number(take());
    const y = Number(take());
    while (isNum(peek())) take();
    if (!Number.isFinite(x) || !Number.isFinite(y)) throw new Error('wkt');
    return [x, y];
  };
  const list = <T>(item: () => T): T[] => {
    take('(');
    const out = [item()];
    while (peek() === ',') { take(); out.push(item()); }
    take(')');
    return out;
  };
  const coords = () => list(coord);
  // Inside COMPOUNDCURVE / CURVEPOLYGON / MULTICURVE: a bare list or a tagged curve.
  const curve = (): Position[] => {
    if (peek() === '(') return coords();
    return asLine(geom());
  };
  let level = 0;
  const geom = (): Geometry | null => {
    if (++level > MAX_DEPTH) throw new Error('depth');
    try { return geomBody(); } finally { level--; }
  };
  const geomBody = (): Geometry | null => {
    const kind = take().toUpperCase();
    let mod = peek()?.toUpperCase();
    if (mod === 'Z' || mod === 'M' || mod === 'ZM') { take(); mod = peek()?.toUpperCase(); }
    if (mod === 'EMPTY') { take(); return null; }
    const k = kind.replace(/(ZM|Z|M)$/, (m) => (['POINT', 'POLYGON', 'TIN'].some((b) => kind === b + m) ? '' : m));
    switch (k) {
      case 'POINT': return { type: 'Point', coordinates: list(coord)[0] };
      case 'LINESTRING': case 'CIRCULARSTRING': return { type: 'LineString', coordinates: coords() };
      case 'POLYGON': case 'TRIANGLE': return { type: 'Polygon', coordinates: list(coords) };
      case 'MULTIPOINT': return { type: 'MultiPoint', coordinates: list(() => (peek() === '(' ? list(coord)[0] : coord())) };
      case 'MULTILINESTRING': return { type: 'MultiLineString', coordinates: list(coords) };
      case 'MULTIPOLYGON': case 'POLYHEDRALSURFACE': case 'TIN': return { type: 'MultiPolygon', coordinates: list(() => list(coords)) };
      case 'COMPOUNDCURVE': return { type: 'LineString', coordinates: list(curve).flat() };
      case 'CURVEPOLYGON': return { type: 'Polygon', coordinates: list(curve) };
      case 'MULTICURVE': return { type: 'MultiLineString', coordinates: list(curve) };
      case 'MULTISURFACE': return { type: 'MultiPolygon', coordinates: list(() => (peek() === '(' ? list(coords) : polyOf(geom()))) };
      case 'GEOMETRYCOLLECTION': {
        const gs = list(geom).filter((g): g is Geometry => !!g);
        return { type: 'GeometryCollection', geometries: gs };
      }
      default: throw new Error('wkt');
    }
  };
  const polyOf = (g: Geometry | null): Position[][] => (g?.type === 'Polygon' ? g.coordinates : []);
  try {
    const g = geom();
    if (i !== toks.length) return null;
    return { geometry: g, srid };
  } catch {
    return null;
  }
}

// -- GeoJSON and points --------------------------------------------------------------

const isPos = (v: unknown): v is number[] => Array.isArray(v) && v.length >= 2 && v.length <= 4 && v.every((n) => typeof n === 'number' && Number.isFinite(n));
const pos = (v: unknown): Position => {
  if (!isPos(v)) throw new Error('position');
  return [v[0], v[1]];
};
const depth = <T>(v: unknown, f: (x: unknown) => T): T[] => {
  if (!Array.isArray(v)) throw new Error('array');
  return v.map(f);
};

/** A GeoJSON geometry (or Feature) object. */
export function fromGeoJson(o: unknown): Spatial | null {
  try {
    return geoJson(o, 0);
  } catch {
    return null;
  }
}

/** `fromGeoJson` at nesting `level`: throws TooDeep past MAX_DEPTH. */
function geoJson(o: unknown, level: number): Spatial | null {
  if (level > MAX_DEPTH) throw new TooDeep();
  if (!o || typeof o !== 'object' || Array.isArray(o)) return null;
  const v = o as Record<string, unknown>;
  try {
    if (v.type === 'Feature') return v.geometry === null ? { geometry: null, srid: null } : geoJson(v.geometry, level + 1);
    const c = v.coordinates;
    const srid = crsSrid(v.crs);
    switch (v.type) {
      case 'Point': return { geometry: { type: 'Point', coordinates: pos(c) }, srid };
      case 'MultiPoint': return { geometry: { type: 'MultiPoint', coordinates: depth(c, pos) }, srid };
      case 'LineString': return { geometry: { type: 'LineString', coordinates: depth(c, pos) }, srid };
      case 'MultiLineString': return { geometry: { type: 'MultiLineString', coordinates: depth(c, (r) => depth(r, pos)) }, srid };
      case 'Polygon': return { geometry: { type: 'Polygon', coordinates: depth(c, (r) => depth(r, pos)) }, srid };
      case 'MultiPolygon': return { geometry: { type: 'MultiPolygon', coordinates: depth(c, (p) => depth(p, (r) => depth(r, pos))) }, srid };
      case 'GeometryCollection': {
        const gs = depth(v.geometries, (g) => geoJson(g, level + 1)?.geometry ?? null).filter((g): g is Geometry => !!g);
        return { geometry: { type: 'GeometryCollection', geometries: gs }, srid };
      }
      // Elasticsearch geo_shape extras.
      case 'envelope': case 'Envelope': {
        const [[minx, maxy], [maxx, miny]] = depth(c, pos);
        return { geometry: { type: 'Polygon', coordinates: [[[minx, miny], [maxx, miny], [maxx, maxy], [minx, maxy], [minx, miny]]] }, srid };
      }
      default: {
        // Lower-case types (Elasticsearch accepts "point", "polygon"…).
        if (typeof v.type === 'string' && /^[a-z]/.test(v.type)) {
          const t = v.type.toLowerCase();
          const fixed = { linestring: 'LineString', multilinestring: 'MultiLineString', multipoint: 'MultiPoint', multipolygon: 'MultiPolygon', geometrycollection: 'GeometryCollection' }[t] ?? t[0].toUpperCase() + t.slice(1);
          if (fixed !== v.type) return geoJson({ ...v, type: fixed }, level + 1);
        }
        return null;
      }
    }
  } catch (e) {
    if (e instanceof TooDeep) throw e;
    return null;
  }
}

/** `"crs": {"type": "name", "properties": {"name": "EPSG:3857"}}`. */
function crsSrid(crs: unknown): number | null {
  const name = (crs as { properties?: { name?: unknown } } | undefined)?.properties?.name;
  if (typeof name !== 'string') return null;
  const m = /EPSG:{1,2}(\d+)$/i.exec(name);
  return m ? Number(m[1]) : /CRS84$/i.test(name) ? 4326 : null;
}

const num = (v: unknown): number | null => {
  const n = typeof v === 'number' ? v : typeof v === 'string' && v.trim() !== '' ? Number(v) : NaN;
  return Number.isFinite(n) ? n : null;
};

/** `{lat, lon}` / `{lat, lng}` / `{latitude, longitude}` / `{x, y[, srid]}`
 *  / `{longitude, latitude, srid}` (Neo4j's points), as a Point. */
function fromPointObject(o: Record<string, unknown>): Spatial | null {
  const keys = Object.keys(o).map((k) => k.toLowerCase());
  const get = (...names: string[]) => {
    for (const k of Object.keys(o)) if (names.includes(k.toLowerCase())) return num(o[k]);
    return null;
  };
  const lat = get('lat', 'latitude');
  const lon = get('lon', 'lng', 'long', 'longitude');
  const srid = get('srid', 'crs_srid');
  if (lat !== null && lon !== null) return { geometry: { type: 'Point', coordinates: [lon, lat] }, srid: srid ?? 4326 };
  const x = get('x');
  const y = get('y');
  if (x !== null && y !== null && keys.every((k) => ['x', 'y', 'z', 'srid', 'crs'].includes(k))) {
    return { geometry: { type: 'Point', coordinates: [x, y] }, srid };
  }
  return null;
}

/** Elasticsearch's `"lat,lon"` text. */
const LAT_LON_TEXT = /^\s*(-?\d+(?:\.\d+)?)\s*,\s*(-?\d+(?:\.\d+)?)\s*$/;

/** Neo4j's text form: `point({srid:4326, x:-58.4, y:-34.6})`. */
const NEO4J_POINT = /^\s*point\s*\(\s*(\{.*\})\s*\)\s*$/i;

/** ClickHouse's geo types: nested arrays named by the column type. */
function fromClickHouse(type: string, v: unknown): Spatial | null {
  const ring = (r: unknown) => depth(r, pos);
  try {
    switch (type) {
      case 'Point': return { geometry: { type: 'Point', coordinates: pos(v) }, srid: null };
      case 'LineString': return { geometry: { type: 'LineString', coordinates: ring(v) }, srid: null };
      case 'Ring': return { geometry: { type: 'Polygon', coordinates: [ring(v)] }, srid: null };
      case 'MultiLineString': return { geometry: { type: 'MultiLineString', coordinates: depth(v, ring) }, srid: null };
      case 'Polygon': return { geometry: { type: 'Polygon', coordinates: depth(v, ring) }, srid: null };
      case 'MultiPolygon': return { geometry: { type: 'MultiPolygon', coordinates: depth(v, (p) => depth(p, ring)) }, srid: null };
      default: return null;
    }
  } catch {
    return null;
  }
}

/** What a cell holds, as a geometry; null when it isn't one. `type` is the
 *  column's type name and `name` its name: they unlock the ambiguous shapes
 *  (`"lat,lon"` text, `[lon, lat]` arrays, ClickHouse's nested arrays) only
 *  for columns that say they're spatial. */
export function parseCell(v: CellValue, type = '', name = ''): Spatial | null {
  const s = parseAny(v, type, name);
  return s?.geometry && countPositions(s.geometry, VALUE_POINT_CAP) > VALUE_POINT_CAP ? null : s;
}

function parseAny(v: CellValue, type: string, name: string): Spatial | null {
  if (v === null || v === undefined || typeof v === 'boolean' || typeof v === 'number') return null;
  const pointy = /geo_?point|point|geo|loc(ation)?|coord|position/i.test(`${type} ${name}`);
  if (typeof v === 'object') {
    if (Array.isArray(v)) {
      const ch = fromClickHouse(type.trim(), v);
      if (ch) return ch;
      if (pointy && isPos(v)) return { geometry: { type: 'Point', coordinates: [v[0], v[1]] }, srid: 4326 };
      return null;
    }
    return fromGeoJson(v) ?? fromPointObject(v as Record<string, unknown>);
  }
  const s = v.trim();
  if (!s) return null;
  if (s[0] === '{' || s[0] === '[') {
    if (s.length > 20_000_000) return null;
    try { return parseAny(JSON.parse(s) as CellValue, type, name); } catch { return null; }
  }
  const neo = NEO4J_POINT.exec(s);
  if (neo) {
    // Cypher map literal: keys unquoted.
    try { return parseAny(JSON.parse(neo[1].replace(/([{,]\s*)([A-Za-z_]\w*)\s*:/g, '$1"$2":')) as CellValue, 'point', ''); } catch { return null; }
  }
  const wkt = parseWkt(s);
  if (wkt) return wkt;
  if (pointy) {
    const ll = LAT_LON_TEXT.exec(s);
    if (ll) return { geometry: { type: 'Point', coordinates: [Number(ll[2]), Number(ll[1])] }, srid: 4326 };
  }
  const bytes = hexBytes(s);
  return bytes ? parseBinary(bytes, type) : null;
}

// -- coordinate systems --------------------------------------------------------------

/** Geographic systems that match WGS 84 to within a few metres: drawn as is. */
export const WGS84_LIKE = new Set([4326, 4979, 4258, 4269, 4283, 7844, 4674, 5340, 4190, 4148, 4167, 4612, 6668, 4490, 4617, 4019, 4030,
  // Oracle's legacy geodetic codes: 8307 (WGS 84), 8265 (NAD 83).
  8307, 8265]);
/** Web Mercator under its several codes. */
export const MERCATOR = new Set([3857, 900913, 3785, 102100, 102113]);

const R = 6378137;
function unmercator([x, y]: Position): Position {
  return [(x / R) * (180 / Math.PI), (2 * Math.atan(Math.exp(y / R)) - Math.PI / 2) * (180 / Math.PI)];
}

function mapPositions(g: Geometry, f: (p: Position) => Position): Geometry {
  switch (g.type) {
    case 'Point': return { type: 'Point', coordinates: f(g.coordinates) };
    case 'MultiPoint': case 'LineString': return { type: g.type, coordinates: g.coordinates.map(f) } as Geometry;
    case 'MultiLineString': case 'Polygon': return { type: g.type, coordinates: g.coordinates.map((r) => r.map(f)) } as Geometry;
    case 'MultiPolygon': return { type: 'MultiPolygon', coordinates: g.coordinates.map((p) => p.map((r) => r.map(f))) };
    case 'GeometryCollection': return { type: 'GeometryCollection', geometries: g.geometries.map((x) => mapPositions(x, f)) };
  }
}

function everyPosition(g: Geometry, f: (p: Position) => boolean): boolean {
  switch (g.type) {
    case 'Point': return f(g.coordinates);
    case 'MultiPoint': case 'LineString': return g.coordinates.every(f);
    case 'MultiLineString': case 'Polygon': return g.coordinates.every((r) => r.every(f));
    case 'MultiPolygon': return g.coordinates.every((p) => p.every((r) => r.every(f)));
    case 'GeometryCollection': return g.geometries.every((x) => everyPosition(x, f));
  }
}

const lonLat = ([x, y]: Position) => Math.abs(x) <= 180 && Math.abs(y) <= 90;

/** The geometry in longitude / latitude, or why it can't be drawn: `srid`
 *  (an SRID the map doesn't reproject) or `range` (no SRID and coordinates
 *  that aren't degrees). */
export function toWgs84(s: Spatial): { geometry: Geometry } | { skip: 'srid' | 'range'; srid: number | null } | null {
  const g = s.geometry;
  if (!g) return null;
  const srid = s.srid;
  if (srid && MERCATOR.has(srid)) return { geometry: mapPositions(g, unmercator) };
  if (srid && !WGS84_LIKE.has(srid)) return { skip: 'srid', srid };
  if (everyPosition(g, lonLat)) return { geometry: g };
  return { skip: 'range', srid };
}

// -- columns ------------------------------------------------------------------------

/** Type names that say "spatial" in some engine. */
const SPATIAL_TYPE = /geometry|geography|geo_point|geo_shape|^point|polygon|linestring|^st_|sdo_geometry|spatial|geojson/i;

export interface SpatialColumns {
  /** Columns whose values are geometries. */
  geometry: number[];
  /** Latitude / longitude columns found by name, if any. */
  lat: number | null;
  lon: number | null;
}

const LAT_NAME = /^(lat|latitude|latitud|y_?lat|.*[_ ]lat|.*latitude|lat[_ ].*)$/i;
const LON_NAME = /^(lon|lng|long|longitude|longitud|x_?lon|.*[_ ](lon|lng)|.*longitude|(lon|lng)[_ ].*)$/i;

/** Which columns the map can draw, from the column types and a sample of
 *  the values (`sample` non-null values per column). */
export function detectSpatial(columns: { name: string; type_name: string }[], rows: CellValue[][], sample = 50): SpatialColumns {
  const geometry: number[] = [];
  columns.forEach((c, ci) => {
    const typed = SPATIAL_TYPE.test(c.type_name);
    let seen = 0;
    let ok = 0;
    for (const row of rows) {
      const v = row[ci];
      if (v === null || v === undefined || v === '') continue;
      if (typeof v === 'number' || typeof v === 'boolean') { seen = -1; break; }
      seen++;
      if (parseCell(v, c.type_name, c.name)) ok++;
      if (seen >= sample) break;
    }
    // A spatial type needs one value that reads; any other column, all of them.
    if (ok > 0 && (typed ? true : ok === seen)) geometry.push(ci);
  });
  const numeric = (ci: number, max: number) => {
    let seen = 0;
    for (const row of rows) {
      const n = row[ci] === null || row[ci] === undefined || row[ci] === '' ? null : num(row[ci]);
      if (row[ci] !== null && row[ci] !== undefined && row[ci] !== '' && (n === null || Math.abs(n) > max)) return false;
      if (n !== null && ++seen >= sample) break;
    }
    return seen > 0;
  };
  const lat = columns.findIndex((c, i) => LAT_NAME.test(c.name) && numeric(i, 90));
  const lon = columns.findIndex((c, i) => LON_NAME.test(c.name) && numeric(i, 180));
  return { geometry, lat: lat >= 0 && lon >= 0 ? lat : null, lon: lat >= 0 && lon >= 0 ? lon : null };
}

/** The source of the map: a geometry column or a latitude / longitude pair. */
export type MapSource = { kind: 'geometry'; column: number } | { kind: 'latlon'; lat: number; lon: number };

export interface MapFeature {
  /** Index into the rows. */
  row: number;
  geometry: Geometry;
}

export interface MapFeatures {
  features: MapFeature[];
  /** Rows with a value that wasn't drawn, by reason. */
  skippedSrid: Map<number | null, number>;
  outOfRange: number;
  unreadable: number;
  /** Rows past `cap` (or past RESULT_POINT_CAP positions) that weren't read. */
  overCap: number;
}

export const FEATURE_CAP = 10_000;

/** The features of `rows` for `source`, up to `cap`. */
export function toFeatures(columns: { name: string; type_name: string }[], rows: CellValue[][], source: MapSource, cap = FEATURE_CAP): MapFeatures {
  const out: MapFeatures = { features: [], skippedSrid: new Map(), outOfRange: 0, unreadable: 0, overCap: 0 };
  let points = 0;
  for (let r = 0; r < rows.length; r++) {
    let s: Spatial | null;
    if (source.kind === 'latlon') {
      const la = rows[r][source.lat];
      const lo = rows[r][source.lon];
      if (la === null || lo === null || la === '' || lo === '') continue;
      const lat = num(la);
      const lon = num(lo);
      s = lat === null || lon === null ? null : { geometry: { type: 'Point', coordinates: [lon, lat] }, srid: 4326 };
    } else {
      const v = rows[r][source.column];
      if (v === null || v === undefined || v === '') continue;
      s = parseCell(v, columns[source.column]?.type_name ?? '', columns[source.column]?.name ?? '');
    }
    if (!s) { out.unreadable++; continue; }
    const w = toWgs84(s);
    if (!w) continue;
    if ('skip' in w) {
      if (w.skip === 'srid') out.skippedSrid.set(w.srid, (out.skippedSrid.get(w.srid) ?? 0) + 1);
      else out.outOfRange++;
      continue;
    }
    const n = countPositions(w.geometry, RESULT_POINT_CAP);
    if (out.features.length >= cap || points + n > RESULT_POINT_CAP) { out.overCap = rows.length - r; break; }
    points += n;
    out.features.push({ row: r, geometry: w.geometry });
  }
  return out;
}
