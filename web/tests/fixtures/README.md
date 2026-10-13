# Browser map fixtures

`natural-earth-land.pbf.gz` is the single `z0/x0/y0` tile generated from
`natural-earth-land.geojson`. Its layer must remain named `ne_land` because the
Gallery Natural Earth style selects that source layer.

Regenerate it from the `web` directory with GDAL and SQLite:

```bash
fixture_dir=$(mktemp -d)
ogr2ogr -f MBTILES "$fixture_dir/natural-earth-land.mbtiles" \
  tests/fixtures/natural-earth-land.geojson \
  -nln ne_land \
  -dsco MINZOOM=0 \
  -dsco MAXZOOM=0
sqlite3 "$fixture_dir/natural-earth-land.mbtiles" \
  "select writefile('tests/fixtures/natural-earth-land.pbf.gz', tile_data) from tiles where zoom_level = 0 and tile_column = 0 and tile_row = 0;"
```
