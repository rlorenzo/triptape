//! Stage 2 — Fingerprint: BLAKE3 exact hash + simple perceptual (average) hash.

use std::path::{Path, PathBuf};

/// File-level parallelism for the ingest background task: at most this many
/// files are read concurrently.
pub const INGEST_HASH_WORKERS: usize = 4;

/// Stream a file through BLAKE3 and return the lowercase hex digest.
pub fn blake3_file(path: &Path) -> Result<String, String> {
    blake3_file_chunked(path, |_| {})
}

/// Read chunk size for [`blake3_file_chunked`]: 1 MiB per callback tick.
pub const HASH_CHUNK_SIZE: usize = 1024 * 1024;

/// Stream a file through BLAKE3 in fixed-size chunks, invoking `on_bytes`
/// with the cumulative bytes read so far after each chunk. The digest is
/// identical to [`blake3_file`]; the callback only exists so callers can
/// report honest sub-file progress for large files.
pub fn blake3_file_chunked(path: &Path, mut on_bytes: impl FnMut(u64)) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; HASH_CHUNK_SIZE];
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
        on_bytes(total);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// Hash several files with bounded file-level parallelism, reusing
/// [`blake3_file_chunked`] per file so digests match the sequential path.
///
/// `workers` is clamped to `1..=paths.len()`; each worker thread reads one
/// file at a time, so at most `workers` files are ever read concurrently.
/// Results align with the input order regardless of completion order, and a
/// missing or unreadable file yields `None` in its slot instead of failing
/// the batch. `on_file` runs on the calling (coordinator) thread only — one
/// call per completed file as `(input index, digest, files done, total)` —
/// so callers can persist and report progress without extra locking.
pub fn blake3_files_parallel(
    paths: &[PathBuf],
    workers: usize,
    mut on_file: impl FnMut(usize, Option<String>, usize, usize),
) -> Vec<Option<String>> {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let total = paths.len();
    let mut results: Vec<Option<String>> = vec![None; total];
    if total == 0 {
        return results;
    }
    let live = workers.max(1).min(total);
    let next = AtomicUsize::new(0);
    let (tx, rx) = std::sync::mpsc::channel::<(usize, Option<String>)>();
    std::thread::scope(|s| {
        // Shared borrows stay borrows under the `move` closures below, so
        // every worker sees the same index counter and input slice.
        let next_ref = &next;
        let paths_ref = paths;
        for _ in 0..live {
            let worker_tx = tx.clone();
            s.spawn(move || loop {
                let idx = next_ref.fetch_add(1, Ordering::SeqCst);
                if idx >= total {
                    break;
                }
                let digest = blake3_file_chunked(&paths_ref[idx], |_| {}).ok();
                if worker_tx.send((idx, digest)).is_err() {
                    break;
                }
            });
        }
        drop(tx);
        let mut done = 0usize;
        for (idx, digest) in rx {
            done += 1;
            on_file(idx, digest.clone(), done, total);
            results[idx] = digest;
        }
    });
    results
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
    fn chunked_matches_whole_file_over_chunk_size() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("big.bin");
        // Larger than one chunk so the callback fires multiple times.
        let bytes: Vec<u8> = (0..(HASH_CHUNK_SIZE * 2 + 12345))
            .map(|i| (i % 251) as u8)
            .collect();
        std::fs::write(&p, &bytes).unwrap();
        let mut seen: Vec<u64> = vec![];
        let chunked = blake3_file_chunked(&p, |n| seen.push(n)).unwrap();
        assert_eq!(chunked, blake3::hash(&bytes).to_hex().as_str());
        assert_eq!(chunked, blake3_file(&p).unwrap());
        assert!(seen.len() >= 3, "expected several chunk ticks, got {seen:?}");
        assert_eq!(*seen.last().unwrap(), bytes.len() as u64);
        let mut last = 0u64;
        for n in seen {
            assert!(n > last, "callback bytes must grow monotonically");
            last = n;
        }
    }

    #[test]
    fn chunked_empty_file_matches_and_never_stalls() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.bin");
        std::fs::write(&p, b"").unwrap();
        let mut calls = 0u32;
        let digest = blake3_file_chunked(&p, |_| calls += 1).unwrap();
        assert_eq!(digest, blake3_file(&p).unwrap());
        assert_eq!(digest, blake3::hash(b"").to_hex().as_str());
        assert_eq!(calls, 0, "empty file has no chunks to report");
    }

    #[test]
    fn parallel_matches_sequential_and_preserves_input_order() {
        let dir = tempfile::tempdir().unwrap();
        // Varied sizes so fast small files finish before slow big ones while
        // slots must still line up with the input order.
        let contents: Vec<Vec<u8>> = (0..8)
            .map(|i| vec![i as u8 + 1; 500 + i * 170_000])
            .collect();
        let paths: Vec<std::path::PathBuf> = contents
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let p = dir.path().join(format!("f{i:02}.bin"));
                std::fs::write(&p, c).unwrap();
                p
            })
            .collect();
        let expected: Vec<String> = contents
            .iter()
            .map(|c| blake3::hash(c).to_hex().to_string())
            .collect();
        let mut seen_done: Vec<usize> = vec![];
        let got = blake3_files_parallel(&paths, 4, |_, _, done, total| {
            assert_eq!(total, 8);
            seen_done.push(done);
        });
        let digests: Vec<String> = got.into_iter().map(|d| d.unwrap()).collect();
        assert_eq!(digests, expected, "slots must align with input order");
        assert_eq!(seen_done, vec![1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    fn parallel_missing_file_yields_none_in_its_slot() {
        let dir = tempfile::tempdir().unwrap();
        let ok = dir.path().join("ok.bin");
        std::fs::write(&ok, b"present").unwrap();
        let missing = dir.path().join("gone.bin");
        let paths = vec![ok.clone(), missing, ok.clone()];
        let mut calls = 0usize;
        let got = blake3_files_parallel(&paths, 2, |_, _, _, _| calls += 1);
        assert_eq!(calls, 3);
        let want = blake3_file(&ok).unwrap();
        assert_eq!(got[0].as_deref(), Some(want.as_str()));
        assert_eq!(got[1], None, "unreadable file must not fail the batch");
        assert_eq!(got[2].as_deref(), Some(want.as_str()));
    }

    #[test]
    fn parallel_empty_input_and_worker_clamping() {
        let mut calls = 0usize;
        assert!(blake3_files_parallel(&[], 4, |_, _, _, _| calls += 1).is_empty());
        assert_eq!(calls, 0, "no files means no callback");
        let dir = tempfile::tempdir().unwrap();
        let paths: Vec<std::path::PathBuf> = (0..3)
            .map(|i| {
                let p = dir.path().join(format!("c{i}.bin"));
                std::fs::write(&p, format!("file-{i}")).unwrap();
                p
            })
            .collect();
        // Zero workers still hashes (clamped to 1); more workers than files
        // is fine (clamped to the file count).
        for workers in [0usize, 1, 99] {
            let got = blake3_files_parallel(&paths, workers, |_, _, _, _| {});
            for (i, d) in got.iter().enumerate() {
                let want = blake3::hash(format!("file-{i}").as_bytes())
                    .to_hex()
                    .to_string();
                assert_eq!(d.as_deref(), Some(want.as_str()), "workers={workers}");
            }
        }
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
