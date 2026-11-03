# Code Review Summary

## Overview

The review loop fixed one medium-severity correctness problem: whole towns and cities were being labeled as attractions, which removed the country from their names. It also made gazetteer generation reproducible, extended the truncation checks to the new filter data, and moved the attraction-before-city rule into the shared place-name lookup. The final review found no remaining issues.

## Improvements Made

- **Towns keep their `City, Country` names.** Wikidata's tourist-attraction class includes whole settlements such as Aachen and Brilon, so their photos were labeled with the bare town name. The generator now drops an item reached only through that broad class if it is also typed as a city, town, village, municipality, or spa town. Archaeological sites are exempt, so Persepolis stays. The dataset shrank from 6,122 to 5,773 rows.
- **The new filter fails safely on partial data.** The town and archaeological-site membership queries get the same checks as the class queries: expected CSV header, well-formed item URLs, and a per-class minimum row count. Without these, a response cut off at a row boundary would parse cleanly and silently let towns back in. The total-row floor was reset from 5,509 to 5,195 to match the filtered output.
- **Regeneration is deterministic.** The endpoint's row order varies, and an item can have several coordinates or labels. The script now picks each item's row the same way on every run and sorts output with a full tie-break, so unchanged source data no longer produces spurious diffs.
- **Naming precedence has one home.** `place_name` now applies the attraction-before-city rule itself, rather than leaving it to the chapter-naming code in `plan.rs`. `attraction_name` and the new `city_name` are private, so callers outside `geo.rs` can't bypass the precedence.
- **The embedded data is smaller.** The sitelinks column is gone from `attractions.tsv` because row order already records notability. Lookups keep the first of equidistant items, so the most notable one wins.
- **Grid constants are shared by name.** `geo.rs` now derives its grid dimensions and coastal buffer from `CELLS_PER_DEG` and `COAST_BUFFER_CELLS` instead of magic numbers. The generator uses a matching `RES` constant and drops unneeded row padding and a one-off helper. The padding removal doesn't affect output because crossings are still tested against each row's exact latitude.
- **Tests cover the new behavior.** New cases check attraction-over-city precedence, towns keeping their country (Aachen, Brilon), and items the filter must spare (Persepolis, Kolomenskoye). City-lookup tests now call `city_name` directly, and the high-latitude attraction test now uses Quttinirpaaq National Park at 82.2°N.

## Created Files

None.

## Remaining Items

- Uncommon settlement classifications beyond the five filtered types can still appear as attractions. This was deliberately deferred, and the final review accepted that decision.

## Files Changed

- `resources/gazetteer/ATTRIBUTION.md`
- `resources/gazetteer/attractions.tsv`
- `scripts/make-gazetteer.sh`
- `src-tauri/crates/triptape-core/src/geo.rs`
- `src-tauri/crates/triptape-core/src/plan.rs`

## Metrics

- **Cycles run:** 1
- **Duration:** 12m 58s
- **Initial review issues:** High 0, Medium 1, Low 0
- **Final issues:** High 0, Medium 0, Low 0
- **Final status:** CLEAN
