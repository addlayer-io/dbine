// Tests for the spatial value parsers (geo.ts). Run: `node --test src/composables/geo.test.mjs`
// from web/ (Node 23.6+ strips the TypeScript types itself). Not part of the
// app's build: nothing imports this file.
//
// The hex vectors were taken from real servers (PostGIS 3.4, MySQL 8.4,
// MariaDB 11.8, SQL Server 2022) with the SQL shown next to each.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  RESULT_POINT_CAP, VALUE_POINT_CAP, detectSpatial, hexBytes, parseBinary, parseCell, parseGeoPackage, parseMysql,
  parseSpatiaLite, parseSqlServer, parseWkb, parseWkt, toFeatures, toWgs84,
} from './geo.ts';

const near = (a, b, eps = 1e-9) => assert.ok(Math.abs(a - b) < eps, `${a} ≉ ${b}`);
const nearPos = (p, q, eps) => { near(p[0], q[0], eps); near(p[1], q[1], eps); };

// -- WKB / EWKB (PostGIS) -------------------------------------------------------------

test('EWKB point with SRID (PostGIS text output, no 0x)', () => {
  // SELECT 'SRID=4326;POINT(-58.38 -34.6)'::geometry
  const v = parseCell('0101000020E6100000713D0AD7A3304DC0CDCCCCCCCC4C41C0');
  assert.equal(v.srid, 4326);
  assert.equal(v.geometry.type, 'Point');
  nearPos(v.geometry.coordinates, [-58.38, -34.6]);
});

test('EWKB geography is the same bytes', () => {
  // SELECT 'SRID=4326;POINT(-58.38 -34.6)'::geography
  const v = parseCell('0101000020E6100000713D0AD7A3304DC0CDCCCCCCCC4C41C0', '');
  nearPos(v.geometry.coordinates, [-58.38, -34.6]);
});

test('EWKB multipolygon with a hole', () => {
  // ST_GeomFromText('MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5),(5.2 5.1,5.8 5.1,5.8 5.7,5.2 5.1)))',4326)
  const hex = '0106000020E6100000020000000103000000010000000400000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F00000000000000000000000000000000010300000002000000040000000000000000001440000000000000144000000000000018400000000000001440000000000000184000000000000018400000000000001440000000000000144004000000CDCCCCCCCCCC14406666666666661440333333333333174066666666666614403333333333331740CDCCCCCCCCCC1640CDCCCCCCCCCC14406666666666661440';
  const v = parseCell(hex);
  assert.equal(v.srid, 4326);
  assert.equal(v.geometry.type, 'MultiPolygon');
  assert.equal(v.geometry.coordinates.length, 2);
  assert.equal(v.geometry.coordinates[1].length, 2, 'hole kept');
  nearPos(v.geometry.coordinates[1][1][2], [5.8, 5.7]);
});

test('EWKB geometry collection', () => {
  // 'SRID=4326;GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))'::geometry
  const v = parseCell('0107000020E6100000020000000101000000000000000000F03F000000000000004001020000000200000000000000000000000000000000000000000000000000F03F000000000000F03F');
  assert.equal(v.geometry.type, 'GeometryCollection');
  assert.deepEqual(v.geometry.geometries.map((g) => g.type), ['Point', 'LineString']);
});

test('ISO WKB with Z (type 1001) drops Z; POINT EMPTY is empty', () => {
  // ST_AsBinary('POINT Z (1 2 3)'::geometry)
  const v = parseWkb(hexBytes('0x01e9030000000000000000f03f00000000000000400000000000000840'));
  assert.deepEqual(v.geometry.coordinates, [1, 2]);
  assert.equal(v.srid, null);
  // 'POINT EMPTY'::geometry: NaN coordinates
  const e = parseCell('0101000000000000000000F87F000000000000F87F');
  assert.equal(e.geometry, null);
});

test('big-endian WKB', () => {
  const v = parseWkb(hexBytes('00000000013FF00000000000004000000000000000'));
  assert.deepEqual(v.geometry.coordinates, [1, 2]);
});

