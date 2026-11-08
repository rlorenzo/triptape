//! G4 model-selection spike. Not wired into scoring.
//!
//! `spike bench <model> <images..>`  per-model latency / load time / peak RSS
//! `spike analyze <out_dir> <images..>`  faces, smile, embeddings, neighbours
//!
//! Env: MODEL_DIR (default output/models), ORT_DYLIB_PATH (libonnxruntime),
//! SPIKE_THREADS (default 2), SPIKE_EP=coreml to try the CoreML EP.

use image::{imageops::FilterType, DynamicImage, ImageDecoder, RgbImage};
use ort::session::Session;
use ort::value::Tensor;
use std::path::{Path, PathBuf};
use std::time::Instant;

type R<T> = Result<T, Box<dyn std::error::Error>>;

fn peak_rss_mb() -> f64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    // macOS reports bytes, Linux kilobytes.
    let raw = ru.ru_maxrss as f64;
    if cfg!(target_os = "macos") {
        raw / 1048576.0
    } else {
        raw / 1024.0
    }
}

fn model_dir() -> PathBuf {
    PathBuf::from(std::env::var("MODEL_DIR").unwrap_or_else(|_| "output/models".into()))
}

static ENV: std::sync::OnceLock<ort::environment::Environment> = std::sync::OnceLock::new();

fn env() -> R<&'static ort::environment::Environment> {
    if let Some(e) = ENV.get() {
        return Ok(e);
    }
    let lib =
        std::env::var("ORT_DYLIB_PATH").map_err(|_| "set ORT_DYLIB_PATH to libonnxruntime")?;
    let e = ort::init_from(lib)?.with_name("spike").build()?;
    Ok(ENV.get_or_init(|| e))
}

