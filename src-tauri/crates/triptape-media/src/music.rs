//! Music bed for the hero film: an ordered playlist joined with
//! `acrossfade`, cycled only when it is shorter than the film. Every seam,
//! including a wrap back to the first track (and a single track looping onto
//! itself), is a crossfade, never a hard cut.
//!
//! [`plan_bed`] is pure timeline maths; [`bed_filter`] turns a plan into the
//! filter-graph fragment used by the final render pass.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Bundled fallback score (`resources/music/triptape-theme.mp3`): the hero
/// film always has a music bed, even when the user passes no `--music`.
static DEFAULT_MUSIC_BYTES: &[u8] =
    include_bytes!("../../../../resources/music/triptape-theme.mp3");

/// Overlap at every track seam.
const CROSSFADE_SECS: f64 = 4.0;
/// `acrossfade` curve for both sides of a seam.
const CROSSFADE_CURVE: &str = "qsin";
/// A seam never eats more than this share of either neighbouring track.
const MAX_SEAM_SHARE: f64 = 0.4;
/// Tracks shorter than this are skipped as unusable.
const MIN_TRACK_SECS: f64 = 1.0;
/// Fade-in at the start of the film.
const FADE_IN_SECS: f64 = 1.0;
/// Fade-out at the film end when the track is cut mid-flow.
const FINAL_FADE_SECS: f64 = 1.5;
/// If the film ends within this long of the last track's own end, the track
/// is close to resolving by itself: a softer, longer fade is used.
const NATURAL_END_WINDOW_SECS: f64 = 10.0;
/// Fade-out used in the natural-end case.
const NATURAL_END_FADE_SECS: f64 = 3.0;
/// Per-track gain adjustment is clamped to this many dB either way.
const MAX_GAIN_DB: f64 = 6.0;
/// Hard cap on placements (guards against pathological tiny tracks).
const MAX_USES: usize = 512;
/// Gains closer to 0 dB than this skip the `volume` filter.
const GAIN_EPSILON_DB: f64 = 0.05;

/// One playable file with its probed duration and loudness-match gain.
#[derive(Debug, Clone)]
pub struct Track {
    pub path: PathBuf,
    pub duration_secs: f64,
    pub gain_db: f64,
}

/// One placement of a playlist track in the bed.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct BedUse {
    /// Index into the playlist.
    pub track: usize,
    /// Where this placement starts in the bed.
    pub start_secs: f64,
}

/// A crossfade between consecutive placements.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Seam {
    /// Bed time at which the crossfade begins.
    pub at_secs: f64,
    pub crossfade_secs: f64,
    /// Playlist index fading out / in.
    pub from_track: usize,
    pub to_track: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BedPlan {
    pub uses: Vec<BedUse>,
    pub seams: Vec<Seam>,
    /// Natural length of the untrimmed bed.
    pub bed_secs: f64,
    /// True when the film end lands in the last window of the final track.
    pub ends_naturally: bool,
    pub fade_out_secs: f64,
}

/// The music bed as rendered, for the EDL: playlist, crossfade seams (bed
/// time = film time, the bed starts with the film), and how the film ends.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EdlMusic {
    pub tracks: Vec<String>,
    pub crossfade_secs: f64,
    pub crossfade_curve: String,
    pub seams: Vec<Seam>,
    pub ends_naturally: bool,
    pub fade_out_secs: f64,
}

impl BedPlan {
    /// EDL record of this plan over `tracks`, seam times rounded to ms.
    pub fn edl(&self, tracks: &[Track]) -> EdlMusic {
        EdlMusic {
            tracks: tracks
                .iter()
                .map(|t| t.path.to_string_lossy().into_owned())
                .collect(),
            crossfade_secs: CROSSFADE_SECS,
            crossfade_curve: CROSSFADE_CURVE.into(),
            seams: self
                .seams
                .iter()
                .map(|x| Seam {
                    at_secs: (x.at_secs * 1000.0).round() / 1000.0,
                    ..x.clone()
                })
                .collect(),
            ends_naturally: self.ends_naturally,
            fade_out_secs: self.fade_out_secs,
        }
    }
}