test('Web Mercator is reprojected', () => {
  // ST_Transform('SRID=4326;POINT(-58.38 -34.6)'::geometry, 3857)
  const v = parseCell('0101000020110F0000AF39D7F783CA58C1C30DE55DAB5A4FC1');
  assert.equal(v.srid, 3857);
  const w = toWgs84(v);
  nearPos(w.geometry.coordinates, [-58.38, -34.6], 1e-9);
});

test('random bytes and truncated values are not geometries', () => {
  assert.equal(parseCell('0xCAFEBABEDEADBEEF00112233'), null);
  assert.equal(parseCell('0x0101000020E6100000713D0AD7A3304DC0CDCC'), null);
  // json_bytes cut at 1 KiB ends with an ellipsis: not hex.
  assert.equal(parseCell('0x0101000020E6100000713D0AD7A3304DC0CDCCCCCCCCCC4C41…'), null);
  assert.equal(parseCell('deadbeef'), null);
});

// -- MySQL / MariaDB -------------------------------------------------------------------

test('MySQL internal format: SRID prefix + WKB, longitude first even for 4326', () => {
  // MySQL 8.4: HEX(ST_GeomFromText('POINT(-58.38 -34.6)', 4326, 'axis-order=long-lat'))
  const v = parseCell('0xE61000000101000000713D0AD7A3304DC0CDCCCCCCCC4C41C0', 'geometry');
  assert.equal(v.srid, 4326);
  nearPos(v.geometry.coordinates, [-58.38, -34.6]);
  // MariaDB 11.8: HEX(ST_GeomFromText('POINT(-58.38 -34.6)', 4326)) — same bytes
  nearPos(parseMysql(hexBytes('E61000000101000000713D0AD7A3304DC0CDCCCCCCCC4C41C0')).geometry.coordinates, [-58.38, -34.6]);
});

test('MySQL SRID 0 is not misread as big-endian WKB', () => {
  // HEX(ST_GeomFromText('POINT(1 2)'))
  const v = parseCell('0x000000000101000000000000000000F03F0000000000000040', 'geometry');
  assert.equal(v.srid, 0);
  assert.deepEqual(v.geometry.coordinates, [1, 2]);
  // HEX(ST_GeomFromText('POLYGON((0 0,3 0,3 3,0 3,0 0))', 0))
  const p = parseCell('0x00000000010300000001000000050000000000000000000000000000000000000000000000000008400000000000000000000000000000084000000000000008400000000000000000000000000000084000000000000000000000000000000000');
  assert.equal(p.geometry.type, 'Polygon');
  assert.equal(p.geometry.coordinates[0].length, 5);
});

// -- SQL Server ------------------------------------------------------------------------

test('SQL Server geometry point (single-point flag)', () => {
  // CAST(geometry::STGeomFromText('POINT(-58.38 -34.6)', 4326) AS varbinary(max))
  const v = parseCell('0xE6100000010C713D0AD7A3304DC0CDCCCCCCCC4C41C0', 'geometry');
  assert.equal(v.srid, 4326);
  nearPos(v.geometry.coordinates, [-58.38, -34.6]);
});

test('SQL Server geography stores latitude first', () => {
  // CAST(geography::STGeomFromText('POINT(-58.38 -34.6)', 4326) AS varbinary(max))
  const v = parseCell('0xE6100000010CCDCCCCCCCC4C41C0713D0AD7A3304DC0', 'geography');
  nearPos(v.geometry.coordinates, [-58.38, -34.6]);
});

