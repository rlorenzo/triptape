//! Stage 2 — Fingerprint: BLAKE3 exact hash + simple perceptual (average) hash.

use std::path::Path;

/// Stream a file through BLAKE3 and return the lowercase hex digest.
pub fn blake3_file(path: &Path) -> Result<String, String> {
    let f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(f).map_err(|e| e.to_string())?;
    Ok(hasher.finalize().to_hex().to_string())
}

/// 64-bit average hash (aHash) of decoded image pixels.
///
/// Decodes with the `image` crate, shrinks to 8x8 luma, and sets one bit
/// per cell above the mean. HEIC files cannot be decoded by `image`;
/// callers must pass an FFmpeg-decoded JPEG proxy for those.
pub fn average_hash_from_image(img: &image::DynamicImage) -> u64 {
    let gray = img.to_luma8();
    let small = image::imageops::resize(&gray, 8, 8, image::imageops::FilterType::Triangle);
    let mean: f32 = small.pixels().map(|p| p[0] as f32).sum::<f32>() / 64.0;
    let mut bits: u64 = 0;
    for (i, p) in small.pixels().enumerate() {
        if p[0] as f32 > mean {
            bits |= 1u64 << i;
        }
    }
    bits
}

/// Hamming distance between two perceptual hashes (0 = identical).
pub fn phash_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Two images are near-duplicates when their distance is at most this.
pub const NEAR_DUPE_MAX_DISTANCE: u32 = 8;

pub fn is_near_dupe(a: u64, b: u64) -> bool {
    phash_distance(a, b) <= NEAR_DUPE_MAX_DISTANCE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(w: u32, h: u32, v: u8) -> image::DynamicImage {
        image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(w, h, image::Luma([v])))
    }

    #[test]
    fn identical_images_hash_equal() {
        let a = average_hash_from_image(&solid(64, 64, 128));
        let b = average_hash_from_image(&solid(64, 64, 128));
        assert_eq!(a, b);
        assert!(is_near_dupe(a, b));
    }

    #[test]
    fn gradient_differs_from_solid() {
        let mut g = image::GrayImage::new(64, 64);
        for (x, _y, p) in g.enumerate_pixels_mut() {
            p[0] = (x * 4) as u8;
        }
        let grad = image::DynamicImage::ImageLuma8(g);
        let flat = solid(64, 64, 128);
        let d = phash_distance(
            average_hash_from_image(&grad),
            average_hash_from_image(&flat),
        );
        assert!(d > 0, "different content must differ");
    }

    #[test]
    fn blake3_known_value() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.bin");
        std::fs::write(&p, b"hello").unwrap();
        let got = blake3_file(&p).unwrap();
        assert_eq!(
            got,
            "ea8f163db38682925e4491c5e58d4bb3506ef8c14eb78a86e908c5624a67200f"
        );
        assert_eq!(got, blake3::hash(b"hello").to_hex().as_str());
    }
}
