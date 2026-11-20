//! Child processes that a job's cancel can stop.
//!
//! Renders spawn FFmpeg and Typst from deep inside the media crate. Rather than
//! threading a flag through every signature, a job installs its [`CancelFlag`]
//! for the current thread with [`with_cancel`], and [`output`] and [`status`]
//! kill their child as soon as the flag is set. With no flag installed they
//! behave exactly like the `Command` methods they replace.
//!
//! It is also the one place that decides *which* FFmpeg, ffprobe and Typst
//! executables run: see [`resolve_tool`] and [`Toolchain`].

use crate::jobs::CancelFlag;
use std::cell::RefCell;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Output, Stdio};
use std::sync::OnceLock;
use std::time::Duration;

thread_local! {
    static CURRENT: RefCell<Option<CancelFlag>> = const { RefCell::new(None) };
}

/// Longest wait between checks on a running child, and so the longest a
/// cancel takes to reach it.
const MAX_POLL: Duration = Duration::from_millis(50);

/// Run `f` with `flag` installed on this thread, restoring the previous flag
/// afterward. Threads spawned inside `f` must install it themselves; see
/// [`current`].
pub fn with_cancel<R>(flag: &CancelFlag, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<CancelFlag>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT.with(|c| *c.borrow_mut() = self.0.take());
        }
    }
    let _restore = Restore(CURRENT.with(|c| c.borrow_mut().replace(flag.clone())));
    f()
}

/// The flag installed on this thread, if any.
pub fn current() -> Option<CancelFlag> {
    CURRENT.with(|c| c.borrow().clone())
}

/// True when this thread's job has been cancelled.
pub fn cancelled() -> bool {
    current().is_some_and(|f| f.is_cancelled())
}

fn cancelled_error() -> io::Error {
    io::Error::new(io::ErrorKind::Interrupted, "cancelled")
}

/// Wait for `child`, killing it if `flag` is set first.
fn wait_or_kill(child: &mut Child, flag: &CancelFlag) -> io::Result<ExitStatus> {
    let mut poll = Duration::from_millis(1);
    loop {
        if flag.is_cancelled() {
            let _ = child.kill();
            let _ = child.wait();
            return Err(cancelled_error());
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(poll);
        poll = (poll * 2).min(MAX_POLL);
    }
}

/// [`Command::status`] that a cancel can interrupt.
pub fn status(cmd: &mut Command) -> io::Result<ExitStatus> {
    let Some(flag) = current() else {
        return cmd.status();
    };
    if flag.is_cancelled() {
        return Err(cancelled_error());
    }
    wait_or_kill(&mut cmd.spawn()?, &flag)
}

/// [`Command::output`] that a cancel can interrupt.
pub fn output(cmd: &mut Command) -> io::Result<Output> {
    let Some(flag) = current() else {
        return cmd.output();
    };
    if flag.is_cancelled() {
        return Err(cancelled_error());
    }
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    // Both pipes drain on their own threads: a child that fills one would
    // otherwise block forever and never exit.
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = vec![];
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|p| Box::new(p) as Box<dyn Read + Send>),
    );
    let status = wait_or_kill(&mut child, &flag);
    let stdout = stdout.join().unwrap_or_default();
    let stderr = stderr.join().unwrap_or_default();
    Ok(Output {
        status: status?,
        stdout,
        stderr,
    })
}

/// An external executable TripTape runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Ffmpeg,
    Ffprobe,
    Typst,
}

impl Tool {
    /// The executable's name, without the platform suffix (`.exe`).
    pub fn name(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "ffmpeg",
            Tool::Ffprobe => "ffprobe",
            Tool::Typst => "typst",
        }
    }

    /// Environment variable that overrides where the tool is found.
    pub fn env_var(self) -> &'static str {
        match self {
            Tool::Ffmpeg => "TRIPTAPE_FFMPEG",
            Tool::Ffprobe => "TRIPTAPE_FFPROBE",
            Tool::Typst => "TRIPTAPE_TYPST",
        }
    }

    fn file_name(self) -> String {
        format!("{}{}", self.name(), std::env::consts::EXE_SUFFIX)
    }
}

/// Where a resolved tool came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSource {
    /// A `--ffmpeg`-style flag or a `TRIPTAPE_*` variable named it.
    Override,
    /// The pinned sidecar bundled next to the executable.
    Sidecar,
    /// Found on `PATH`: a development fallback, never what a release ships.
    Path,
    /// Nowhere: running it will fail.
    Missing,
}

/// One tool, resolved to the program to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTool {
    pub tool: Tool,
    /// What to pass to [`Command::new`]: a full path, or the bare name when
    /// the tool is left to the `PATH` lookup.
    pub bin: String,
    pub source: ToolSource,
}