fn load(name: &str) -> R<Session> {
    let threads: usize = std::env::var("SPIKE_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2);
    let mut b = Session::builder(env()?)?
        .with_intra_threads(threads)?
        .with_inter_threads(1)?;
    #[cfg(feature = "coreml")]
    if std::env::var("SPIKE_EP").as_deref() == Ok("coreml") {
        b = b.with_execution_providers([ort::ep::CoreML::default().build()])?;
    }
    Ok(b.commit_from_file(model_dir().join(name))?)
}

fn load_image(p: &Path, long_edge: u32) -> R<RgbImage> {
    let mut dec = image::ImageReader::open(p)?
        .with_guessed_format()?
        .into_decoder()?;
    let orient = dec.orientation()?;
    let mut img = DynamicImage::from_decoder(dec)?;
    img.apply_orientation(orient);
    let (w, h) = (img.width(), img.height());
    let s = long_edge as f32 / w.max(h) as f32;
    if s < 1.0 {
        img = img.resize_exact(
            ((w as f32 * s) as u32).max(1),
            ((h as f32 * s) as u32).max(1),
            FilterType::Triangle,
        );
    }
    Ok(img.to_rgb8())
}

fn run_f32(
    s: &mut Session,
    input: &str,
    shape: [i64; 4],
    data: Vec<f32>,
) -> R<Vec<(Vec<i64>, Vec<f32>)>> {
    let t = Tensor::from_array((shape, data))?;
    let out = s.run(vec![(
        std::borrow::Cow::Borrowed(input),
        ort::session::SessionInputValue::from(t),
    )])?;
    let mut v = Vec::new();
    for (_, o) in out.iter() {
        let (sh, d) = o.try_extract_tensor::<f32>()?;
        v.push((sh.iter().copied().collect(), d.to_vec()));
    }
    Ok(v)
}

// ---------------------------------------------------------------- YuNet
#[derive(Clone, Debug)]
struct Face {
    x1: f32,
    y1: f32,
    x2: f32,
    y2: f32,
    score: f32,
}

fn yunet(s: &mut Session, img: &RgbImage, thr: f32) -> R<Vec<Face>> {
    // Dynamic-shape model (2026may): long edge 640, both dims padded to /32.
    let (w, h) = (img.width() as f32, img.height() as f32);
    let sc = 640.0 / w.max(h);
    let rw = (w * sc).round() as u32;
    let rh = (h * sc).round() as u32;
    let (pw, ph) = (rw.div_ceil(32) * 32, rh.div_ceil(32) * 32);
    let r = image::imageops::resize(img, rw, rh, FilterType::Triangle);
    let mut data = vec![0f32; (3 * pw * ph) as usize];
    for (x, y, p) in r.enumerate_pixels() {
        let i = (y * pw + x) as usize;
        data[i] = p[2] as f32; // BGR
        data[(pw * ph) as usize + i] = p[1] as f32;
        data[2 * (pw * ph) as usize + i] = p[0] as f32;
    }
    let name = s.inputs()[0].name().to_string();
    let outs = run_f32(s, &name, [1, 3, ph as i64, pw as i64], data)?;
    // order: cls_8,16,32 obj_8,16,32 bbox_8,16,32 kps_8,16,32
    let mut faces = Vec::new();
    for (si, stride) in [8f32, 16.0, 32.0].iter().enumerate() {
        let cls = &outs[si].1;
        let obj = &outs[3 + si].1;
        let bb = &outs[6 + si].1;
        let cols = (pw as f32 / stride) as usize;
        for i in 0..cls.len() {
            let sco = (cls[i].clamp(0.0, 1.0) * obj[i].clamp(0.0, 1.0)).sqrt();
            if sco < thr {
                continue;
            }
            let (r_, c_) = ((i / cols) as f32, (i % cols) as f32);
            let cx = (c_ + bb[i * 4]) * stride;
            let cy = (r_ + bb[i * 4 + 1]) * stride;
            let bw = bb[i * 4 + 2].exp() * stride;
            let bh = bb[i * 4 + 3].exp() * stride;
            faces.push(Face {
                x1: (cx - bw / 2.0) / sc,
                y1: (cy - bh / 2.0) / sc,
                x2: (cx + bw / 2.0) / sc,
                y2: (cy + bh / 2.0) / sc,
                score: sco,
            });
        }
    }
    faces.sort_by(|a, b| b.score.total_cmp(&a.score));
    let mut keep: Vec<Face> = Vec::new();
    for f in faces {
        if keep.iter().all(|k| iou(k, &f) < 0.3) {
            keep.push(f);
        }
    }
    Ok(keep)
}

fn iou(a: &Face, b: &Face) -> f32 {
    let iw = (a.x2.min(b.x2) - a.x1.max(b.x1)).max(0.0);
    let ih = (a.y2.min(b.y2) - a.y1.max(b.y1)).max(0.0);
    let i = iw * ih;
    let u = (a.x2 - a.x1) * (a.y2 - a.y1) + (b.x2 - b.x1) * (b.y2 - b.y1) - i;
    if u <= 0.0 {
        0.0
    } else {
        i / u
    }
}

/// Square crop around a face, expanded by `pad`, clamped to the image.
fn face_crop(img: &RgbImage, f: &Face, pad: f32) -> RgbImage {
    let (cx, cy) = ((f.x1 + f.x2) / 2.0, (f.y1 + f.y2) / 2.0);
    let side = (f.x2 - f.x1).max(f.y2 - f.y1) * (1.0 + pad);
    let x0 = (cx - side / 2.0).max(0.0) as u32;
    let y0 = (cy - side / 2.0).max(0.0) as u32;
    let x1 = ((cx + side / 2.0) as u32).min(img.width());
    let y1 = ((cy + side / 2.0) as u32).min(img.height());
    image::imageops::crop_imm(img, x0, y0, (x1 - x0).max(1), (y1 - y0).max(1)).to_image()
}

// ---------------------------------------------------------------- FER+
fn ferplus(s: &mut Session, crop: &RgbImage) -> R<Vec<f32>> {
    let g = DynamicImage::ImageRgb8(crop.clone())
        .grayscale()
        .resize_exact(64, 64, FilterType::Triangle)
        .to_luma8();
    let data: Vec<f32> = g.pixels().map(|p| p[0] as f32).collect();
    let name = s.inputs()[0].name().to_string();
    let o = run_f32(s, &name, [1, 1, 64, 64], data)?;
    Ok(softmax(&o[0].1))
}
const FER_LABELS: [&str; 8] = [
    "neutral",
    "happiness",
    "surprise",
    "sadness",
    "anger",
    "disgust",
    "fear",
    "contempt",
];

fn softmax(v: &[f32]) -> Vec<f32> {
    let m = v.iter().cloned().fold(f32::MIN, f32::max);
    let e: Vec<f32> = v.iter().map(|x| (x - m).exp()).collect();
    let s: f32 = e.iter().sum();
    e.iter().map(|x| x / s).collect()
}

// ------------------------------------------------- generic 224 encoders
const IMNET: ([f32; 3], [f32; 3]) = ([0.485, 0.456, 0.406], [0.229, 0.224, 0.225]);
const CLIPN: ([f32; 3], [f32; 3]) = (
    [0.481_454_66, 0.457_827_5, 0.408_210_73],
    [0.268_629_55, 0.261_302_6, 0.275_777_1],
);

/// `resize` shortest side to `short`, centre-crop 224, normalise, NCHW.
fn prep224(img: &RgbImage, short: u32, norm: ([f32; 3], [f32; 3]), squash: bool) -> Vec<f32> {
    let r = if squash {
        image::imageops::resize(img, 224, 224, FilterType::CatmullRom)
    } else {
        let (w, h) = (img.width() as f32, img.height() as f32);
        let s = short as f32 / w.min(h);
        let (nw, nh) = (
            ((w * s).round() as u32).max(224),
            ((h * s).round() as u32).max(224),
        );
        let r = image::imageops::resize(img, nw, nh, FilterType::CatmullRom);
        image::imageops::crop_imm(&r, (nw - 224) / 2, (nh - 224) / 2, 224, 224).to_image()
    };
    let mut d = vec![0f32; 3 * 224 * 224];
    for (x, y, p) in r.enumerate_pixels() {
        for c in 0..3 {
            d[c * 224 * 224 + (y * 224 + x) as usize] =
                (p[c] as f32 / 255.0 - norm.0[c]) / norm.1[c];
        }
    }
    d
}

fn l2(mut v: Vec<f32>) -> Vec<f32> {
    let n = v.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-9);
    v.iter_mut().for_each(|x| *x /= n);
    v
}
fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