fn seam_len(prev: f64, next: f64) -> f64 {
    CROSSFADE_SECS
        .min(prev * MAX_SEAM_SHARE)
        .min(next * MAX_SEAM_SHARE)
}

/// Lay the playlist along the film: play in order, wrap to the first track
/// again only while the bed is still shorter than `total`.
pub fn plan_bed(durations: &[f64], total: f64) -> BedPlan {
    assert!(!durations.is_empty(), "plan_bed needs at least one track");
    let mut uses = Vec::new();
    let mut seams = Vec::new();
    let mut bed_end = 0.0;
    let mut prev: Option<(usize, f64)> = None;
    let mut k = 0usize;
    while (uses.is_empty() || bed_end < total - 1e-6) && uses.len() < MAX_USES {
        let i = k % durations.len();
        let d = durations[i];
        let start = match prev {
            None => 0.0,
            Some((pi, pd)) => {
                let xf = seam_len(pd, d);
                let at = bed_end - xf;
                seams.push(Seam {
                    at_secs: at,
                    crossfade_secs: xf,
                    from_track: pi,
                    to_track: i,
                });
                at
            }
        };
        uses.push(BedUse {
            track: i,
            start_secs: start,
        });
        bed_end = start + d;
        prev = Some((i, d));
        k += 1;
    }
    let tail = bed_end - total;
    let ends_naturally = (0.0..NATURAL_END_WINDOW_SECS).contains(&tail);
    BedPlan {
        uses,
        seams,
        bed_secs: bed_end,
        ends_naturally,
        fade_out_secs: if ends_naturally {
            NATURAL_END_FADE_SECS
        } else {
            FINAL_FADE_SECS
        },
    }
}

/// Input files for [`bed_filter`], one per placement in `plan.uses` order;
/// each must be passed as its own `-i`, starting at `first_input`.
pub fn bed_inputs<'a>(plan: &BedPlan, tracks: &'a [Track]) -> Vec<&'a Path> {
    plan.uses
        .iter()
        .map(|u| tracks[u.track].path.as_path())
        .collect()
}

/// Filter-graph fragment producing the finished bed as `[mbed]`: every
/// placement is a separate input (`first_input`, `first_input + 1`, ...,
/// in [`bed_inputs`] order), normalised to 48 kHz stereo float, gain-matched,
/// joined with `acrossfade`, trimmed to `total`, faded in and out.
pub fn bed_filter(plan: &BedPlan, tracks: &[Track], first_input: usize, total: f64) -> String {
    let mut g = String::new();
    for (n, u) in plan.uses.iter().enumerate() {
        let gain = tracks[u.track].gain_db;
        let vol = if gain.abs() > GAIN_EPSILON_DB {
            format!(",volume={gain:.2}dB")
        } else {
            String::new()
        };
        g.push_str(&format!(
            "[{}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo,asetpts=PTS-STARTPTS{vol}[t{n}];",
            first_input + n
        ));
    }
    let mut cur = "[t0]".to_string();
    for (n, seam) in plan.seams.iter().enumerate() {
        let out = format!("[x{}]", n + 1);
        g.push_str(&format!(
            "{cur}[t{}]acrossfade=d={:.3}:c1={CROSSFADE_CURVE}:c2={CROSSFADE_CURVE}{out};",
            n + 1,
            seam.crossfade_secs
        ));
        cur = out;
    }
    let fade_out_start = (total - plan.fade_out_secs).max(0.0);
    g.push_str(&format!(
        "{cur}atrim=0:{total:.3},asetpts=PTS-STARTPTS,apad=whole_dur={total:.3},afade=t=in:st=0:d={FADE_IN_SECS},afade=t=out:st={fade_out_start:.3}:d={:.3}[mbed]",
        plan.fade_out_secs
    ));
    g
}

/// Duration of an audio file via ffprobe (`ffprobe_file` insists on video).
fn probe_audio_secs(ffprobe_bin: &str, path: &Path) -> Result<f64, crate::MediaError> {
    let out = Command::new(ffprobe_bin)
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
        ])
        .arg(path)
        .output()
        .map_err(|e| crate::MediaError::Ffmpeg(format!("spawn {ffprobe_bin}: {e}")))?;
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|d| out.status.success() && d.is_finite())
        .ok_or_else(|| crate::MediaError::Ffmpeg(format!("no audio duration: {}", path.display())))
}