test('SQL Server line segment, polygon with hole, multipoint, multipolygon, collection', () => {
  // LINESTRING(1 2,3 4): single-segment flag
  const l = parseSqlServer(hexBytes('0x000000000114000000000000F03F000000000000004000000000000008400000000000001040'), false);
  assert.deepEqual(l.geometry.coordinates, [[1, 2], [3, 4]]);
  // POLYGON((0 0,3 0,3 3,0 3,0 0),(1 1,2 1,2 2,1 1))
  const p = parseCell('0x000000000104090000000000000000000000000000000000000000000000000008400000000000000000000000000000084000000000000008400000000000000000000000000000084000000000000000000000000000000000000000000000F03F000000000000F03F0000000000000040000000000000F03F00000000000000400000000000000040000000000000F03F000000000000F03F020000000200000000000500000001000000FFFFFFFF0000000003', 'geometry');
  assert.equal(p.geometry.type, 'Polygon');
  assert.deepEqual(p.geometry.coordinates.map((r) => r.length), [5, 4]);
  // MULTIPOINT((1 2),(3 4))
  const mp = parseCell('0x00000000010402000000000000000000F03F000000000000004000000000000008400000000000001040020000000100000000010100000003000000FFFFFFFF0000000004000000000000000001000000000100000001', 'geometry');
  assert.deepEqual(mp.geometry, { type: 'MultiPoint', coordinates: [[1, 2], [3, 4]] });
  // MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))
  const mpo = parseCell('0x0000000001040800000000000000000000000000000000000000000000000000F03F0000000000000000000000000000F03F000000000000F03F0000000000000000000000000000000000000000000014400000000000001440000000000000184000000000000014400000000000001840000000000000184000000000000014400000000000001440020000000200000000020400000003000000FFFFFFFF0000000006000000000000000003000000000100000003', 'geometry');
  assert.equal(mpo.geometry.type, 'MultiPolygon');
  assert.equal(mpo.geometry.coordinates.length, 2);
  // GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1,2 0))
  const gc = parseCell('0x00000000010404000000000000000000F03F000000000000004000000000000000000000000000000000000000000000F03F000000000000F03F00000000000000400000000000000000020000000100000000010100000003000000FFFFFFFF0000000007000000000000000001000000000100000002', 'geometry');
  assert.equal(gc.geometry.type, 'GeometryCollection');
  assert.deepEqual(gc.geometry.geometries[1].coordinates, [[0, 0], [1, 1], [2, 0]]);
});

test('SQL Server v2 circular string (geography) is drawn through its points', () => {
  // CAST(geography::STGeomFromText('CIRCULARSTRING(-58 -34, -57 -33, -56 -34)', 4326) AS varbinary(max))
  const v = parseCell('0xE610000002040300000000000000000041C00000000000004DC000000000008040C00000000000804CC000000000000041C00000000000004CC001000000020000000001000000FFFFFFFF0000000008', 'geography');
  assert.equal(v.geometry.type, 'LineString');
  assert.deepEqual(v.geometry.coordinates, [[-58, -34], [-57, -33], [-56, -34]]);
});

// -- SpatiaLite --------------------------------------------------------------------------

test('SpatiaLite blob', () => {
  // MakePoint(-58.38, -34.6, 4326): 0x00 01 SRID MBR 7C class(1) x y FE
  const b = new Uint8Array(60);
  const dv = new DataView(b.buffer);
  b[0] = 0; b[1] = 1;
  dv.setInt32(2, 4326, true);
  [-58.38, -34.6, -58.38, -34.6].forEach((n, i) => dv.setFloat64(6 + 8 * i, n, true));
  b[38] = 0x7c;
  dv.setInt32(39, 1, true);
  dv.setFloat64(43, -58.38, true);
  dv.setFloat64(51, -34.6, true);
  b[59] = 0xfe;
  const v = parseSpatiaLite(b);
  assert.equal(v.srid, 4326);
  assert.deepEqual(v.geometry.coordinates, [-58.38, -34.6]);
  assert.deepEqual(parseBinary(b).geometry.coordinates, [-58.38, -34.6]);
});

// -- WKT --------------------------------------------------------------------------------