fn dino(s: &mut Session, img: &RgbImage) -> R<Vec<f32>> {
    let name = s.inputs()[0].name().to_string();
    let o = run_f32(s, &name, [1, 3, 224, 224], prep224(img, 256, IMNET, false))?;
    // last_hidden_state [1,257,384]: CLS token + mean of patch tokens.
    let (sh, d) = (&o[0].0, &o[0].1);
    let dim = sh[2] as usize;
    let mut e = d[..dim].to_vec();
    let n = sh[1] as usize - 1;
    for t in 0..n {
        for k in 0..dim {
            e[k] += d[(1 + t) * dim + k] / n as f32;
        }
    }
    Ok(l2(e))
}

fn clip_img(s: &mut Session, img: &RgbImage, squash: bool) -> R<Vec<f32>> {
    let name = s.inputs()[0].name().to_string();
    let o = run_f32(s, &name, [1, 3, 224, 224], prep224(img, 224, CLIPN, squash))?;
    Ok(l2(o
        .iter()
        .find(|(sh, _)| sh.len() == 2)
        .map(|x| x.1.clone())
        .ok_or("no embed output")?))
}

fn mnet(s: &mut Session, img: &RgbImage) -> R<Vec<f32>> {
    let name = s.inputs()[0].name().to_string();
    let o = run_f32(s, &name, [1, 3, 224, 224], prep224(img, 256, IMNET, false))?;
    Ok(l2(o[0].1.clone()))
}

fn clip_text(prompts: &[&str]) -> R<Vec<Vec<f32>>> {
    let tok = tokenizers::Tokenizer::from_file(model_dir().join("clip_tokenizer.json"))
        .map_err(|e| e.to_string())?;
    let pad = tok.token_to_id("<|endoftext|>").unwrap_or(49407) as i64;
    let mut s = load("clipb32_text_int8.onnx")?;
    let mut out = Vec::new();
    for p in prompts {
        let enc = tok.encode(*p, true).map_err(|e| e.to_string())?;
        let mut ids: Vec<i64> = enc.get_ids().iter().map(|&i| i as i64).collect();
        ids.truncate(77);
        let mut mask = vec![1i64; ids.len()];
        ids.resize(77, pad);
        mask.resize(77, 0);
        let mut inputs = Vec::new();
        for i in s.inputs() {
            let (n, d) = (
                i.name().to_string(),
                if i.name() == "attention_mask" {
                    mask.clone()
                } else {
                    ids.clone()
                },
            );
            inputs.push((
                std::borrow::Cow::Owned(n),
                ort::session::SessionInputValue::from(Tensor::from_array(([1i64, 77], d))?),
            ));
        }
        let o = s.run(inputs)?;
        let mut emb = None;
        for (_, v) in o.iter() {
            let (sh, d) = v.try_extract_tensor::<f32>()?;
            if sh.len() == 2 {
                emb = Some(d.to_vec());
            }
        }
        out.push(l2(emb.ok_or("no text embed")?));
    }
    Ok(out)
}