/// Probe the playlist: skip missing or unreadable files (noting each in
/// `fallbacks`), then measure loudness for gain matching when there is more
/// than one track. With no listed file usable, the bundled score is written
/// into `work_dir` and used instead.
pub fn load_playlist(
    ffmpeg_bin: &str,
    ffprobe_bin: &str,
    paths: &[PathBuf],
    work_dir: &Path,
    fallbacks: &mut Vec<String>,
) -> Result<Vec<Track>, crate::MediaError> {
    let mut tracks: Vec<Track> = Vec::new();
    for p in paths {
        if !p.exists() {
            fallbacks.push(format!("music file {} missing; skipped", p.display()));
            continue;
        }
        match probe_audio_secs(ffprobe_bin, p) {
            Ok(d) if d >= MIN_TRACK_SECS => tracks.push(Track {
                path: p.clone(),
                duration_secs: d,
                gain_db: 0.0,
            }),
            _ => fallbacks.push(format!("music file {} unreadable; skipped", p.display())),
        }
    }
    if tracks.is_empty() {
        let b = work_dir.join("triptape-theme.mp3");
        std::fs::write(&b, DEFAULT_MUSIC_BYTES)
            .map_err(|e| crate::MediaError::Io(e.to_string()))?;
        let d = probe_audio_secs(ffprobe_bin, &b)?;
        fallbacks.push("bundled triptape-theme music bed".into());
        return Ok(vec![Track {
            path: b,
            duration_secs: d,
            gain_db: 0.0,
        }]);
    }
    if tracks.len() > 1 {
        // Each measurement is a full decode: run them concurrently.
        let lufs: Vec<Option<f64>> = std::thread::scope(|scope| {
            let handles: Vec<_> = tracks
                .iter()
                .map(|t| scope.spawn(|| measure_lufs(ffmpeg_bin, &t.path)))
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().ok().flatten())
                .collect()
        });
        for (t, g) in tracks.iter_mut().zip(match_gains(&lufs)) {
            t.gain_db = g;
        }
    }
    Ok(tracks)
}

/// Integrated loudness (LUFS) of a file via `ebur128`; `None` on any failure.
fn measure_lufs(ffmpeg_bin: &str, path: &Path) -> Option<f64> {
    let out = Command::new(ffmpeg_bin)
        .args(["-hide_banner", "-nostats", "-i"])
        .arg(path)
        .args(["-af", "ebur128=peak=none", "-vn", "-f", "null", "-"])
        .output()
        .ok()?;
    parse_integrated_lufs(&String::from_utf8_lossy(&out.stderr))
}

fn parse_integrated_lufs(stderr: &str) -> Option<f64> {
    // The closing summary holds "    I:         -20.1 LUFS".
    let summary = &stderr[stderr.rfind("Integrated loudness")?..];
    let line = summary.lines().find(|l| l.trim_start().starts_with("I:"))?;
    let v: f64 = line
        .trim_start()
        .strip_prefix("I:")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    v.is_finite().then_some(v)
}