test('WKT and EWKT', () => {
  assert.deepEqual(parseWkt('POINT (-58.38 -34.6)'), { geometry: { type: 'Point', coordinates: [-58.38, -34.6] }, srid: null });
  assert.equal(parseWkt('SRID=4326;POINT(1 2)').srid, 4326);
  assert.deepEqual(parseWkt('POINT Z (1 2 3)').geometry.coordinates, [1, 2]);
  assert.deepEqual(parseWkt('POINTZ(1 2 3)').geometry.coordinates, [1, 2]);
  assert.deepEqual(parseWkt('MULTIPOINT(1 2, 3 4)').geometry.coordinates, [[1, 2], [3, 4]]);
  assert.deepEqual(parseWkt('MULTIPOINT((1 2),(3 4))').geometry.coordinates, [[1, 2], [3, 4]]);
  assert.equal(parseWkt('POLYGON((0 0,1 0,1 1,0 0),(0.2 0.1,0.8 0.1,0.8 0.7,0.2 0.1))').geometry.coordinates.length, 2);
  assert.equal(parseWkt('MULTIPOLYGON(((0 0,1 0,1 1,0 0)),((5 5,6 5,6 6,5 5)))').geometry.coordinates.length, 2);
  assert.equal(parseWkt('GEOMETRYCOLLECTION(POINT(1 2),LINESTRING(0 0,1 1))').geometry.geometries.length, 2);
  assert.equal(parseWkt('POINT EMPTY').geometry, null);
  assert.equal(parseWkt('LINESTRING EMPTY').geometry, null);
  assert.deepEqual(parseWkt('COMPOUNDCURVE(CIRCULARSTRING(0 0,1 1,2 0),(2 0,3 0))').geometry.coordinates, [[0, 0], [1, 1], [2, 0], [2, 0], [3, 0]]);
  assert.equal(parseWkt('POINT(1)'), null);
  assert.equal(parseWkt('POINT(1 2'), null);
  assert.equal(parseWkt('Point of sale'), null);
  assert.equal(parseWkt('hello'), null);
});

test('Oracle SDO_GEOMETRY as the driver sends it (EWKT from SDO_UTIL.TO_WKTGEOMETRY)', () => {
  // Oracle Free 23 (full image), SDO_GEOMETRY(2001, 4326, SDO_POINT_TYPE(-58.3816, -34.6037, NULL), NULL, NULL)
  const p = parseCell('SRID=4326;POINT (-58.3816 -34.6037)', 'SDO_GEOMETRY');
  assert.deepEqual(p, { geometry: { type: 'Point', coordinates: [-58.3816, -34.6037] }, srid: 4326 });
  // SRID 8307 (Oracle's WGS 84) comes as 4326; the rectangle shorthand (etype 1003, interpretation 3) as its polygon
  const g = parseCell('SRID=4326;POLYGON ((-59.0 -35.0, -58.0 -35.0, -58.0 -34.0, -59.0 -34.0, -59.0 -35.0))', 'SDO_GEOMETRY');
  assert.deepEqual(g.geometry.coordinates[0][2], [-58, -34]);
  // 3D point (SDO_GTYPE 3001): three numbers without a Z tag
  assert.deepEqual(parseCell('SRID=4326;POINT (1.0 2.0 0.0)', 'SDO_GEOMETRY').geometry.coordinates, [1, 2]);
  // No SDO_SRID: plain WKT, drawn when the numbers are degrees
  assert.equal(parseCell('POINT (10.0 20.0)', 'SDO_GEOMETRY').srid, null);
  // Oracle's legacy geodetic codes, should an EWKT carry them
  assert.ok('geometry' in toWgs84({ geometry: { type: 'Point', coordinates: [-58, -34] }, srid: 8307 }));
  const d = detectSpatial([{ name: 'ID', type_name: 'NUMBER' }, { name: 'Geom', type_name: 'SDO_GEOMETRY' }], [[1, 'SRID=4326;POINT (-58.3816 -34.6037)'], [3, null]]);
  assert.deepEqual(d.geometry, [1]);
});

// -- GeoJSON and points -----------------------------------------------------------------

test('GeoJSON as text (MongoDB, Snowflake) and as an object', () => {
  const v = parseCell('{"type":"Point","coordinates":[-58.38,-34.6]}');
  assert.deepEqual(v.geometry.coordinates, [-58.38, -34.6]);
  const f = parseCell({ type: 'Feature', geometry: { type: 'LineString', coordinates: [[0, 0], [1, 1]] }, properties: {} });
  assert.equal(f.geometry.type, 'LineString');
  // Elasticsearch geo_shape: lower case types, envelope
  assert.equal(parseCell('{"type":"polygon","coordinates":[[[0,0],[1,0],[1,1],[0,0]]]}').geometry.type, 'Polygon');
  assert.equal(parseCell('{"type":"envelope","coordinates":[[0,1],[1,0]]}').geometry.coordinates[0].length, 5);
  assert.equal(parseCell('{"type":"Point","coordinates":"x"}'), null);
  assert.equal(parseCell('{"name":"Ana"}'), null);
});