impl ResolvedTool {
    /// A sentence for the user when this tool is not the bundled or chosen one.
    pub fn note(&self) -> Option<String> {
        let name = self.tool.name();
        match self.source {
            ToolSource::Override | ToolSource::Sidecar => None,
            ToolSource::Path => Some(format!(
                "Using {name} from PATH: the copy bundled with TripTape is missing (development fallback)."
            )),
            ToolSource::Missing => Some(format!(
                "{name} was not found: it is not bundled with this build and not on PATH."
            )),
        }
    }
}

/// Resolve `tool`, first match wins:
///
/// 1. `explicit` (a command-line flag), then the tool's `TRIPTAPE_*`
///    variable. Either is used as given, even if it does not exist, so a
///    typo fails loudly instead of quietly picking another binary.
/// 2. The bundled sidecar next to the running executable. Tauri installs
///    `bundle.externalBin` there (`Contents/MacOS` in a macOS app, the
///    install folder on Windows), and its build script copies them into
///    `target/<profile>/` for dev runs and the CLI. A test or bench binary
///    in `target/<profile>/deps/` looks one level up.
/// 3. `PATH`, as a development fallback; [`ResolvedTool::note`] says so.
pub fn resolve_tool(tool: Tool, explicit: Option<&str>) -> ResolvedTool {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    resolve_tool_in(
        tool,
        explicit,
        std::env::var_os(tool.env_var()),
        exe_dir.as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

/// [`resolve_tool`] with its environment passed in, for tests.
fn resolve_tool_in(
    tool: Tool,
    explicit: Option<&str>,
    env: Option<OsString>,
    exe_dir: Option<&Path>,
    path: Option<&OsStr>,
) -> ResolvedTool {
    let resolved = |bin: String, source| ResolvedTool { tool, bin, source };
    if let Some(bin) = explicit.filter(|b| !b.is_empty()) {
        return resolved(bin.to_string(), ToolSource::Override);
    }
    if let Some(bin) = env
        .and_then(|v| v.into_string().ok())
        .filter(|b| !b.is_empty())
    {
        return resolved(bin, ToolSource::Override);
    }
    let file = tool.file_name();
    // A non-UTF-8 install path cannot travel as a `String`; PATH then.
    if let Some(bin) = exe_dir
        .and_then(|d| sidecar_in(d, &file))
        .and_then(|p| p.to_str().map(str::to_string))
    {
        return resolved(bin, ToolSource::Sidecar);
    }
    let source = if path.is_some_and(|p| on_path(p, &file)) {
        ToolSource::Path
    } else {
        ToolSource::Missing
    };
    resolved(tool.name().to_string(), source)
}

fn sidecar_in(exe_dir: &Path, file: &str) -> Option<PathBuf> {
    let here = exe_dir.join(file);
    if here.is_file() {
        return Some(here);
    }
    // Cargo puts test and bench binaries in `deps/`, one below the sidecars.
    if exe_dir.file_name().is_some_and(|n| n == "deps") {
        let up = exe_dir.parent()?.join(file);
        if up.is_file() {
            return Some(up);
        }
    }
    None
}

fn on_path(path: &OsStr, file: &str) -> bool {
    std::env::split_paths(path).any(|dir| dir.join(file).is_file())
}

/// FFmpeg, ffprobe and Typst, resolved together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toolchain {
    pub ffmpeg: ResolvedTool,
    pub ffprobe: ResolvedTool,
    pub typst: ResolvedTool,
}

impl Toolchain {
    /// Resolve all three; each `Some` is an explicit override (a CLI flag).
    pub fn resolve(ffmpeg: Option<&str>, ffprobe: Option<&str>, typst: Option<&str>) -> Self {
        Self {
            ffmpeg: resolve_tool(Tool::Ffmpeg, ffmpeg),
            ffprobe: resolve_tool(Tool::Ffprobe, ffprobe),
            typst: resolve_tool(Tool::Typst, typst),
        }
    }

    /// The process-wide toolchain (no flags; `TRIPTAPE_*` variables still
    /// apply), resolved once on first use.
    pub fn global() -> &'static Toolchain {
        static TOOLS: OnceLock<Toolchain> = OnceLock::new();
        TOOLS.get_or_init(|| Toolchain::resolve(None, None, None))
    }

    /// One note per tool that is not the bundled or chosen one.
    pub fn notes(&self) -> Vec<String> {
        [&self.ffmpeg, &self.ffprobe, &self.typst]
            .into_iter()
            .filter_map(ResolvedTool::note)
            .collect()
    }
}

