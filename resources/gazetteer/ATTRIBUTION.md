# Gazetteer data

`cities.tsv` (name, country, latitude, longitude) is derived from the
GeoNames `cities15000` dump (all populated places above ~15,000 people),
joined with `countryInfo.txt` for country names.

Source: https://download.geonames.org/export/dump/
License: Creative Commons Attribution 4.0 (CC BY 4.0), © GeoNames.
Regenerate with `scripts/make-gazetteer.sh`.