test('points: Elasticsearch geo_point, Neo4j, ClickHouse', () => {
  assert.deepEqual(parseCell('{"lat":-34.6,"lon":-58.38}').geometry.coordinates, [-58.38, -34.6]);
  // "lat,lon" text and [lon, lat] arrays only for columns that say they're points
  assert.deepEqual(parseCell('-34.6,-58.38', 'geo_point').geometry.coordinates, [-58.38, -34.6]);
  assert.equal(parseCell('-34.6,-58.38', ''), null);
  assert.deepEqual(parseCell('[-58.38,-34.6]', '', 'location').geometry.coordinates, [-58.38, -34.6]);
  assert.equal(parseCell('[-58.38,-34.6]', ''), null);
  // Neo4j point (the driver's JSON) and Cypher text
  const n = parseCell('{"srid":4326,"x":-58.38,"y":-34.6}');
  assert.equal(n.srid, 4326);
  assert.deepEqual(n.geometry.coordinates, [-58.38, -34.6]);
  assert.equal(parseCell('point({srid:4326, x:-58.38, y:-34.6})').srid, 4326);
  // Neo4j cartesian points: not degrees, reported as an unsupported SRID
  assert.deepEqual(toWgs84(parseCell('{"srid":7203,"x":10,"y":20}')), { skip: 'srid', srid: 7203 });
  // ClickHouse Point / Polygon / MultiPolygon (JSON arrays with the type name)
  assert.deepEqual(parseCell('[-58.38,-34.6]', 'Point').geometry.coordinates, [-58.38, -34.6]);
  assert.equal(parseCell('[[[0,0],[1,0],[1,1],[0,0]]]', 'Polygon').geometry.type, 'Polygon');
  assert.equal(parseCell('[[[[0,0],[1,0],[1,1],[0,0]]],[[[5,5],[6,5],[6,6],[5,5]]]]', 'MultiPolygon').geometry.coordinates.length, 2);
  assert.equal(parseCell('[[0,0],[1,1]]', 'LineString').geometry.type, 'LineString');
  assert.equal(parseCell('[[0,0],[1,0],[1,1],[0,0]]', 'Ring').geometry.type, 'Polygon');
});

test('coordinate systems', () => {
  assert.ok('geometry' in toWgs84({ geometry: { type: 'Point', coordinates: [-58, -34] }, srid: 5340 }), 'POSGAR 2007 as WGS 84');
  assert.deepEqual(toWgs84({ geometry: { type: 'Point', coordinates: [5600000, 6100000] }, srid: 22185 }), { skip: 'srid', srid: 22185 });
  assert.deepEqual(toWgs84({ geometry: { type: 'Point', coordinates: [5600000, 6100000] }, srid: null }), { skip: 'range', srid: null });
  assert.equal(toWgs84({ geometry: null, srid: 4326 }), null);
});

// -- columns ----------------------------------------------------------------------------

test('detects geometry columns and lat/lon pairs', () => {
  const columns = [
    { name: 'id', type_name: 'int' },
    { name: 'nombre', type_name: 'varchar' },
    { name: 'geom', type_name: '' },
    { name: 'latitud', type_name: 'float' },
    { name: 'longitud', type_name: 'float' },
    { name: 'notas', type_name: 'text' },
  ];
  const rows = [
    [1, 'Obelisco', '0101000020E6100000713D0AD7A3304DC0CDCCCCCCCC4C41C0', -34.6, -58.38, 'POINT de venta'],
    [2, 'Córdoba', null, '-31.42', '-64.18', null],
  ];
  const d = detectSpatial(columns, rows);
  assert.deepEqual(d.geometry, [2]);
  assert.equal(d.lat, 3);
  assert.equal(d.lon, 4);
  const pts = toFeatures(columns, rows, { kind: 'latlon', lat: 3, lon: 4 });
  assert.equal(pts.features.length, 2);
  assert.deepEqual(pts.features[1].geometry.coordinates, [-64.18, -31.42]);
  const g = toFeatures(columns, rows, { kind: 'geometry', column: 2 });
  assert.equal(g.features.length, 1);
});

