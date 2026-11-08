//! Render progress and cancellation, shared by the CLI and the desktop app.
//!
//! Renderers talk to a [`Progress`] handle: it forwards [`ProgressEvent`]s to
//! a [`ProgressSink`] (stderr for the CLI, the `job://progress` stream for the
//! app) and carries the cooperative cancel flag. A default handle reports
//! nothing and never cancels, so library callers and tests pay nothing.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One progress report from a renderer.
#[derive(Debug, Clone, PartialEq)]
pub enum ProgressEvent {
    /// `done` of `total` units finished ("segment", "reel", "page", ...).
    Counted {
        keepsake: &'static str,
        unit: &'static str,
        done: usize,
        total: usize,
    },
    /// A single long step, with the fraction done when it is knowable.
    Step {
        keepsake: &'static str,
        label: &'static str,
        fraction: Option<f64>,
    },
}

impl ProgressEvent {
    pub fn keepsake(&self) -> &'static str {
        match self {
            Self::Counted { keepsake, .. } | Self::Step { keepsake, .. } => keepsake,
        }
    }

    /// Fraction of the whole *keepsake* this event stands for, in `0..=1`,
    /// or `None` while a step cannot say. Each phase owns a band of the
    /// keepsake (segments are most of a film, the final encode the tail).
    /// Used by the app, which has one number per job rather than a text line.
    pub fn keepsake_fraction(&self) -> Option<f64> {
        let (name, within) = match self {
            Self::Counted {
                unit, done, total, ..
            } => (*unit, *done as f64 / (*total).max(1) as f64),
            Self::Step {
                label, fraction, ..
            } => (*label, (*fraction)?),
        };
        let (lo, hi) = match (self.keepsake(), name) {
            ("video", "segment") => (0.0, 0.70),
            ("video", "overlay") => (0.70, 0.75),
            ("video", n) if n.starts_with("assembling") => (0.75, 0.90),
            ("video", _) => (0.90, 1.0),
            ("book", "page") => (0.0, 0.90),
            ("book", _) => (0.90, 1.0),
            _ => (0.0, 1.0),
        };
        Some(lo + (hi - lo) * within.clamp(0.0, 1.0))
    }

    /// `video: segment 17/152 (11%)`, `video: final pass 42%`, or
    /// `video: assembling transitions…` when no fraction is known.
    pub fn text(&self) -> String {
        match self {
            Self::Counted {
                keepsake,
                unit,
                done,
                total,
            } => {
                let pct = percent(*done as f64 / (*total).max(1) as f64);
                format!("{keepsake}: {unit} {done}/{total} ({pct}%)")
            }
            Self::Step {
                keepsake,
                label,
                fraction: Some(f),
            } => format!("{keepsake}: {label} {}%", percent(*f)),
            Self::Step {
                keepsake,
                label,
                fraction: None,
            } => format!("{keepsake}: {label}…"),
        }
    }

    /// Identity of the phase, so sinks can tell a new phase from an update.
    fn phase_key(&self) -> (&'static str, &'static str) {
        match self {
            Self::Counted { keepsake, unit, .. } => (keepsake, unit),
            Self::Step {
                keepsake, label, ..
            } => (keepsake, label),
        }
    }
}

/// Receives progress. `report` may be called from any render thread.
pub trait ProgressSink: Send + Sync {
    fn report(&self, event: &ProgressEvent);

    /// True when the owner wants the render stopped (the app's job row went
    /// `cancelled`). Polled often, so implementations must be cheap.
    fn cancelled(&self) -> bool {
        false
    }
}

/// Cheap-to-clone handle renderers report through and poll for cancel.
#[derive(Clone, Default)]
pub struct Progress {
    sink: Option<Arc<dyn ProgressSink>>,
    cancel: Arc<AtomicBool>,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress")
            .field("sink", &self.sink.is_some())
            .field("cancelled", &self.cancel.load(Ordering::Relaxed))
            .finish()
    }
}