fn dhash(img: &RgbImage) -> u64 {
    let g = DynamicImage::ImageRgb8(img.clone())
        .grayscale()
        .resize_exact(9, 8, FilterType::Triangle)
        .to_luma8();
    let mut h = 0u64;
    for y in 0..8 {
        for x in 0..8 {
            if g.get_pixel(x, y)[0] > g.get_pixel(x + 1, y)[0] {
                h |= 1 << (y * 8 + x);
            }
        }
    }
    h
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * p).round() as usize]
}

fn bench(model: &str, files: &[PathBuf]) -> R<()> {
    let imgs: Vec<RgbImage> = files
        .iter()
        .map(|f| load_image(f, 1280))
        .collect::<R<_>>()?;
    let rss_base = peak_rss_mb();
    let t0 = Instant::now();
    let (file, kind) = match model {
        "yunet" => ("face_detection_yunet_2026may.onnx", 0),
        "yunet23" => ("face_detection_yunet_2023mar.onnx", 0),
        "ferplus" => ("emotion-ferplus-8.onnx", 1),
        "dino_int8" => ("dinov2s_int8.onnx", 2),
        "dino_fp32" => ("dinov2s_fp32.onnx", 2),
        "clip_int8" => ("clipb32_vision_int8.onnx", 3),
        "mnetv2" => ("mobilenetv2-12.onnx", 4),
        _ => return Err("unknown model".into()),
    };
    let mut s = load(file)?;
    let load_ms = t0.elapsed().as_secs_f64() * 1e3;
    let rss_loaded = peak_rss_mb();
    let mut ts = Vec::new();
    // first run is warm-up (graph optimisation / EP compile) and reported apart
    let mut first = 0.0;
    for (i, img) in imgs.iter().enumerate() {
        let t = Instant::now();
        match kind {
            0 => {
                yunet(&mut s, img, 0.7)?;
            }
            1 => {
                ferplus(
                    &mut s,
                    &image::imageops::crop_imm(
                        img,
                        0,
                        0,
                        200.min(img.width()),
                        200.min(img.height()),
                    )
                    .to_image(),
                )?;
            }
            2 => {
                dino(&mut s, img)?;
            }
            3 => {
                clip_img(&mut s, img, false)?;
            }
            _ => {
                mnet(&mut s, img)?;
            }
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        if i == 0 {
            first = ms
        } else {
            ts.push(ms)
        }
    }
    let mean = ts.iter().sum::<f64>() / ts.len() as f64;
    println!(
        "{{\"model\":\"{model}\",\"threads\":{},\"ep\":\"{}\",\"n\":{},\"load_ms\":{load_ms:.0},\"first_ms\":{first:.1},\"mean_ms\":{mean:.1},\"p50_ms\":{:.1},\"p95_ms\":{:.1},\"rss_base_images_mb\":{rss_base:.0},\"rss_after_load_mb\":{rss_loaded:.0},\"peak_rss_mb\":{:.0},\"model_peak_delta_mb\":{:.0}}}",
        std::env::var("SPIKE_THREADS").unwrap_or("2".into()),
        std::env::var("SPIKE_EP").unwrap_or("cpu".into()),
        ts.len(), pct(&mut ts.clone(), 0.5), pct(&mut ts.clone(), 0.95), peak_rss_mb(), peak_rss_mb() - rss_base
    );
    Ok(())
}

fn draw_rect(img: &mut RgbImage, f: &Face, col: [u8; 3]) {
    let (w, h) = (img.width() as i32, img.height() as i32);
    for t in 0..3 {
        for x in (f.x1 as i32 - t).max(0)..=(f.x2 as i32 + t).min(w - 1) {
            for y in [
                (f.y1 as i32 - t).clamp(0, h - 1),
                (f.y2 as i32 + t).clamp(0, h - 1),
            ] {
                img.put_pixel(x as u32, y as u32, image::Rgb(col));
            }
        }
        for y in (f.y1 as i32 - t).max(0)..=(f.y2 as i32 + t).min(h - 1) {
            for x in [
                (f.x1 as i32 - t).clamp(0, w - 1),
                (f.x2 as i32 + t).clamp(0, w - 1),
            ] {
                img.put_pixel(x as u32, y as u32, image::Rgb(col));
            }
        }
    }
}

fn analyze(out: &Path, files: &[PathBuf]) -> R<()> {
    std::fs::create_dir_all(out.join("annotated"))?;
    let smile_prompts = [
        "a photo of a person smiling",
        "a photo of a person with a neutral expression",
        "a close-up photo of a face with eyes open",
        "a close-up photo of a face with eyes closed",
    ];
    let text = clip_text(&smile_prompts)?;
    let mut yn = load("face_detection_yunet_2026may.onnx")?;
    let mut fer = load("emotion-ferplus-8.onnx")?;
    let mut dn = load("dinov2s_int8.onnx")?;
    let mut cl = load("clipb32_vision_int8.onnx")?;
    let mut rows = Vec::new();
    let mut dvec = Vec::new();
    let mut cvec = Vec::new();
    let mut hashes = Vec::new();
    for f in files {
        let img = load_image(f, 1280)?;
        let faces = yunet(&mut yn, &img, 0.7)?;
        let mut jf = Vec::new();
        let mut ann = img.clone();
        for fc in &faces {
            let crop = face_crop(&img, fc, 0.25);
            let fp = ferplus(&mut fer, &crop)?;
            let ce = clip_img(&mut cl, &crop, true)?;
            let sm = softmax(&[100.0 * dot(&ce, &text[0]), 100.0 * dot(&ce, &text[1])])[0];
            let eo = softmax(&[100.0 * dot(&ce, &text[2]), 100.0 * dot(&ce, &text[3])])[0];
            draw_rect(
                &mut ann,
                fc,
                if sm > 0.5 { [0, 255, 0] } else { [255, 0, 0] },
            );
            jf.push(format!("{{\"w\":{:.0},\"score\":{:.2},\"ferplus_happy\":{:.2},\"ferplus_top\":\"{}\",\"clip_smile\":{:.2},\"clip_eyes_open\":{:.2}}}", fc.x2 - fc.x1, fc.score, fp[1], FER_LABELS[fp.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).unwrap().0], sm, eo));
        }
        image::DynamicImage::ImageRgb8(ann)
            .resize(900, 900, FilterType::Triangle)
            .save(out.join("annotated").join(f.file_name().unwrap()))?;
        dvec.push(dino(&mut dn, &img)?);
        cvec.push(clip_img(&mut cl, &img, false)?);
        hashes.push(dhash(&img));
        rows.push(format!(
            "{{\"file\":\"{}\",\"faces\":[{}]}}",
            f.file_name().unwrap().to_string_lossy(),
            jf.join(",")
        ));
    }
    // all pairs: embedding similarity vs dHash hamming distance
    let mut pairs = Vec::new();
    for i in 0..files.len() {
        for j in i + 1..files.len() {
            pairs.push(format!(
                "{{\"a\":\"{}\",\"b\":\"{}\",\"dino\":{:.3},\"clip\":{:.3},\"dhash_hamming\":{}}}",
                files[i].file_name().unwrap().to_string_lossy(),
                files[j].file_name().unwrap().to_string_lossy(),
                dot(&dvec[i], &dvec[j]),
                dot(&cvec[i], &cvec[j]),
                (hashes[i] ^ hashes[j]).count_ones()
            ));
        }
    }
    std::fs::write(
        out.join("analysis.json"),
        format!(
            "{{\"images\":[{}],\"pairs\":[{}]}}",
            rows.join(",\n"),
            pairs.join(",\n")
        ),
    )?;
    println!("peak_rss_mb {:.0}", peak_rss_mb());
    Ok(())
}