test('a text column with one non-geometry value is not spatial', () => {
  const d = detectSpatial([{ name: 'w', type_name: 'text' }], [['POINT(1 2)'], ['hola']]);
  assert.deepEqual(d.geometry, []);
  const t = detectSpatial([{ name: 'w', type_name: 'geometry' }], [['POINT(1 2)'], ['0xCAFE']]);
  assert.deepEqual(t.geometry, [0], 'a spatial type needs one readable value');
});

test('the cap and the skipped SRIDs are counted', () => {
  const cols = [{ name: 'g', type_name: 'geometry' }];
  const rows = Array.from({ length: 30 }, (_, i) => [i < 25 ? `POINT(${i} 1)` : `SRID=22185;POINT(5600000 6100000)`]);
  const f = toFeatures(cols, rows, { kind: 'geometry', column: 0 }, 10);
  assert.equal(f.features.length, 10);
  assert.ok(f.overCap > 0);
  const all = toFeatures(cols, rows, { kind: 'geometry', column: 0 });
  assert.equal(all.features.length, 25);
  assert.equal(all.skippedSrid.get(22185), 5);
});

// -- untrusted counts and offsets -------------------------------------------------------

const timed = (f, ms) => {
  const t0 = performance.now();
  const v = f();
  const dt = performance.now() - t0;
  assert.ok(dt < ms, `took ${dt.toFixed(0)} ms`);
  return v;
};

test('SQL Server shape figure offset past the figures (72 hex chars) is not a geometry, quickly', () => {
  // SRID 4326, v1, no points, no figures; shape 0 a LineString at figure 0,
  // shape 1 at figure 0x7FFFFFFF: the range in between used to be pushed whole.
  const hex = 'E6100000' + '01' + '00' + '00000000' + '00000000' + '02000000'
    + 'FFFFFFFF' + '00000000' + '02' + '00000000' + 'FFFFFF7F' + '01';
  assert.equal(hex.length, 72);
  assert.equal(timed(() => parseCell(hex), 200), null);
  assert.equal(timed(() => parseCell('0x' + hex, 'geometry'), 200), null);
  const cols = [{ name: 'comment', type_name: 'nvarchar' }];
  assert.deepEqual(timed(() => detectSpatial(cols, Array.from({ length: 50 }, () => [hex])), 1000).geometry, []);
});

test('SQL Server offsets out of order or out of range are refused', () => {
  const head = 'E6100000' + '01' + '00';
  const pts = '02000000' + '0000000000000000'.repeat(4); // 2 points
  // Figure offset past the points.
  assert.equal(parseSqlServer(hexBytes(head + pts + '01000000' + '01' + '03000000' + '01000000' + 'FFFFFFFF0000000002'), false), null);
  // Figure offsets descending.
  assert.equal(parseSqlServer(hexBytes(head + pts + '02000000' + '01' + '01000000' + '01' + '00000000' + '01000000' + 'FFFFFFFF0000000002'), false), null);
  // Shape figure past the figures.
  assert.equal(parseSqlServer(hexBytes(head + pts + '01000000' + '01' + '00000000' + '01000000' + 'FFFFFFFF0500000002'), false), null);
  // Parent after the child, and a first shape with a parent.
  assert.equal(parseSqlServer(hexBytes(head + pts + '01000000' + '01' + '00000000' + '02000000' + 'FFFFFFFF0000000007' + '01000000' + '00000000' + '02'), false), null);
  assert.equal(parseSqlServer(hexBytes(head + pts + '01000000' + '01' + '00000000' + '01000000' + '000000000000000002'), false), null);
  // The same value, valid: LINESTRING(0 0,0 0).
  assert.deepEqual(parseSqlServer(hexBytes(head + pts + '01000000' + '01' + '00000000' + '01000000' + 'FFFFFFFF0000000002'), false).geometry,
    { type: 'LineString', coordinates: [[0, 0], [0, 0]] });
});