impl Progress {
    pub fn new(sink: Arc<dyn ProgressSink>) -> Self {
        Self {
            sink: Some(sink),
            cancel: Arc::default(),
        }
    }

    /// The flag [`Progress::is_cancelled`] watches; set it to stop the render.
    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
    }

    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancel.load(Ordering::Relaxed) || self.sink.as_ref().is_some_and(|s| s.cancelled())
    }

    /// `Err(Cancelled)` once a cancel was requested, for `?` at loop heads.
    pub fn check_cancelled(&self) -> Result<(), crate::MediaError> {
        if self.is_cancelled() {
            Err(crate::MediaError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub fn report(&self, event: ProgressEvent) {
        if let Some(s) = &self.sink {
            s.report(&event);
        }
    }

    pub fn counted(&self, keepsake: &'static str, unit: &'static str, done: usize, total: usize) {
        self.report(ProgressEvent::Counted {
            keepsake,
            unit,
            done,
            total,
        });
    }

    pub fn step(&self, keepsake: &'static str, label: &'static str, fraction: Option<f64>) {
        self.report(ProgressEvent::Step {
            keepsake,
            label,
            fraction,
        });
    }
}

// ---------------------------------------------------------------------------
// Text rendering shared by the CLI sink.
// ---------------------------------------------------------------------------

/// `4m12s`, `1h02m`, `45s`: elapsed time.
pub fn fmt_elapsed(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `<1m`, `28m`, `1h05m`: a remaining-time estimate, rounded up to a minute.
pub fn fmt_eta(secs: f64) -> String {
    let m = (secs.max(0.0) / 60.0).ceil() as u64;
    match m {
        0 | 1 => "<1m".to_string(),
        2..=59 => format!("{m}m"),
        _ => format!("{}h{:02}m", m / 60, m % 60),
    }
}

/// Moving-average throughput over the last few completions. Averaging the
/// span across a window (not the mean of per-item gaps) stays honest when
/// parallel workers finish in bursts.
#[derive(Debug, Clone)]
pub struct EtaEstimator {
    window: VecDeque<(f64, usize)>,
    cap: usize,
}

impl EtaEstimator {
    pub fn new(window: usize) -> Self {
        Self {
            window: VecDeque::new(),
            cap: window.max(2),
        }
    }

    /// Record `done` items finished at `t` seconds since the phase began.
    pub fn record(&mut self, t: f64, done: usize) {
        if self.window.is_empty() {
            // Anchor: nothing was done when the phase began.
            self.window.push_back((0.0, 0));
        }
        if self.window.back().is_some_and(|&(_, d)| done <= d) {
            return;
        }
        self.window.push_back((t, done));
        while self.window.len() > self.cap {
            self.window.pop_front();
        }
    }

    /// Seconds left for `total - done` items, once there is a rate to go on.
    pub fn eta(&self, done: usize, total: usize) -> Option<f64> {
        let (t0, d0) = *self.window.front()?;
        let (t1, d1) = *self.window.back()?;
        if d1 <= d0 || t1 <= t0 {
            return None;
        }
        let rate = (t1 - t0) / (d1 - d0) as f64;
        Some(rate * total.saturating_sub(done) as f64)
    }
}

/// Text for one event. `eta` is the remaining seconds when known.
pub fn format_line(event: &ProgressEvent, elapsed: f64, eta: Option<f64>) -> String {
    let mut parts = vec![event.text(), format!("elapsed {}", fmt_elapsed(elapsed))];
    if let Some(e) = eta {
        parts.push(format!("ETA {}", fmt_eta(e)));
    }
    parts.join(" · ")
}

fn percent(f: f64) -> u32 {
    (f.clamp(0.0, 1.0) * 100.0).floor() as u32
}

/// Minimum gap between in-place redraws on a terminal.
const TTY_REDRAW: Duration = Duration::from_millis(250);

struct ConsoleState<W> {
    out: W,
    phase: Option<(&'static str, &'static str)>,
    keepsake: Option<&'static str>,
    keepsake_start: Instant,
    phase_start: Instant,
    eta: EtaEstimator,
    last_draw: Option<Instant>,
    /// Last bucket printed on a non-terminal (5% steps).
    last_bucket: Option<u32>,
    /// A `\r` line is open and needs a newline before other output.
    line_open: bool,
}

/// Progress on stderr. On a terminal the line is rewritten in place; when
/// piped it prints plain lines at a sensible cadence (every item for short
/// phases, otherwise each 5%).
pub struct ConsoleProgress<W: Write + Send> {
    tty: bool,
    state: Mutex<ConsoleState<W>>,
}

impl ConsoleProgress<std::io::Stderr> {
    pub fn stderr() -> Self {
        use std::io::IsTerminal;
        Self::new(std::io::stderr(), std::io::stderr().is_terminal())
    }
}

impl<W: Write + Send> ConsoleProgress<W> {
    pub fn new(out: W, tty: bool) -> Self {
        let now = Instant::now();
        Self {
            tty,
            state: Mutex::new(ConsoleState {
                out,
                phase: None,
                keepsake: None,
                keepsake_start: now,
                phase_start: now,
                eta: EtaEstimator::new(12),
                last_draw: None,
                last_bucket: None,
                line_open: false,
            }),
        }
    }

    /// Close an open in-place line so later output starts on a fresh one.
    pub fn finish(&self) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if st.line_open {
            let _ = writeln!(st.out);
            st.line_open = false;
        }
        let _ = st.out.flush();
    }

    /// Report with an explicit clock (tests drive time directly).
    fn report_at(&self, event: &ProgressEvent, now: Instant) {
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let key = event.phase_key();
        let new_phase = st.phase != Some(key);
        if st.keepsake != Some(event.keepsake()) {
            st.keepsake = Some(event.keepsake());
            st.keepsake_start = now;
        }
        if new_phase {
            if st.line_open {
                let _ = writeln!(st.out);
                st.line_open = false;
            }
            st.phase = Some(key);
            st.phase_start = now;
            st.eta = EtaEstimator::new(12);
            st.last_draw = None;
            st.last_bucket = None;
        }
        let in_phase = now.saturating_duration_since(st.phase_start).as_secs_f64();
        let elapsed = now
            .saturating_duration_since(st.keepsake_start)
            .as_secs_f64();
        // (done, total, fraction) drives ETA and cadence.
        let (frac, last, small) = match event {
            ProgressEvent::Counted { done, total, .. } => {
                st.eta.record(in_phase, *done);
                (
                    *done as f64 / (*total).max(1) as f64,
                    done >= total,
                    *total <= 20,
                )
            }
            ProgressEvent::Step { fraction, .. } => (fraction.unwrap_or(0.0), false, false),
        };
        let eta = match event {
            ProgressEvent::Counted { done, total, .. } if done < total => st.eta.eta(*done, *total),
            ProgressEvent::Counted { .. } => None,
            // A step's own fraction is its rate; ignore the first sliver.
            ProgressEvent::Step {
                fraction: Some(f), ..
            } if *f >= 0.02 && *f < 1.0 => Some(in_phase * (1.0 - f) / f),
            ProgressEvent::Step { .. } => None,
        };
        let line = format_line(event, elapsed, eta);

        if self.tty {
            let due = st
                .last_draw
                .is_none_or(|t| now.saturating_duration_since(t) >= TTY_REDRAW);
            if due || last {
                st.last_draw = Some(now);
                let _ = write!(st.out, "\r\x1b[2K{line}");
                let _ = st.out.flush();
                st.line_open = true;
            }
        } else {
            let bucket = percent(frac) / 5;
            let is_step = matches!(event, ProgressEvent::Step { .. });
            // Steps print at 10% marks (and once without a fraction);
            // counted phases every item when short, else each 5%.
            let print = if is_step {
                st.last_bucket.is_none() || (bucket / 2 > st.last_bucket.unwrap_or(0) / 2)
            } else {
                small || last || st.last_bucket.is_none_or(|b| bucket > b)
            };
            if print {
                st.last_bucket = Some(bucket);
                let _ = writeln!(st.out, "{line}");
                let _ = st.out.flush();
            }
        }
    }
}

impl<W: Write + Send> ProgressSink for ConsoleProgress<W> {
    fn report(&self, event: &ProgressEvent) {
        self.report_at(event, Instant::now());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Buf {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn seg(done: usize, total: usize) -> ProgressEvent {
        ProgressEvent::Counted {
            keepsake: "video",
            unit: "segment",
            done,
            total,
        }
    }

    #[test]
    fn durations_format_compactly() {
        assert_eq!(fmt_elapsed(0.0), "0s");
        assert_eq!(fmt_elapsed(59.4), "59s");
        assert_eq!(fmt_elapsed(252.0), "4m12s");
        assert_eq!(fmt_elapsed(3720.0), "1h02m");
        assert_eq!(fmt_eta(10.0), "<1m");
        assert_eq!(fmt_eta(61.0), "2m");
        assert_eq!(fmt_eta(28.0 * 60.0), "28m");
        assert_eq!(fmt_eta(3900.0), "1h05m");
    }

    #[test]
    fn line_matches_the_documented_shape() {
        assert_eq!(
            format_line(&seg(17, 152), 252.0, Some(28.0 * 60.0)),
            "video: segment 17/152 (11%) · elapsed 4m12s · ETA 28m"
        );
        let step = ProgressEvent::Step {
            keepsake: "video",
            label: "assembling transitions",
            fraction: None,
        };
        assert_eq!(
            format_line(&step, 3.0, None),
            "video: assembling transitions… · elapsed 3s"
        );
        let with = ProgressEvent::Step {
            keepsake: "video",
            label: "assembling transitions",
            fraction: Some(0.426),
        };
        assert!(format_line(&with, 3.0, None).contains("assembling transitions 42%"));
    }

    #[test]
    fn eta_uses_a_moving_window_and_survives_bursts() {
        let mut e = EtaEstimator::new(4);
        assert_eq!(e.eta(0, 10), None);
        // Slow start: 10 s per item for the first three.
        for i in 1..=3 {
            e.record(10.0 * i as f64, i);
        }
        assert!((e.eta(3, 10).unwrap() - 70.0).abs() < 1e-9);
        // Speeds up to 2 s per item; the window forgets the slow start.
        for i in 4..=9 {
            e.record(30.0 + 2.0 * (i - 3) as f64, i);
        }
        let eta = e.eta(9, 10).unwrap();
        assert!((eta - 2.0).abs() < 1e-9, "{eta}");
        // A burst (two workers finishing together) does not divide by zero.
        let mut b = EtaEstimator::new(8);
        b.record(5.0, 1);
        b.record(5.0, 2);
        assert!(b.eta(2, 10).unwrap() > 0.0);
        // Duplicate/regressing counts are ignored.
        b.record(6.0, 2);
        assert_eq!(b.window.back().unwrap().1, 2);
    }

    #[test]
    fn fraction_maps_phases_into_keepsake_bands() {
        let near = |a: Option<f64>, b: f64| assert!((a.unwrap() - b).abs() < 1e-9, "{a:?}");
        near(seg(50, 100).keepsake_fraction(), 0.35);
        let step = |label, fraction| ProgressEvent::Step {
            keepsake: "video",
            label,
            fraction,
        };
        near(
            step("assembling transitions", Some(0.5)).keepsake_fraction(),
            0.825,
        );
        near(
            step("final pass (music)", Some(1.5)).keepsake_fraction(),
            1.0,
        );
        assert_eq!(step("final pass", None).keepsake_fraction(), None);
        let page = ProgressEvent::Counted {
            keepsake: "book",
            unit: "page",
            done: 10,
            total: 10,
        };
        near(page.keepsake_fraction(), 0.9);
    }

    #[test]
    fn tty_rewrites_one_line_in_place() {
        let buf = Buf::default();
        let c = ConsoleProgress::new(buf.clone(), true);
        let t0 = Instant::now();
        c.report_at(&seg(1, 40), t0);
        // Inside the redraw window: dropped.
        c.report_at(&seg(2, 40), t0 + Duration::from_millis(50));
        c.report_at(&seg(3, 40), t0 + Duration::from_secs(1));
        c.report_at(&seg(40, 40), t0 + Duration::from_secs(2));
        let text = buf.text();
        assert!(!text.contains('\n'), "{text:?}");
        assert_eq!(text.matches("\r\x1b[2K").count(), 3, "{text:?}");
        assert!(!text.contains("segment 2/40"));
        assert!(text.ends_with("video: segment 40/40 (100%) · elapsed 2s"));
        // A new phase closes the line; finish() closes the last one.
        c.report_at(
            &ProgressEvent::Step {
                keepsake: "video",
                label: "assembling transitions",
                fraction: None,
            },
            t0 + Duration::from_secs(3),
        );
        c.finish();
        let text = buf.text();
        assert_eq!(text.matches('\n').count(), 2, "{text:?}");
    }

    #[test]
    fn plain_output_prints_lines_at_five_percent_steps() {
        let buf = Buf::default();
        let c = ConsoleProgress::new(buf.clone(), false);
        let t0 = Instant::now();
        for i in 1..=100 {
            c.report_at(&seg(i, 100), t0 + Duration::from_secs(i as u64));
        }
        let text = buf.text();
        assert!(!text.contains('\r') && !text.contains('\x1b'), "{text:?}");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 21, "{text}");
        assert!(lines[0].starts_with("video: segment 1/100 (1%)"));
        assert!(lines[1].contains("segment 5/100 (5%)"));
        assert!(lines[20].contains("segment 100/100 (100%)"));
        // ETA appears once there is a rate.
        assert!(lines[1].contains("ETA"));

        // Short phases print every item.
        let buf = Buf::default();
        let c = ConsoleProgress::new(buf.clone(), false);
        for i in 1..=5 {
            c.report_at(&seg(i, 5), t0);
        }
        assert_eq!(buf.text().lines().count(), 5);
    }

    #[test]
    fn plain_steps_print_once_then_at_ten_percent() {
        let buf = Buf::default();
        let c = ConsoleProgress::new(buf.clone(), false);
        let t0 = Instant::now();
        let step = |f| ProgressEvent::Step {
            keepsake: "video",
            label: "final pass",
            fraction: f,
        };
        c.report_at(&step(None), t0);
        c.report_at(&step(None), t0);
        assert_eq!(buf.text().lines().count(), 1);
        let buf = Buf::default();
        let c = ConsoleProgress::new(buf.clone(), false);
        for i in 0..=100 {
            c.report_at(&step(Some(f64::from(i) / 100.0)), t0);
        }
        let n = buf.text().lines().count();
        assert!((10..=12).contains(&n), "{n}");
    }

    #[test]
    fn cancel_flag_and_sink_both_cancel() {
        struct Stop;
        impl ProgressSink for Stop {
            fn report(&self, _: &ProgressEvent) {}
            fn cancelled(&self) -> bool {
                true
            }
        }
        let p = Progress::default();
        assert!(p.check_cancelled().is_ok());
        p.cancel();
        assert!(matches!(
            p.check_cancelled(),
            Err(crate::MediaError::Cancelled)
        ));
        assert!(Progress::new(Arc::new(Stop)).is_cancelled());
    }
}