#[cfg(test)]
mod resolve_tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, b"").unwrap();
    }

    fn resolve(explicit: Option<&str>, env: Option<&str>, exe: &Path, path: &Path) -> ResolvedTool {
        resolve_tool_in(
            Tool::Ffmpeg,
            explicit,
            env.map(OsString::from),
            Some(exe),
            Some(path.as_os_str()),
        )
    }

    #[test]
    fn order_is_flag_then_env_then_sidecar_then_path() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("app");
        let path = dir.path().join("bin");
        let file = Tool::Ffmpeg.file_name();
        touch(&exe.join(&file));
        touch(&path.join(&file));

        let r = resolve(Some("/x/ffmpeg"), Some("/y/ffmpeg"), &exe, &path);
        assert_eq!(
            (r.bin.as_str(), r.source),
            ("/x/ffmpeg", ToolSource::Override)
        );
        let r = resolve(None, Some("/y/ffmpeg"), &exe, &path);
        assert_eq!(
            (r.bin.as_str(), r.source),
            ("/y/ffmpeg", ToolSource::Override)
        );
        let r = resolve(Some(""), Some(""), &exe, &path);
        assert_eq!(r.source, ToolSource::Sidecar);
        assert_eq!(Path::new(&r.bin), exe.join(&file));
        assert_eq!(r.note(), None);

        std::fs::remove_file(exe.join(&file)).unwrap();
        let r = resolve(None, None, &exe, &path);
        assert_eq!((r.bin.as_str(), r.source), ("ffmpeg", ToolSource::Path));
        assert!(r.note().unwrap().contains("development fallback"));

        std::fs::remove_file(path.join(&file)).unwrap();
        let r = resolve(None, None, &exe, &path);
        assert_eq!(r.source, ToolSource::Missing);
        assert!(r.note().unwrap().contains("not found"));
    }

    #[test]
    fn a_test_binary_finds_the_sidecar_beside_deps() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("debug");
        let file = Tool::Typst.file_name();
        touch(&target.join(&file));
        let r = resolve_tool_in(Tool::Typst, None, None, Some(&target.join("deps")), None);
        assert_eq!(r.source, ToolSource::Sidecar);
        assert_eq!(Path::new(&r.bin), target.join(&file));
        // Only `deps` looks up: any other folder keeps to itself.
        let r = resolve_tool_in(Tool::Typst, None, None, Some(&target.join("other")), None);
        assert_eq!(r.source, ToolSource::Missing);
    }

    #[test]
    fn notes_name_only_the_fallbacks() {
        let tool = |tool, source| ResolvedTool {
            tool,
            bin: tool.name().to_string(),
            source,
        };
        let tools = Toolchain {
            ffmpeg: tool(Tool::Ffmpeg, ToolSource::Sidecar),
            ffprobe: tool(Tool::Ffprobe, ToolSource::Path),
            typst: tool(Tool::Typst, ToolSource::Missing),
        };
        let notes = tools.notes();
        assert_eq!(notes.len(), 2);
        assert!(notes[0].starts_with("Using ffprobe from PATH"));
        assert!(notes[1].starts_with("typst was not found"));
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::Instant;

    fn sh(script: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", script]);
        c
    }

    #[test]
    fn without_a_flag_commands_run_normally() {
        let out = output(&mut sh("echo hi")).unwrap();
        assert_eq!(out.stdout, b"hi\n");
        assert!(status(&mut sh("exit 3")).unwrap().code() == Some(3));
    }

    #[test]
    fn with_a_flag_output_is_still_captured() {
        let flag = CancelFlag::new();
        let out = with_cancel(&flag, || {
            // Larger than a pipe buffer, on both streams.
            output(&mut sh(
                "head -c 200000 /dev/zero; head -c 200000 /dev/zero >&2; exit 2",
            ))
        })
        .unwrap();
        assert_eq!(out.status.code(), Some(2));
        assert_eq!(out.stdout.len(), 200_000);
        assert_eq!(out.stderr.len(), 200_000);
    }

    #[test]
    fn cancel_kills_a_running_child_quickly() {
        let flag = CancelFlag::new();
        let canceller = flag.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            canceller.cancel();
        });
        let start = Instant::now();
        // `exec` so the shell itself is the sleeping child holding the pipes.
        let err = with_cancel(&flag, || output(&mut sh("exec sleep 30"))).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn a_cancelled_job_starts_nothing() {
        let flag = CancelFlag::new();
        flag.cancel();
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let script = format!("touch '{}'", marker.display());
        with_cancel(&flag, || {
            assert!(output(&mut sh(&script)).is_err());
            assert!(status(&mut sh(&script)).is_err());
        });
        assert!(!marker.exists());
    }

    #[test]
    fn the_flag_is_scoped_to_the_closure() {
        let flag = CancelFlag::new();
        flag.cancel();
        with_cancel(&flag, || assert!(cancelled()));
        assert!(!cancelled());
    }
}
