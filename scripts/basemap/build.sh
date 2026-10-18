#!/usr/bin/env bash
# Rebuilds the results' map view's offline base map
# (web/public/basemap/natural-earth.topo.json) from Natural Earth v5.1.2
# (public domain, naturalearthdata.com), with mapshaper:
#
#   scripts/basemap/build.sh <empty work dir>
#
# 1:50m countries, coastline and lakes; 1:10m admin-1 boundary lines and
# populated places (scalerank ≤ 6 or a capital: name, min_zoom as `z`,
# scalerank as `r`). Simplified, dissolved where the features don't matter,
# and quantized TopoJSON: about 2 MB, 0.6 MB gzipped.
set -euo pipefail
work="${1:?usage: $0 <empty work dir>}"
out="$(cd "$(dirname "$0")/../.." && pwd)/web/public/basemap/natural-earth.topo.json"
base=https://raw.githubusercontent.com/nvkelso/natural-earth-vector/v5.1.2/geojson
mkdir -p "$work" && cd "$work"
for f in ne_50m_admin_0_countries ne_10m_admin_1_states_provinces_lines ne_50m_coastline ne_50m_lakes ne_10m_populated_places_simple; do
  [ -f "$f.geojson" ] || curl -sfL -o "$f.geojson" "$base/$f.geojson"
done
npx --yes mapshaper@0.6.102 \
  -i ne_50m_admin_0_countries.geojson ne_10m_admin_1_states_provinces_lines.geojson ne_50m_coastline.geojson \
     ne_50m_lakes.geojson ne_10m_populated_places_simple.geojson combine-files \
  -rename-layers countries,admin1,coast,lakes,places \
  -filter-fields NAME target=countries -rename-fields name=NAME target=countries \
  -dissolve target=admin1 -dissolve target=coast \
  -filter-fields name target=lakes \
  -filter 'scalerank<=6 || adm0cap==1' target=places -each 'z=min_zoom, r=scalerank' target=places \
  -filter-fields name,z,r target=places \
  -simplify 25% keep-shapes target=countries -simplify 6% target=admin1 -simplify 25% target=coast \
  -simplify 30% keep-shapes target=lakes \
  -o "$out" format=topojson quantization=100000 'target=*'
