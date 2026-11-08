//! Resolve a human reference (asset id, file name or path) to an asset.

use crate::types::Asset;
use std::path::Path;

fn file_name_matches(path: &Path, name: &str) -> bool {
    path.file_name()
        .is_some_and(|f| f.to_string_lossy().eq_ignore_ascii_case(name))
}

/// Find the one asset `query` names. Tried in order: exact asset id; a path
/// (anything with a separator: full path, or its trailing components, case
/// sensitive); a file name, case-insensitive, matching the asset's still or
/// video path or its Live Photo companion. Errors clearly on no match or
/// when several assets match (listing their paths).
pub fn resolve_asset<'a>(assets: &'a [Asset], query: &str) -> Result<&'a Asset, String> {
    let query = query.trim();
    if query.is_empty() {
        return Err("empty asset reference".to_string());
    }
    if let Some(a) = assets.iter().find(|a| a.id == query) {
        return Ok(a);
    }
    let q = Path::new(query);
    let matches: Vec<&Asset> = if query.contains('/') || query.contains('\\') {
        let ends = |p: &Path| p == q || (q.is_relative() && p.ends_with(q));
        assets
            .iter()
            .filter(|a| ends(&a.path) || a.companion_path.as_deref().is_some_and(ends))
            .collect()
    } else {
        assets
            .iter()
            .filter(|a| {
                file_name_matches(&a.path, query)
                    || a.companion_path
                        .as_deref()
                        .is_some_and(|c| file_name_matches(c, query))
            })
            .collect()
    };
    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(format!(
            "no asset matches {query:?} (id, file name or path)"
        )),
        many => {
            let paths: Vec<String> = many
                .iter()
                .map(|a| format!("{} ({})", a.id, a.path.display()))
                .collect();
            Err(format!(
                "{query:?} is ambiguous, matches {}: use the asset id or a longer path",
                paths.join(", ")
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::AssetKind;
    use std::path::PathBuf;

    fn assets() -> Vec<Asset> {
        let mut live = Asset::new(
            "a2",
            PathBuf::from("/t/d1/IMG_2.HEIC"),
            AssetKind::LivePhoto,
        );
        live.companion_path = Some(PathBuf::from("/t/d1/IMG_2.MOV"));
        vec![
            Asset::new("a1", PathBuf::from("/t/d1/IMG_1060.JPG"), AssetKind::Photo),
            live,
            Asset::new("a3", PathBuf::from("/t/d1/IMG_1061.MOV"), AssetKind::Video),
            Asset::new("a4", PathBuf::from("/t/d2/IMG_1060.JPG"), AssetKind::Photo),
        ]
    }

    #[test]
    fn resolves_id_name_companion_and_path() {
        let a = assets();
        assert_eq!(resolve_asset(&a, "a3").unwrap().id, "a3");
        assert_eq!(resolve_asset(&a, "img_1061.mov").unwrap().id, "a3");
        assert_eq!(resolve_asset(&a, "IMG_2.mov").unwrap().id, "a2");
        assert_eq!(resolve_asset(&a, "/t/d2/IMG_1060.JPG").unwrap().id, "a4");
        assert_eq!(resolve_asset(&a, "d1/IMG_1060.JPG").unwrap().id, "a1");
    }

    #[test]
    fn errors_on_ambiguity_and_no_match() {
        let a = assets();
        let e = resolve_asset(&a, "img_1060.jpg").unwrap_err();
        assert!(
            e.contains("ambiguous") && e.contains("a1") && e.contains("a4"),
            "{e}"
        );
        assert!(resolve_asset(&a, "nope.jpg")
            .unwrap_err()
            .contains("no asset"));
        assert!(resolve_asset(&a, " ").is_err());
    }
}
