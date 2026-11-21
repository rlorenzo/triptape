//! Optional on-device models run through ONNX Runtime (CPU execution
//! provider only).
//!
//! Nothing here is bundled or downloaded by the app or the build:
//! `scripts/fetch-models` installs the pinned files (see
//! `THIRD_PARTY_MODELS.md`) into a models directory, and [`LocalModels::load`]
//! picks up whatever is there. Anything missing (the runtime library or a
//! model file) leaves that capability off and yields one note saying what is
//! missing and how to get it; callers then behave exactly as without models.
//!
//! Lookup order for the models directory: an explicit path (CLI `--models`),
//! then `TRIPTAPE_MODELS_DIR`, then the caller's default (the app data dir's
//! `models/`). The runtime library is `TRIPTAPE_ORT_LIB` when set, else
//! `<models dir>/onnxruntime/<platform library name>`.
//!
//! Privacy: inputs are decoded thumbnails, outputs are plain numbers handed
//! to `triptape-core`. Nothing is logged per photo.

use image::{imageops::FilterType, DynamicImage, RgbImage};
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError};
use triptape_core::embedding::Embedding;
use triptape_core::types::{Analysis, Asset, AssetKind, FaceSignals};
use triptape_core::{hash, proxies, score};

use crate::faces;
use crate::{EmbedResult, Embedder, ModelStatus};

/// Env var naming the models directory.
pub const MODELS_DIR_ENV: &str = "TRIPTAPE_MODELS_DIR";
/// Env var naming the ONNX Runtime shared library (absolute path).
pub const ORT_LIB_ENV: &str = "TRIPTAPE_ORT_LIB";
/// DINOv2-small int8 (`onnx-community/dinov2-small@8b1f705a3a`).
pub const DINO_FILE: &str = "dinov2-small-int8.onnx";
/// YuNet face detector (opencv_zoo `48b10fa`).
pub const YUNET_FILE: &str = "face_detection_yunet_2026may.onnx";
/// FER+ expression classifier (onnx/models `c5cae0f`).
pub const FERPLUS_FILE: &str = "emotion-ferplus-8.onnx";
/// How to get the files, for the "missing" notes.
const FETCH_HINT: &str = "run scripts/fetch-models";

/// Platform file name of the ONNX Runtime shared library.
pub fn ort_library_name() -> &'static str {
    if cfg!(target_os = "windows") {
        "onnxruntime.dll"
    } else if cfg!(target_os = "macos") {
        "libonnxruntime.dylib"
    } else {
        "libonnxruntime.so"
    }
}

/// Where the runtime and the model files are looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelPaths {
    pub dir: PathBuf,
    pub runtime: PathBuf,
}

impl ModelPaths {
    /// Resolve the lookup order in the module docs. `explicit` is a CLI
    /// flag, `default_dir` the caller's default; `env` reads variables
    /// (injected so tests need not touch the process environment).
    pub fn resolve(
        explicit: Option<&Path>,
        default_dir: Option<PathBuf>,
        env: impl Fn(&str) -> Option<String>,
    ) -> Option<Self> {
        let dir = explicit
            .map(Path::to_path_buf)
            .or_else(|| {
                env(MODELS_DIR_ENV)
                    .filter(|v| !v.is_empty())
                    .map(PathBuf::from)
            })
            .or(default_dir)?;
        let runtime = env(ORT_LIB_ENV)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| dir.join("onnxruntime").join(ort_library_name()));
        Some(Self { dir, runtime })
    }

    /// [`ModelPaths::resolve`] with the process environment.
    pub fn from_env(explicit: Option<&Path>, default_dir: Option<PathBuf>) -> Option<Self> {
        Self::resolve(explicit, default_dir, |k| std::env::var(k).ok())
    }

    /// Which capabilities have their files installed: a cheap check that
    /// loads nothing, so a reopened project knows whether analysis it lacks
    /// could run at all.
    pub fn installed(&self) -> Capabilities {
        let has = |f: &str| self.dir.join(f).is_file();
        let runtime = self.runtime.is_file();
        Capabilities {
            embedding: runtime && has(DINO_FILE),
            faces: runtime && has(YUNET_FILE) && has(FERPLUS_FILE),
        }
    }
}