/// Per-track gains pulling each track toward the playlist's mean loudness
/// (clamped); tracks without a measurement stay at 0 dB.
fn match_gains(lufs: &[Option<f64>]) -> Vec<f64> {
    let known: Vec<f64> = lufs.iter().flatten().copied().collect();
    if known.len() < 2 {
        return vec![0.0; lufs.len()];
    }
    let mean = known.iter().sum::<f64>() / known.len() as f64;
    lufs.iter()
        .map(|l| l.map_or(0.0, |v| (mean - v).clamp(-MAX_GAIN_DB, MAX_GAIN_DB)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracks(durs: &[f64]) -> Vec<Track> {
        durs.iter()
            .enumerate()
            .map(|(i, d)| Track {
                path: PathBuf::from(format!("t{i}.wav")),
                duration_secs: *d,
                gain_db: 0.0,
            })
            .collect()
    }

    #[test]
    fn film_shorter_than_first_track_is_plain_trim() {
        let p = plan_bed(&[156.0, 120.0], 60.0);
        assert_eq!(p.uses.len(), 1);
        assert!(p.seams.is_empty());
        let g = bed_filter(&p, &tracks(&[156.0, 120.0]), 1, 60.0);
        assert!(!g.contains("acrossfade"), "{g}");
        assert!(g.contains("[1:a]aresample=48000"));
        assert!(g.contains("atrim=0:60.000"));
        assert!(g.ends_with("[mbed]"));
    }

    #[test]
    fn two_tracks_one_crossfade() {
        let p = plan_bed(&[100.0, 100.0], 180.0);
        assert_eq!(p.uses.len(), 2);
        assert_eq!(p.seams.len(), 1);
        assert_eq!(p.seams[0].at_secs, 96.0);
        assert_eq!(p.seams[0].crossfade_secs, CROSSFADE_SECS);
        assert_eq!(p.bed_secs, 196.0);
        assert!(!p.ends_naturally);
        let g = bed_filter(&p, &tracks(&[100.0, 100.0]), 1, 180.0);
        assert_eq!(g.matches("acrossfade").count(), 1);
        assert!(
            g.contains("[t0][t1]acrossfade=d=4.000:c1=qsin:c2=qsin[x1]"),
            "{g}"
        );
        assert!(g.contains("[2:a]"));
        assert!(g.contains("afade=t=out:st=178.500:d=1.500[mbed]"), "{g}");
    }

    #[test]
    fn three_tracks_chain() {
        let p = plan_bed(&[100.0, 80.0, 90.0], 250.0);
        assert_eq!(
            p.uses.iter().map(|u| u.track).collect::<Vec<_>>(),
            [0, 1, 2]
        );
        assert_eq!(p.seams.len(), 2);
        assert_eq!(p.seams[1].at_secs, 96.0 + 80.0 - 4.0);
        let g = bed_filter(&p, &tracks(&[100.0, 80.0, 90.0]), 1, 250.0);
        assert!(g.contains("[x1][t2]acrossfade"), "{g}");
    }

    #[test]
    fn wraps_with_crossfade_only_when_needed() {
        let p = plan_bed(&[100.0, 100.0], 500.0);
        let order: Vec<usize> = p.uses.iter().map(|u| u.track).collect();
        assert_eq!(order, [0, 1, 0, 1, 0, 1]);
        assert_eq!(p.seams.len(), 5);
        assert_eq!(p.seams[1].from_track, 1);
        assert_eq!(p.seams[1].to_track, 0, "wrap seam is a crossfade");
        let q = plan_bed(&[100.0, 100.0], 196.0);
        assert_eq!(q.uses.len(), 2, "no needless extra track");
    }

    #[test]
    fn single_track_loops_onto_itself_with_crossfade() {
        let p = plan_bed(&[156.0], 620.0);
        assert!(p.uses.len() >= 5);
        assert!(p.seams.iter().all(|s| s.from_track == 0 && s.to_track == 0));
        assert!(p.bed_secs >= 620.0);
        let g = bed_filter(&p, &tracks(&[156.0]), 1, 620.0);
        assert_eq!(g.matches("acrossfade").count(), p.seams.len());
        assert!(!g.contains("stream_loop"));
    }

    #[test]
    fn natural_end_window_softens_final_fade() {
        let near = plan_bed(&[100.0], 95.0);
        assert!(near.ends_naturally);
        assert_eq!(near.fade_out_secs, NATURAL_END_FADE_SECS);
        let far = plan_bed(&[100.0], 60.0);
        assert!(!far.ends_naturally);
        assert_eq!(far.fade_out_secs, FINAL_FADE_SECS);
    }

    #[test]
    fn short_tracks_clamp_crossfade() {
        let p = plan_bed(&[5.0, 5.0], 20.0);
        assert!(p.seams.iter().all(|s| s.crossfade_secs <= 2.0 + 1e-9));
        assert!(p.bed_secs >= 20.0);
    }

    #[test]
    fn mismatched_formats_normalised_per_input_and_gain_applied() {
        let mut t = tracks(&[100.0, 100.0]);
        t[1].gain_db = -3.5;
        let p = plan_bed(&[100.0, 100.0], 150.0);
        let g = bed_filter(&p, &t, 1, 150.0);
        for n in 0..2 {
            assert!(
                g.contains(&format!(
                    "[{}:a]aresample=48000,aformat=sample_fmts=fltp:channel_layouts=stereo",
                    n + 1
                )),
                "{g}"
            );
        }
        assert!(g.contains("volume=-3.50dB[t1]"), "{g}");
        assert!(!g.contains("volume=0"), "no gain filter for 0 dB: {g}");
    }

    #[test]
    fn lufs_parse_and_gain_match() {
        let s =
            "...\n  Integrated loudness:\n    I:         -20.1 LUFS\n    Threshold: -30.3 LUFS\n";
        assert_eq!(parse_integrated_lufs(s), Some(-20.1));
        assert_eq!(parse_integrated_lufs("nothing"), None);
        let g = match_gains(&[Some(-20.0), Some(-26.0), None]);
        assert_eq!(g, vec![-3.0, 3.0, 0.0]);
        assert_eq!(match_gains(&[Some(-20.0)]), vec![0.0]);
        assert_eq!(match_gains(&[Some(-10.0), Some(-40.0)]), vec![-6.0, 6.0]);
    }

    #[test]
    fn bed_inputs_follow_uses_and_edl_rounds_seams() {
        let t = tracks(&[100.0 / 3.0, 50.0]);
        let p = plan_bed(&[t[0].duration_secs, t[1].duration_secs], 120.0);
        let inputs = bed_inputs(&p, &t);
        assert_eq!(inputs.len(), p.uses.len());
        for (m, u) in inputs.iter().zip(&p.uses) {
            assert_eq!(*m, t[u.track].path.as_path());
        }
        let e = p.edl(&t);
        assert_eq!(e.tracks, ["t0.wav", "t1.wav"]);
        assert_eq!(e.seams.len(), p.seams.len());
        for (r, x) in e.seams.iter().zip(&p.seams) {
            assert!((r.at_secs - x.at_secs).abs() <= 0.0005);
            assert_eq!(r.at_secs, (r.at_secs * 1000.0).round() / 1000.0);
            assert_eq!(r.from_track, x.from_track);
        }
        assert_eq!(e.fade_out_secs, p.fade_out_secs);
    }

    /// No usable listed file: missing paths are noted and the bundled theme
    /// is written to the work dir and probed.
    #[test]
    fn real_ffmpeg_falls_back_to_bundled_theme() {
        let Some((ffmpeg, ffprobe)) = full_ffmpeg() else {
            eprintln!("ffmpeg-full missing; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mut fb = Vec::new();
        let missing = dir.path().join("nope.wav");
        let t = load_playlist(&ffmpeg, &ffprobe, &[missing], dir.path(), &mut fb).unwrap();
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].path, dir.path().join("triptape-theme.mp3"));
        assert!(t[0].duration_secs > MIN_TRACK_SECS);
        assert!(fb.iter().any(|f| f.contains("missing; skipped")), "{fb:?}");
        assert!(fb.iter().any(|f| f.contains("bundled")), "{fb:?}");
    }

    fn full_ffmpeg() -> Option<(String, String)> {
        let dir = "/opt/homebrew/opt/ffmpeg-full/bin";
        let (f, p) = (format!("{dir}/ffmpeg"), format!("{dir}/ffprobe"));
        (Path::new(&f).exists() && Path::new(&p).exists()).then_some((f, p))
    }

    /// Short lavfi tones (different rates and layouts), a real acrossfade
    /// bed, then: exact film length and no silence at any seam.
    #[test]
    fn real_ffmpeg_bed_has_exact_length_and_no_gap() {
        let Some((ffmpeg, ffprobe)) = full_ffmpeg() else {
            eprintln!("ffmpeg-full missing; skipping");
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let mk = |name: &str, spec: &str| {
            let out = dir.path().join(name);
            let st = Command::new(&ffmpeg)
                .args(["-y", "-v", "error", "-f", "lavfi", "-i", spec])
                .arg(&out)
                .status()
                .unwrap();
            assert!(st.success());
            out
        };
        let a = mk("a.wav", "sine=frequency=440:sample_rate=44100:duration=10");
        let b = mk(
            "b.wav",
            "sine=frequency=660:sample_rate=48000:duration=8,pan=stereo|c0=c0|c1=c0",
        );
        let paths = vec![a, b];
        for total in [6.0_f64, 25.0] {
            let mut fb = Vec::new();
            let tracks = load_playlist(&ffmpeg, &ffprobe, &paths, dir.path(), &mut fb)
                .unwrap_or_else(|e| panic!("{e}: {fb:?}"));
            assert_eq!(tracks.len(), 2);
            let plan = plan_bed(
                &tracks.iter().map(|t| t.duration_secs).collect::<Vec<_>>(),
                total,
            );
            if total > 20.0 {
                assert!(plan.seams.len() >= 2, "wraps: {:?}", plan.seams);
            }
            let graph = bed_filter(&plan, &tracks, 0, total);
            let out = dir.path().join("bed.wav");
            let mut cmd = Command::new(&ffmpeg);
            cmd.args(["-y", "-v", "error"]);
            for m in bed_inputs(&plan, &tracks) {
                cmd.arg("-i").arg(m);
            }
            let st = cmd
                .args(["-filter_complex", &graph, "-map", "[mbed]"])
                .arg(&out)
                .status()
                .unwrap();
            assert!(st.success(), "graph failed: {graph}");
            let dur = probe_audio_secs(&ffprobe, &out).unwrap();
            assert!((dur - total).abs() < 0.05, "duration {dur} vs {total}");
            // Silence anywhere but the first/last fade samples means a gap.
            let probe = Command::new(&ffmpeg)
                .args(["-hide_banner", "-nostats", "-i"])
                .arg(&out)
                .args(["-af", "silencedetect=noise=-40dB:d=0.05", "-f", "null", "-"])
                .output()
                .unwrap();
            let log = String::from_utf8_lossy(&probe.stderr);
            let starts: Vec<&str> = log
                .lines()
                .filter(|l| l.contains("silence_start"))
                .collect();
            for l in &starts {
                let t: f64 = l.rsplit(' ').next().unwrap().parse().unwrap();
                assert!(
                    t < 0.1 || t > total - plan.fade_out_secs,
                    "silence mid-bed at {t}s (total {total}): {log}"
                );
            }
        }
    }

    /// Manual audio-only check on real tracks:
    /// `MUSIC_REAL="a.wav;b.wav" MUSIC_REAL_SECS=620 MUSIC_REAL_OUT=out.wav
    /// cargo test -p triptape-media real_tracks -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn real_tracks_bed() {
        let (Some((ffmpeg, ffprobe)), Ok(list), Ok(out)) = (
            full_ffmpeg(),
            std::env::var("MUSIC_REAL"),
            std::env::var("MUSIC_REAL_OUT"),
        ) else {
            return;
        };
        let total: f64 = std::env::var("MUSIC_REAL_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(620.0);
        let paths: Vec<PathBuf> = list.split(';').map(PathBuf::from).collect();
        let mut fb = Vec::new();
        let dir = tempfile::tempdir().unwrap();
        let tracks = load_playlist(&ffmpeg, &ffprobe, &paths, dir.path(), &mut fb).unwrap();
        let plan = plan_bed(
            &tracks.iter().map(|t| t.duration_secs).collect::<Vec<_>>(),
            total,
        );
        for t in &tracks {
            eprintln!(
                "{} {:.1}s gain {:+.2} dB",
                t.path.display(),
                t.duration_secs,
                t.gain_db
            );
        }
        eprintln!(
            "seams: {:?}\nnatural end: {}",
            plan.seams, plan.ends_naturally
        );
        let mut cmd = Command::new(&ffmpeg);
        cmd.args(["-y", "-v", "error"]);
        for m in bed_inputs(&plan, &tracks) {
            cmd.arg("-i").arg(m);
        }
        let st = cmd
            .args([
                "-filter_complex",
                &bed_filter(&plan, &tracks, 0, total),
                "-map",
                "[mbed]",
            ])
            .arg(&out)
            .status()
            .unwrap();
        assert!(st.success());
        eprintln!(
            "bed duration {:.3}s",
            probe_audio_secs(&ffprobe, Path::new(&out)).unwrap()
        );
    }
}
