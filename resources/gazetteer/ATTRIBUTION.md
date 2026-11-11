# Gazetteer data

`cities.tsv` (name, country, latitude, longitude) is derived from the
GeoNames `cities15000` dump (all populated places above ~15,000 people),
joined with `countryInfo.txt` for country names.

Source: https://download.geonames.org/export/dump/
License: Creative Commons Attribution 4.0 (CC BY 4.0), © GeoNames.
Regenerate with `scripts/make-gazetteer.sh`.

`attractions.tsv` (name, latitude, longitude) is derived
from Wikidata items typed as tourist attraction, amusement park,
urban park, park, museum, botanical garden, zoo, castle, shopping center,
Buddhist temple, Shinto shrine, church building, mosque, Hindu temple, or
synagogue (or a subclass of one of those), with at least 12 Wikidata sitelinks
as a notability floor. Items typed as a destroyed building or structure, or
with a state of use of "demolished or destroyed", are left out (the Asakusa
tower Ryōunkaku, demolished in 1923, no longer names a day at Sensō-ji).
Items that are only tourist attractions and are also typed as a city, town,
village, municipality, or spa town are left out (archaeological sites stay),
so those places keep their `City, Country` name from `cities.tsv`. Umbrella resorts parenting two or more
amusement parks are also dropped, so a park keeps its own name
(`Tokyo DisneySea`, not `Tokyo Disney Resort`). It lets chapter and
caption naming prefer a place a traveler actually visited
(`Tokyo DisneySea`, `Gardens by the Bay`, `Hong Kong Disneyland`)
over the generic surrounding city.

Source: Wikidata via the public QLever endpoint https://qlever.dev/api/wikidata (SPARQL, P625 coordinates)
License: Creative Commons CC0 1.0 Universal (public domain), © Wikidata contributors.
Regenerate with `scripts/make-gazetteer.sh` (see its Wikidata section).

`land-grid.bin` is a 0.05° land/water bitmap (7200 × 3600 bits,
row-major from -90, -180) rasterized from Natural Earth 50m land
polygons. It keeps GPS points on open water (a ship under way) from
being named after the nearest coastal town.

Source: https://github.com/nvkelso/natural-earth-vector (`ne_50m_land.geojson`)
License: public domain (Natural Earth).
Regenerate with `scripts/make-gazetteer.sh` (see its land-grid section).