/// Which optional model outputs are available (installed or loaded).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities {
    pub embedding: bool,
    pub faces: bool,
}

impl Capabilities {
    /// Whether `a` (a still or Live Photo still) still lacks an output
    /// these capabilities would add: rescans skip assets that have it all.
    pub fn owe(self, a: &Asset) -> bool {
        a.kind != AssetKind::Video
            && ((self.embedding && a.embedding.is_none()) || (self.faces && a.faces.is_none()))
    }
}

/// The process-wide ONNX Runtime environment. The library can be loaded
/// only once per process, so the first outcome sticks.
static ORT_ENV: OnceLock<Result<ort::environment::Environment, String>> = OnceLock::new();

fn ort_env(lib: &Path) -> Result<&'static ort::environment::Environment, String> {
    ORT_ENV
        .get_or_init(|| {
            if !lib.is_file() {
                return Err(format!(
                    "ONNX Runtime library not found at {}",
                    lib.display()
                ));
            }
            ort::init_from(lib)
                .map_err(|e| format!("ONNX Runtime at {} failed to load: {e}", lib.display()))?
                .with_name("triptape")
                .build()
                .map_err(|e| format!("ONNX Runtime failed to start: {e}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// Most sessions kept per model. Each DINOv2 session costs about 290 MB of
/// peak RSS (measured: 4 sessions took a scan from 0.68 to 1.85 GB, over the
/// 1.5 GB core-tier ceiling), so workers beyond this share sessions.
pub const MAX_SESSIONS: usize = 2;

/// A few sessions of one model, so parallel scoring workers do not queue
/// on a single one (`Session::run` needs `&mut`). Each session runs
/// single-threaded: the spike measured 2 intra-op threads barely beating 1,
/// so parallelism comes from the workers.
struct SessionPool {
    sessions: Vec<Mutex<Session>>,
    next: AtomicUsize,
    /// Runs and summed wall time inside them (for per-photo cost reports).
    runs: AtomicU64,
    nanos: AtomicU64,
}

impl SessionPool {
    fn load(env: &ort::environment::Environment, file: &Path, n: usize) -> Result<Self, String> {
        let one = || -> Result<Session, String> {
            Session::builder(env)
                .and_then(|b| b.with_intra_threads(1).map_err(Into::into))
                .and_then(|b| b.with_inter_threads(1).map_err(Into::into))
                .and_then(|mut b| b.commit_from_file(file))
                .map_err(|e| format!("{}: {e}", file.display()))
        };
        let sessions = (0..n.clamp(1, MAX_SESSIONS))
            .map(|_| one().map(Mutex::new))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            sessions,
            next: AtomicUsize::new(0),
            runs: AtomicU64::new(0),
            nanos: AtomicU64::new(0),
        })
    }

    /// (runs, mean milliseconds per run) so far.
    fn timing(&self) -> (u64, f64) {
        let runs = self.runs.load(Ordering::Relaxed);
        let nanos = self.nanos.load(Ordering::Relaxed);
        (
            runs,
            if runs == 0 {
                0.0
            } else {
                nanos as f64 / runs as f64 / 1e6
            },
        )
    }

    fn timed<T>(&self, s: &mut Session, f: impl FnOnce(&mut Session) -> T) -> T {
        let t = std::time::Instant::now();
        let out = f(s);
        self.runs.fetch_add(1, Ordering::Relaxed);
        self.nanos
            .fetch_add(t.elapsed().as_nanos() as u64, Ordering::Relaxed);
        out
    }

    /// Run `f` on a free session (or wait for the next one in turn).
    fn with<T>(&self, f: impl FnOnce(&mut Session) -> T) -> T {
        for s in &self.sessions {
            if let Ok(mut s) = s.try_lock() {
                return self.timed(&mut s, f);
            }
        }
        let i = self.next.fetch_add(1, Ordering::Relaxed) % self.sessions.len();
        let mut s = self.sessions[i]
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.timed(&mut s, f)
    }
}

/// One model output: (shape, data).
type Output = (Vec<i64>, Vec<f32>);

/// Run a single-input model on an `f32` NCHW tensor; every output as
/// (shape, data).
fn run_f32(s: &mut Session, shape: [i64; 4], data: Vec<f32>) -> Result<Vec<Output>, String> {
    let name = s.inputs()[0].name().to_string();
    let t = Tensor::from_array((shape, data)).map_err(|e| e.to_string())?;
    let out = s
        .run(vec![(
            std::borrow::Cow::Owned(name),
            ort::session::SessionInputValue::from(t),
        )])
        .map_err(|e| e.to_string())?;
    let mut v = Vec::new();
    for (_, o) in out.iter() {
        let (sh, d) = o.try_extract_tensor::<f32>().map_err(|e| e.to_string())?;
        v.push((sh.iter().copied().collect(), d.to_vec()));
    }
    Ok(v)
}

// -- DINOv2 embedding -------------------------------------------------------

/// ImageNet mean / std, the DINOv2 preprocessing.
const IMAGENET: ([f32; 3], [f32; 3]) = ([0.485, 0.456, 0.406], [0.229, 0.224, 0.225]);
const DINO_SIDE: u32 = 224;
/// Shortest side before the centre crop (the 256 -> 224 ImageNet recipe the
/// spike measured with).
const DINO_SHORT: u32 = 256;

/// Resize the shortest side to [`DINO_SHORT`], centre-crop 224x224,
/// normalise, NCHW.
pub fn dino_input(img: &RgbImage) -> Vec<f32> {
    let (w, h) = (img.width().max(1) as f32, img.height().max(1) as f32);
    let s = DINO_SHORT as f32 / w.min(h);
    let nw = ((w * s).round() as u32).max(DINO_SIDE);
    let nh = ((h * s).round() as u32).max(DINO_SIDE);
    let r = image::imageops::resize(img, nw, nh, FilterType::CatmullRom);
    let crop = image::imageops::crop_imm(
        &r,
        (nw - DINO_SIDE) / 2,
        (nh - DINO_SIDE) / 2,
        DINO_SIDE,
        DINO_SIDE,
    )
    .to_image();
    let plane = (DINO_SIDE * DINO_SIDE) as usize;
    let mut d = vec![0f32; 3 * plane];
    for (x, y, p) in crop.enumerate_pixels() {
        let i = (y * DINO_SIDE + x) as usize;
        for c in 0..3 {
            d[c * plane + i] = (f32::from(p[c]) / 255.0 - IMAGENET.0[c]) / IMAGENET.1[c];
        }
    }
    d
}

/// Pool DINOv2's `last_hidden_state` (`[1, tokens, dim]`, CLS first) into
/// one vector: CLS plus the mean patch token, L2-normalised (the spike's
/// pooling, which its thresholds were measured with). `None` on a shape it
/// does not expect.
pub fn dino_pool(shape: &[i64], data: &[f32]) -> Option<Vec<f32>> {
    let [1, tokens, dim] = *shape else {
        return None;
    };
    let (tokens, dim) = (usize::try_from(tokens).ok()?, usize::try_from(dim).ok()?);
    if tokens < 2 || dim == 0 || data.len() != tokens * dim {
        return None;
    }
    let mut e = data[..dim].to_vec();
    let patches = (tokens - 1) as f32;
    for t in 1..tokens {
        for (k, v) in e.iter_mut().enumerate() {
            *v += data[t * dim + k] / patches;
        }
    }
    let n = e.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !(n.is_finite() && n > 0.0) {
        return None;
    }
    e.iter_mut().for_each(|x| *x /= n);
    Some(e)
}

/// DINOv2-small image embedder (the [`Embedder`] seam).
pub struct DinoEmbedder {
    pool: SessionPool,
}

impl DinoEmbedder {
    fn embed_rgb(&self, img: &RgbImage) -> Result<Vec<f32>, String> {
        let input = dino_input(img);
        let side = i64::from(DINO_SIDE);
        let out = self.pool.with(|s| run_f32(s, [1, 3, side, side], input))?;
        let (shape, data) = out.first().ok_or("no output")?;
        dino_pool(shape, data).ok_or_else(|| format!("unexpected output shape {shape:?}"))
    }
}

impl Embedder for DinoEmbedder {
    fn embed(&self, rgb: &[u8], width: u32, height: u32) -> EmbedResult {
        let embedding = RgbImage::from_raw(width, height, rgb.to_vec())
            .and_then(|img| self.embed_rgb(&img).ok());
        EmbedResult {
            embedding,
            status: ModelStatus::Ready,
        }
    }
}

// -- The loaded set -----------------------------------------------------------

/// What the optional models derive from one photo, as plain data for core.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PhotoAnalysis {
    pub embedding: Option<Embedding>,
    /// `None` when the face models are not loaded.
    pub faces: Option<FaceSignals>,
}

/// YuNet face detector plus FER+ expression, run together.
struct FaceModels {
    detector: SessionPool,
    expression: SessionPool,
}

impl FaceModels {
    /// Every face YuNet finds in `img`, in its pixels.
    fn detect(&self, img: &RgbImage) -> Result<Vec<faces::Face>, String> {
        let (input, shape, scale) = faces::yunet_input(img);
        let out = self.detector.with(|s| run_f32(s, shape, input))?;
        let data: Vec<Vec<f32>> = out.into_iter().map(|(_, d)| d).collect();
        let (h, w) = (shape[2] as u32, shape[3] as u32);
        faces::yunet_decode(&data, w, h, scale).ok_or_else(|| "unexpected YuNet outputs".into())
    }

    fn analyze(&self, img: &RgbImage) -> Result<FaceSignals, String> {
        let found = self.detect(img)?;
        let happy: Vec<Option<f32>> = faces::smile_gated(&found)
            .map(|f| {
                let input = faces::ferplus_input(img, f);
                self.expression
                    .with(|s| run_f32(s, [1, 1, 64, 64], input))
                    .ok()
                    .and_then(|o| o.first().and_then(|(_, d)| faces::ferplus_happiness(d)))
            })
            .collect();
        Ok(faces::fold_signals(
            &found,
            &happy,
            img.width(),
            img.height(),
        ))
    }

    /// Plausible faces in `img`, as boxes normalised to its size.
    fn boxes(&self, img: &RgbImage) -> Result<Vec<crate::FaceBox>, String> {
        let found = self.detect(img)?;
        let (iw, ih) = (img.width().max(1) as f32, img.height().max(1) as f32);
        Ok(found
            .iter()
            .filter(|f| f.side() >= faces::MIN_FACE_PX)
            .map(|f| crate::FaceBox {
                x: (f.x1 / iw).clamp(0.0, 1.0),
                y: (f.y1 / ih).clamp(0.0, 1.0),
                w: ((f.x2 - f.x1) / iw).clamp(0.0, 1.0),
                h: ((f.y2 - f.y1) / ih).clamp(0.0, 1.0),
            })
            .collect())
    }
}

/// The optional local models that loaded. Every capability is `None` when
/// its files are missing, in which case callers keep their old behaviour.
#[derive(Default)]
pub struct LocalModels {
    embedder: Option<DinoEmbedder>,
    faces: Option<FaceModels>,
    notes: Vec<String>,
}

impl LocalModels {
    /// No models (the default without a models directory).
    pub fn none() -> Self {
        Self::default()
    }

    /// Load whatever is installed under `paths`, with `sessions` sessions
    /// per model (the number of parallel scoring workers, capped at
    /// [`MAX_SESSIONS`]). Never fails: missing pieces become
    /// [`LocalModels::notes`], one line each.
    pub fn load(paths: Option<&ModelPaths>, sessions: usize) -> Self {
        const DUPES_OFF: &str = "near-duplicates use pHash only";
        const FACES_OFF: &str = "no face or smile signals";
        let Some(paths) = paths else {
            return Self {
                notes: vec![format!(
                    "local models: no models directory; {DUPES_OFF}, {FACES_OFF} ({FETCH_HINT})"
                )],
                ..Self::default()
            };
        };
        let mut out = Self::default();
        let missing: Vec<&str> = [
            (DINO_FILE, DUPES_OFF),
            (YUNET_FILE, FACES_OFF),
            (FERPLUS_FILE, FACES_OFF),
        ]
        .into_iter()
        .filter(|(f, _)| !paths.dir.join(f).is_file())
        .map(|(f, off)| {
            out.notes.push(format!(
                "local models: {f} not found in {}; {off} ({FETCH_HINT})",
                paths.dir.display()
            ));
            f
        })
        .collect();
        if missing.len() == 3 {
            return out;
        }
        let env = match ort_env(&paths.runtime) {
            Ok(env) => env,
            Err(e) => {
                out.notes.push(format!(
                    "local models: {e}; {DUPES_OFF}, {FACES_OFF} ({FETCH_HINT}, or set {ORT_LIB_ENV})"
                ));
                return out;
            }
        };
        let pool = |file: &str, out: &mut Self, off: &str| {
            if missing.contains(&file) {
                return None;
            }
            SessionPool::load(env, &paths.dir.join(file), sessions)
                .map_err(|e| {
                    out.notes
                        .push(format!("local models: {file} failed to load ({e}); {off}"))
                })
                .ok()
        };
        let embedder = pool(DINO_FILE, &mut out, DUPES_OFF);
        out.embedder = embedder.map(|pool| DinoEmbedder { pool });
        let detector = pool(YUNET_FILE, &mut out, FACES_OFF);
        let expression = pool(FERPLUS_FILE, &mut out, FACES_OFF);
        if let (Some(detector), Some(expression)) = (detector, expression) {
            out.faces = Some(FaceModels {
                detector,
                expression,
            });
        }
        out
    }

    /// One line per missing or failed piece, for the CLI / app log.
    pub fn notes(&self) -> &[String] {
        &self.notes
    }

    /// Per loaded model: (name, runs, mean ms per run, preprocessing
    /// excluded). For the CLI's cost line.
    pub fn timings(&self) -> Vec<(&'static str, u64, f64)> {
        let mut out = vec![];
        if let Some(e) = &self.embedder {
            let (n, ms) = e.pool.timing();
            out.push(("dinov2-small", n, ms));
        }
        if let Some(f) = &self.faces {
            let (n, ms) = f.detector.timing();
            out.push(("yunet", n, ms));
            let (n, ms) = f.expression.timing();
            out.push(("ferplus (per face)", n, ms));
        }
        out
    }

    /// Whether any model loaded (callers skip resizing work otherwise).
    pub fn any(&self) -> bool {
        self.embedder.is_some() || self.faces.is_some()
    }

    /// What loaded (a subset of what [`ModelPaths::installed`] found).
    pub fn capabilities(&self) -> Capabilities {
        Capabilities {
            embedding: self.embedder.is_some(),
            faces: self.faces.is_some(),
        }
    }

    /// Whether `a` still lacks something the loaded models would add
    /// ([`Capabilities::owe`]).
    pub fn wants(&self, a: &Asset) -> bool {
        self.capabilities().owe(a)
    }

    /// Faces in a decoded, upright photo (any size; the detector shrinks
    /// it), as normalised boxes. `None` when the face models are not
    /// loaded or the detector failed.
    pub fn face_boxes(&self, img: &RgbImage) -> Option<Vec<crate::FaceBox>> {
        self.faces.as_ref()?.boxes(img).ok()
    }

    /// Run the loaded models on a decoded, upright thumbnail.
    pub fn analyze(&self, img: &DynamicImage) -> PhotoAnalysis {
        if !self.any() {
            return PhotoAnalysis::default();
        }
        let rgb = img.to_rgb8();
        PhotoAnalysis {
            embedding: self
                .embedder
                .as_ref()
                .and_then(|e| e.embed_rgb(&rgb).ok())
                .and_then(|v| Embedding::from_f32(&v)),
            faces: self.faces.as_ref().and_then(|f| f.analyze(&rgb).ok()),
        }
    }

    /// Score one still and fingerprint it (pHash from the same `pixels`),
    /// plus whatever the loaded models add. The models see a small upright
    /// copy, never the full-resolution frame: cached thumbnails already
    /// are; pass `original` when `pixels` is the decoded source file, which
    /// is then shrunk to thumbnail size and turned by its EXIF rotation.
    /// `None` pixels get the neutral fallback score. Pure per-file work.
    pub fn analyze_still(
        &self,
        a: &Asset,
        pixels: Option<&DynamicImage>,
        original: bool,
    ) -> Analysis {
        let found = match pixels {
            Some(p) if original && self.any() => {
                let small = p.thumbnail(proxies::THUMB_LONG_EDGE, proxies::THUMB_LONG_EDGE);
                self.analyze(&match a.rotation_deg {
                    90 => small.rotate90(),
                    180 => small.rotate180(),
                    270 => small.rotate270(),
                    _ => small,
                })
            }
            Some(p) => self.analyze(p),
            None => PhotoAnalysis::default(),
        };
        let signals = score::StorySignals::for_asset(a).with_faces(found.faces);
        Analysis {
            score: score::score_still_or_fallback(pixels, &signals),
            phash: pixels.map(hash::average_hash_from_image),
            embedding: found.embedding,
            faces: found.faces,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_resolve_flag_then_env_then_default() {
        let env = |k: &str| match k {
            MODELS_DIR_ENV => Some("/env/models".to_string()),
            _ => None,
        };
        let none = |_: &str| None;
        let flag = ModelPaths::resolve(Some(Path::new("/flag")), Some("/def".into()), env).unwrap();
        assert_eq!(flag.dir, PathBuf::from("/flag"));
        assert_eq!(
            flag.runtime,
            PathBuf::from("/flag/onnxruntime").join(ort_library_name())
        );
        let from_env = ModelPaths::resolve(None, Some("/def".into()), env).unwrap();
        assert_eq!(from_env.dir, PathBuf::from("/env/models"));
        let default = ModelPaths::resolve(None, Some("/def".into()), none).unwrap();
        assert_eq!(default.dir, PathBuf::from("/def"));
        assert_eq!(ModelPaths::resolve(None, None, none), None);
        let lib = |k: &str| (k == ORT_LIB_ENV).then(|| "/opt/ort/lib.so".to_string());
        assert_eq!(
            ModelPaths::resolve(None, Some("/def".into()), lib)
                .unwrap()
                .runtime,
            PathBuf::from("/opt/ort/lib.so")
        );
    }

    #[test]
    fn missing_models_leave_everything_off_with_one_note() {
        let dir = std::env::temp_dir().join("triptape-no-models-here");
        let paths = ModelPaths {
            runtime: dir.join("nope"),
            dir,
        };
        let m = LocalModels::load(Some(&paths), 2);
        assert!(!m.any());
        assert_eq!(m.notes().len(), 3, "one line per missing file");
        assert!(m.notes()[0].contains(DINO_FILE), "{:?}", m.notes());
        assert!(m.notes()[1].contains(YUNET_FILE), "{:?}", m.notes());
        assert!(m.notes()[0].contains("fetch-models"));
        let img = DynamicImage::new_rgb8(8, 8);
        assert_eq!(m.analyze(&img), PhotoAnalysis::default());
        let a = Asset::new("a", "/t/a.jpg".into(), AssetKind::Photo);
        assert!(!m.wants(&a), "nothing to add without models");
        assert_eq!(LocalModels::load(None, 1).notes().len(), 1);
    }

    #[test]
    fn capabilities_owe_only_what_is_installed_and_missing() {
        let mut a = Asset::new("a", "/t/a.jpg".into(), AssetKind::Photo);
        let embed_only = Capabilities {
            embedding: true,
            faces: false,
        };
        assert!(embed_only.owe(&a));
        a.embedding = Embedding::from_f32(&[1.0, 0.0]);
        // Faces missing, but their models are not installed: nothing owed.
        assert!(!embed_only.owe(&a));
        let both = Capabilities {
            embedding: true,
            faces: true,
        };
        assert!(both.owe(&a));
        a.faces = Some(FaceSignals::default());
        assert!(!both.owe(&a));
        let video = Asset::new("v", "/t/v.mov".into(), AssetKind::Video);
        assert!(!both.owe(&video), "videos get no model output");
        assert!(!Capabilities::default().owe(&video));
    }

    #[test]
    fn installed_needs_the_runtime_and_each_capability_s_files() {
        let dir = std::env::temp_dir().join(format!("triptape-installed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ModelPaths {
            runtime: dir.join("libort"),
            dir: dir.clone(),
        };
        let touch = |f: &str| std::fs::write(dir.join(f), b"").unwrap();
        touch(DINO_FILE);
        touch(YUNET_FILE);
        assert_eq!(paths.installed(), Capabilities::default(), "no runtime");
        touch("libort");
        assert_eq!(
            paths.installed(),
            Capabilities {
                embedding: true,
                faces: false
            },
            "faces need both YuNet and FER+"
        );
        touch(FERPLUS_FILE);
        assert_eq!(
            paths.installed(),
            Capabilities {
                embedding: true,
                faces: true
            }
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn analyze_still_without_models_scores_and_hashes_the_pixels() {
        let m = LocalModels::none();
        let a = Asset::new("a", "/t/a.jpg".into(), AssetKind::Photo);
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(64, 48, |x, y| {
            image::Rgb([(x * 4) as u8, (y * 5) as u8, 90])
        }));
        let got = m.analyze_still(&a, Some(&img), true);
        assert_eq!(got.phash, Some(hash::average_hash_from_image(&img)));
        assert_eq!((got.embedding, got.faces), (None, None));
        let none = m.analyze_still(&a, None, false);
        assert_eq!(none.phash, None);
        assert_eq!(none.score.total, score::fallback_score().total);
    }

    #[test]
    fn dino_input_is_normalised_nchw_224() {
        let img = RgbImage::from_pixel(320, 240, image::Rgb([124, 116, 104]));
        let d = dino_input(&img);
        assert_eq!(d.len(), 3 * 224 * 224);
        // ImageNet-mean grey normalises to about zero in every channel.
        assert!(
            d.iter().all(|v| v.abs() < 0.02),
            "max {:?}",
            d.iter().fold(0f32, |m, v| m.max(v.abs()))
        );
    }

    #[test]
    fn dino_pool_adds_cls_to_mean_patch_and_normalises() {
        // 1 CLS + 2 patches, dim 2.
        let data = [1.0, 0.0, 0.0, 2.0, 0.0, 0.0];
        let e = dino_pool(&[1, 3, 2], &data).unwrap();
        // CLS (1,0) + mean patch (0,1) = (1,1) -> unit.
        assert!((e[0] - e[1]).abs() < 1e-6 && (e[0] - 0.70710677).abs() < 1e-6);
        assert_eq!(dino_pool(&[1, 1, 2], &[1.0, 0.0]), None, "no patches");
        assert_eq!(dino_pool(&[3, 2], &data), None, "wrong rank");
        assert_eq!(dino_pool(&[1, 3, 2], &data[..4]), None, "short data");
        assert_eq!(dino_pool(&[1, 3, 2], &[0.0; 6]), None, "zero vector");
    }

    /// Needs the real files: `TRIPTAPE_MODELS_DIR` (and `TRIPTAPE_ORT_LIB`
    /// unless the runtime is installed beside the models).
    #[test]
    #[ignore = "needs scripts/fetch-models and an ONNX Runtime library"]
    fn real_dino_embeds_a_picture_and_itself_alike() {
        let paths = ModelPaths::from_env(None, None).expect("set TRIPTAPE_MODELS_DIR");
        let m = LocalModels::load(Some(&paths), 1);
        assert!(m.any(), "{:?}", m.notes());
        let img = DynamicImage::ImageRgb8(RgbImage::from_fn(320, 240, |x, y| {
            image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8])
        }));
        let a = m.analyze(&img).embedding.unwrap();
        let b = m.analyze(&img.fliph()).embedding.unwrap();
        assert_eq!(a.len(), 384);
        assert!(a.cosine(&a).unwrap() > 0.999);
        assert!(
            a.cosine(&b).unwrap() > 0.5,
            "a mirrored picture is the same scene"
        );
        // A synthetic pattern has no face, stored as zero faces.
        let faces = m.analyze(&img).faces.expect("face models loaded");
        assert_eq!(faces.count, 0);
        assert_eq!(faces.smile, None);
    }
}