test('deeply nested collections are not read', () => {
  const n = 5000;
  // WKB: n GEOMETRYCOLLECTIONs of one, then a point.
  const wkb = '010700000001000000'.repeat(n) + '0101000000000000000000F03F0000000000000040';
  assert.equal(timed(() => parseCell(wkb), 1000), null);
  const wkt = 'GEOMETRYCOLLECTION('.repeat(n) + 'POINT(1 2)' + ')'.repeat(n);
  assert.equal(timed(() => parseCell(wkt), 1000), null);
  const json = '{"type":"GeometryCollection","geometries":['.repeat(n) + '{"type":"Point","coordinates":[1,2]}' + ']}'.repeat(n);
  assert.equal(timed(() => parseCell(json), 1000), null);
  // A few levels still read.
  assert.equal(parseCell('GEOMETRYCOLLECTION(GEOMETRYCOLLECTION(POINT(1 2)))').geometry.type, 'GeometryCollection');
});

test('a value with more positions than the cap is unreadable', () => {
  const n = VALUE_POINT_CAP + 1;
  const big = { type: 'MultiPoint', coordinates: Array.from({ length: n }, () => [1, 2]) };
  assert.equal(parseCell(big), null);
  const ok = { type: 'MultiPoint', coordinates: Array.from({ length: 1000 }, () => [1, 2]) };
  assert.equal(parseCell(ok).geometry.coordinates.length, 1000);
});

test('a result stops drawing past the positions cap', () => {
  const per = VALUE_POINT_CAP;
  const line = { type: 'LineString', coordinates: Array.from({ length: per }, (_, i) => [i % 180, 0]) };
  const rows = Array.from({ length: RESULT_POINT_CAP / per + 3 }, () => [line]);
  const f = toFeatures([{ name: 'g', type_name: 'geometry' }], rows, { kind: 'geometry', column: 0 });
  assert.equal(f.features.length, RESULT_POINT_CAP / per);
  assert.equal(f.overCap, 3);
});

test('random hex buffers finish quickly in every parser', () => {
  // A small deterministic PRNG, so a failure can be replayed.
  let seed = 0x9e3779b9;
  const rnd = () => { seed ^= seed << 13; seed ^= seed >>> 17; seed ^= seed << 5; return (seed >>> 0) / 2 ** 32; };
  const parsers = [
    (b) => parseWkb(b), (b) => parseMysql(b), (b) => parseGeoPackage(b), (b) => parseSpatiaLite(b),
    (b) => parseSqlServer(b, false), (b) => parseSqlServer(b, true), (b) => parseBinary(b),
  ];
  // Headers the parsers accept, so the random part reaches the counts and offsets.
  const prefixes = [[], [0xe6, 0x10, 0, 0, 1, 0], [0xe6, 0x10, 0, 0, 2, 0], [1, 7, 0, 0, 0], [0, 0, 0, 0, 1, 7, 0, 0, 0], [0x47, 0x50, 0, 1, 0, 0, 0, 0]];
  const t0 = performance.now();
  for (let i = 0; i < 3000; i++) {
    const pre = prefixes[i % prefixes.length];
    const len = 9 + Math.floor(rnd() * 120);
    const b = new Uint8Array(len);
    for (let k = 0; k < len; k++) {
      // Mostly small numbers and 0xFF runs: the values that make counts and offsets.
      const x = rnd();
      b[k] = x < 0.4 ? 0 : x < 0.55 ? 0xff : x < 0.7 ? Math.floor(rnd() * 4) : x < 0.75 ? 0x7f : Math.floor(rnd() * 256);
    }
    b.set(pre.slice(0, len));
    for (const p of parsers) {
      const s = performance.now();
      p(b);
      assert.ok(performance.now() - s < 100, `slow on ${Buffer.from(b).toString('hex')}`);
    }
    const hex = Buffer.from(b).toString('hex');
    parseCell(hex);
  }
  assert.ok(performance.now() - t0 < 10_000);
});