/// int8 vs fp32 DINOv2-small embedding agreement (quantisation sanity check).
fn parity(files: &[PathBuf]) -> R<()> {
    let mut a = load("dinov2s_int8.onnx")?;
    let mut b = load("dinov2s_fp32.onnx")?;
    let mut cos = Vec::new();
    for f in files {
        let img = load_image(f, 1280)?;
        cos.push(f64::from(dot(&dino(&mut a, &img)?, &dino(&mut b, &img)?)));
    }
    let mean = cos.iter().sum::<f64>() / cos.len() as f64;
    let min = cos.iter().cloned().fold(f64::MAX, f64::min);
    println!(
        "dino int8 vs fp32 cosine: mean {mean:.4} min {min:.4} n {}",
        cos.len()
    );
    Ok(())
}

fn main() -> R<()> {
    let a: Vec<String> = std::env::args().collect();
    match a.get(1).map(String::as_str) {
        Some("bench") => bench(&a[2], &a[3..].iter().map(PathBuf::from).collect::<Vec<_>>()),
        Some("parity") => parity(&a[2..].iter().map(PathBuf::from).collect::<Vec<_>>()),
        Some("analyze") => analyze(
            Path::new(&a[2]),
            &a[3..].iter().map(PathBuf::from).collect::<Vec<_>>(),
        ),
        _ => Err("usage: spike bench <model> <imgs..> | spike analyze <out> <imgs..>".into()),
    }
}
