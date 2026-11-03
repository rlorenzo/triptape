# Code Review Loop Summary

## Overview

The review loop made the attraction and open-water naming work more correct and its data pipeline stricter. It fixed a radius bug at high latitudes, cut the embedded attraction data down to its documented notability floor, and made regeneration fail loudly instead of silently writing partial data. It also added tests for edge cases and for chapter naming. One Medium issue remains: the regeneration script cannot currently reproduce the committed attraction file.

## Improvements Made

- **Fixed attraction lookups at high latitudes.** The original lookup rejected candidates more than a fixed 0.05° of longitude away. A degree of longitude shrinks toward the poles, so that filter could drop places well inside the 2 km radius. Attractions now share the city lookup's latitude-aware, antimeridian-safe search, which also removes a duplicate nearest-neighbor loop.
- **Brought the data in line with its stated notability floor.** The original file held 11,147 rows, including items with as few as 5 sitelinks, although the attribution and script both state a floor of 12. The loop removed the 7,892 rows below that floor and added none, leaving 3,255. This shrinks the binary and reduces false matches, but it also drops some recognizable places, such as Zoo Miami and Young V&A.
- **Made attraction regeneration fail loudly.** Failed queries were previously ignored and malformed rows skipped, so an outage or cut-off response could silently overwrite the file with partial data. HTTP errors now abort after retries, and the script validates the header, every row, and the sitelink floor. It also enforces per-class and total row minimums before writing, which catches a response cut off cleanly at a row boundary.
- **Corrected the tourist-attraction class.** The Wikidata query now uses `Q570116` in place of `Q2977832`, and the attraction classes and sitelink floor are named settings passed into the validator.
- **Sped up land-grid generation.** Rasterizing now visits only the rows each polygon edge can span, rather than scanning every edge for every row.
- **Stopped Git treating the land grid as text.** The bitmap is now marked binary, so it no longer adds megabytes of unreadable text to patches or risks text handling of its bytes.
- **Tightened the API and comments.** `is_land` is now private to the geo module, and comments now describe all three datasets and both embedded place-name sources.
- **Expanded test coverage.** New tests cover the high-latitude radius, both poles, and land reachable only across the antimeridian. Chapter naming now has tests for an attraction beating the surrounding city and for open water naming nothing. The original combined test was split by behavior, dropping its Asakusa city-name assertion.

## Created Files

- `.gitattributes`

## Remaining Items

- **[Medium] The attraction dataset cannot be regenerated as documented.** The eight configured classes produce about 2,067 unique attractions according to the recorded query, but the script requires at least 3,000, so a normal run fails before writing `attractions.tsv`. The committed 3,255 rows also include items outside those classes (for example, the headland Nordostrundingen, which a test relies on), so the attribution's description does not match the file. The review recommends widening the query to cover the intended dataset, recording a successful run, and resetting the floors from that result. The current floor should stay until then so regeneration cannot silently remove names.

## Files Changed

- `.gitattributes`
- `resources/gazetteer/attractions.tsv`
- `scripts/make-gazetteer.sh`
- `src-tauri/crates/triptape-core/src/geo.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`

## Metrics

- **Cycles run:** 2 (maximum reached)
- **Duration:** 51m 44s
- **Final status:** Max iterations reached, with one issue remaining
- **Issue counts by review:**

| Review | High | Medium | Low |
|---|---:|---:|---:|
| Initial | 1 | 2 | 0 |
| Cycle 1 | 0 | 1 | 0 |
| Cycle 2 (final) | 0 | 1 | 0 |
