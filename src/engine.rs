//! Bounded resource-aware parallel subprocess engine for update-agents.
//!
//! Runs at most `RunOptions.jobs` worker threads; each job leases its
//! `ToolSpec.resource` across the before-version, update and after-version
//! phases, so equal nonempty resources never update concurrently. Children
//! run in their own process group with stdin null and stdout/stderr streamed
//! straight into private log files (no pipes, no unbounded buffers).
//! Cancellation and timeouts deliver SIGTERM to the whole process group and
//! escalate to SIGKILL after a grace period, then reap, even when the group
//! leader exits first while descendants survive. A `report.json` is
//! finalized in the run directory when the run ends.

use crate::model::{CommandSpec, Job, Preflight, RunOptions, RunState, Status, ToolSpec};
use serde::Serialize;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Scheduler and child-poll cadence: at most 10 Hz.
const POLL_INTERVAL: Duration = Duration::from_millis(100);
/// Hard wall-clock bound for a single version probe.
const VERSION_TIMEOUT: Duration = Duration::from_secs(15);
/// SIGTERM grace before SIGKILL escalation.
const TERM_GRACE: Duration = Duration::from_secs(5);
/// SIGKILL reap grace before giving up on an unkillable child.
const KILL_GRACE: Duration = Duration::from_secs(5);
/// Version output is captured on disk; at most this much is read back.
const MAX_VERSION_BYTES: usize = 64 * 1024;
const MAX_VERSION_CHARS: usize = 256;
const MAX_MESSAGE_CHARS: usize = 512;
/// Defensive cap so a caller cannot turn `tail` into an unbounded read.
const MAX_TAIL_BYTES: usize = 1024 * 1024;
/// Streaming failure-marker scan: read chunk size and the longest marker
/// the scan will verify. Longer configured markers fail verification
/// instead of being silently skipped; the catalogue rejects them at load.
const SCAN_CHUNK: usize = 64 * 1024;
const MAX_MARKER_BYTES: usize = 4096;

/// Shared scheduler state handed to every worker thread.
struct Engine {
    state: Arc<Mutex<RunState>>,
    cancel: Arc<AtomicBool>,
    options: RunOptions,
    started_unix: u64,
    started_iso: String,
}

/// Public run handle: the UI may snapshot `state` briefly and only ever
/// mutates `cancel`; the scheduler thread is private and drained by `wait`.
pub struct RunHandle {
    pub state: Arc<Mutex<RunState>>,
    pub cancel: Arc<AtomicBool>,
    join: Option<JoinHandle<io::Result<()>>>,
}

impl RunHandle {
    /// Joins the scheduler, then propagates engine failure (report writing,
    /// scheduler or worker panics) to the caller.
    pub fn wait(&mut self) -> io::Result<()> {
        match self.join.take() {
            Some(join) => match join.join() {
                Ok(result) => result,
                Err(_) => Err(io::Error::other("engine scheduler panicked")),
            },
            None => Ok(()),
        }
    }
}

/// Creates the private run directory, maps non-Ready preflight jobs to
/// their terminal skipped/blocked status without execution, and launches
/// the bounded worker pool.
pub fn start(specs: Vec<ToolSpec>, options: RunOptions) -> io::Result<RunHandle> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&options.run_dir)?;
    let jobs: Vec<Job> = specs
        .into_iter()
        .map(|spec| {
            let log = options
                .run_dir
                .join(format!("{}.log", file_component(&spec.id)));
            let mut job = Job {
                spec,
                status: Status::Queued,
                started: None,
                elapsed: Duration::ZERO,
                before: String::new(),
                after: String::new(),
                message: String::new(),
                log,
                exit_code: None,
            };
            match &job.spec.preflight {
                Preflight::Ready => {}
                Preflight::Skipped(reason) => {
                    job.status = Status::Skipped;
                    job.message = clip(sanitize_terminal(reason), MAX_MESSAGE_CHARS);
                }
                Preflight::Blocked(reason) => {
                    job.status = Status::Blocked;
                    job.message = clip(sanitize_terminal(reason), MAX_MESSAGE_CHARS);
                }
            }
            job
        })
        .collect();
    let state = Arc::new(Mutex::new(RunState {
        jobs,
        started: Instant::now(),
        done: false,
        run_dir: options.run_dir.clone(),
    }));
    let cancel = Arc::new(AtomicBool::new(false));
    let now_unix = unix_now();
    let engine = Arc::new(Engine {
        state: Arc::clone(&state),
        cancel: Arc::clone(&cancel),
        options,
        started_unix: now_unix,
        started_iso: rfc3339_utc(now_unix),
    });
    let join = thread::Builder::new()
        .name("update-agents-engine".to_string())
        .spawn(move || schedule(engine))?;
    Ok(RunHandle {
        state,
        cancel,
        join: Some(join),
    })
}

fn schedule(engine: Arc<Engine>) -> io::Result<()> {
    let workers = {
        let state = lock_state(&engine.state);
        engine.options.jobs.max(1).min(state.jobs.len())
    };
    let mut handles: Vec<JoinHandle<()>> = Vec::with_capacity(workers);
    let mut spawn_error: Option<io::Error> = None;
    for index in 0..workers {
        let worker_engine = Arc::clone(&engine);
        match thread::Builder::new()
            .name(format!("ua-worker-{index}"))
            .spawn(move || worker_loop(worker_engine))
        {
            Ok(handle) => handles.push(handle),
            Err(error) => {
                spawn_error = Some(error);
                break;
            }
        }
    }
    if spawn_error.is_some() {
        engine.cancel.store(true, Ordering::Relaxed);
    }
    let mut worker_failure = false;
    for handle in handles {
        if handle.join().is_err() {
            worker_failure = true;
        }
    }
    let finish_result = finish(&engine);
    if let Some(error) = spawn_error {
        return Err(error);
    }
    if worker_failure {
        return Err(io::Error::other("engine worker panicked"));
    }
    finish_result
}

fn worker_loop(engine: Arc<Engine>) {
    loop {
        if let Some(index) = claim(&engine) {
            let spec = {
                let state = lock_state(&engine.state);
                state.jobs[index].spec.clone()
            };
            let job_engine = Arc::clone(&engine);
            let attempted = catch_unwind(AssertUnwindSafe(|| run_job(&job_engine, &spec)));
            let result = match attempted {
                Ok(result) => result,
                Err(_) => outcome(
                    Status::Failed,
                    "internal error: worker panic",
                    &[],
                    "",
                    "",
                    None,
                ),
            };
            finalize(&engine, index, result);
            continue;
        }
        if drain_or_wait(&engine) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Claims the next runnable job under the state lock: a queued job whose
/// nonempty resource is not leased by a currently running job. Jobs whose
/// preflight is not Ready are terminal from initialization and never
/// queued.
fn claim(engine: &Engine) -> Option<usize> {
    let mut state = lock_state(&engine.state);
    if engine.cancel.load(Ordering::Relaxed) {
        return None;
    }
    let mut chosen: Option<usize> = None;
    for (index, job) in state.jobs.iter().enumerate() {
        if job.status != Status::Queued {
            continue;
        }
        let resource = &job.spec.resource;
        if !resource.is_empty()
            && state
                .jobs
                .iter()
                .any(|other| other.status == Status::Running && &other.spec.resource == resource)
        {
            continue;
        }
        chosen = Some(index);
        break;
    }
    if let Some(index) = chosen {
        let job = &mut state.jobs[index];
        job.status = Status::Running;
        job.started = Some(Instant::now());
    }
    chosen
}

/// With cancellation set and no job running, cancels every queued job.
/// Returns true once every job has reached a terminal status.
fn drain_or_wait(engine: &Engine) -> bool {
    let mut state = lock_state(&engine.state);
    if engine.cancel.load(Ordering::Relaxed)
        && !state.jobs.iter().any(|job| job.status == Status::Running)
    {
        for job in state.jobs.iter_mut() {
            if job.status == Status::Queued {
                job.status = Status::Cancelled;
                if job.message.is_empty() {
                    job.message = "cancelled before start".to_string();
                }
            }
        }
    }
    state
        .jobs
        .iter()
        .all(|job| !matches!(job.status, Status::Queued | Status::Running))
}

fn finalize(engine: &Engine, index: usize, result: Outcome) {
    let mut state = lock_state(&engine.state);
    if let Some(job) = state.jobs.get_mut(index) {
        job.status = result.status;
        job.before = result.before;
        job.after = result.after;
        job.message = result.message;
        job.exit_code = result.exit_code;
        job.elapsed = job.started.map(|s| s.elapsed()).unwrap_or_default();
    }
}

/// Final safety sweep, then marks the run done and writes report.json.
fn finish(engine: &Engine) -> io::Result<()> {
    {
        let mut state = lock_state(&engine.state);
        for job in state.jobs.iter_mut() {
            match job.status {
                Status::Queued => {
                    job.status = Status::Cancelled;
                    if job.message.is_empty() {
                        job.message = "cancelled before start".to_string();
                    }
                }
                Status::Running => {
                    job.status = Status::Failed;
                    if job.message.is_empty() {
                        job.message = "internal error: worker lost".to_string();
                    }
                    job.elapsed = job.started.map(|s| s.elapsed()).unwrap_or_default();
                }
                _ => {}
            }
        }
        state.done = true;
    }
    write_report(engine)
}

/// Outcome of one job, applied to the state under the lock at finalize time.
struct Outcome {
    status: Status,
    message: String,
    before: String,
    after: String,
    exit_code: Option<i32>,
}

fn outcome(
    status: Status,
    primary: &str,
    notes: &[String],
    before: &str,
    after: &str,
    exit_code: Option<i32>,
) -> Outcome {
    let mut parts: Vec<String> = Vec::with_capacity(notes.len() + 1);
    if !primary.is_empty() {
        parts.push(primary.to_string());
    }
    parts.extend(notes.iter().cloned());
    let joined = parts.join("; ");
    Outcome {
        status,
        message: clip(sanitize_terminal(&joined), MAX_MESSAGE_CHARS),
        before: before.to_string(),
        after: after.to_string(),
        exit_code,
    }
}

enum PhaseEnd {
    Exit(ExitStatus),
    TimedOut,
    Cancelled,
}

/// Runs one job end to end: before-version, update, after-version. The
/// resource lease is held by the Running status across all three phases.
fn run_job(engine: &Engine, spec: &ToolSpec) -> Outcome {
    let component = file_component(&spec.id);
    let log_path = engine.options.run_dir.join(format!("{component}.log"));
    write_log_header(&log_path, spec);
    let mut notes: Vec<String> = Vec::new();
    let mut before = String::new();
    let mut after = String::new();

    if let Some(version_cmd) = &spec.version {
        let capture = engine
            .options
            .run_dir
            .join(format!("{component}.before.log"));
        if collect_version(
            engine,
            version_cmd,
            spec.version_line,
            &capture,
            "before",
            &mut notes,
            &mut before,
        ) {
            return outcome(
                Status::Cancelled,
                "cancelled",
                &notes,
                &before,
                &after,
                None,
            );
        }
    }
    if engine.cancel.load(Ordering::Relaxed) {
        return outcome(
            Status::Cancelled,
            "cancelled",
            &notes,
            &before,
            &after,
            None,
        );
    }

    let mut status = Status::Succeeded;
    let mut primary = String::new();
    let mut exit_code: Option<i32> = None;
    let mut abort = false;
    match run_phase(
        &spec.update,
        engine.options.timeout,
        &engine.cancel,
        &log_path,
    ) {
        Err(error) => {
            status = Status::Failed;
            primary = format!("failed to start {}: {error}", spec.update.program);
            abort = true;
        }
        Ok(PhaseEnd::Exit(exit_status)) => {
            exit_code = exit_status.code();
            if !exit_status.success() {
                status = Status::Failed;
                primary = format!("update failed ({})", describe_exit(&exit_status));
                append_excerpt(&mut primary, &log_path);
                abort = true;
            } else {
                match scan_log_for_markers(&log_path, &spec.failure_contains) {
                    Ok(Some(marker)) => {
                        // False success: the tool exited 0 while its own
                        // output declares failure. Exit code stays truthful
                        // (Some(0)); the job is still a failure.
                        status = Status::Failed;
                        primary =
                            format!("update matched failure marker {marker:?} despite exit 0");
                        append_excerpt(&mut primary, &log_path);
                        abort = true;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        // The configured failure scan could not be
                        // completed, so the update must not be reported as
                        // a verified success. Exit code stays truthful.
                        status = Status::Failed;
                        primary = format!("failure-marker scan could not verify the log: {error}");
                        append_excerpt(&mut primary, &log_path);
                        abort = true;
                    }
                }
            }
        }
        Ok(PhaseEnd::TimedOut) => {
            status = Status::TimedOut;
            primary = format!(
                "timed out after {}s (terminated, then killed)",
                engine.options.timeout.as_secs()
            );
            append_excerpt(&mut primary, &log_path);
            abort = true;
        }
        Ok(PhaseEnd::Cancelled) => {
            status = Status::Cancelled;
            primary = "cancelled".to_string();
            abort = true;
        }
    }

    if !abort {
        if engine.cancel.load(Ordering::Relaxed) {
            notes.push("after version skipped (run cancelled)".to_string());
        } else if let Some(version_cmd) = &spec.version {
            let capture = engine
                .options
                .run_dir
                .join(format!("{component}.after.log"));
            if collect_version(
                engine,
                version_cmd,
                spec.version_line,
                &capture,
                "after",
                &mut notes,
                &mut after,
            ) {
                notes.push("after version cancelled".to_string());
            }
        }
    }
    outcome(status, &primary, &notes, &before, &after, exit_code)
}

fn append_excerpt(primary: &mut String, log_path: &Path) {
    let excerpt = log_excerpt(log_path);
    if !excerpt.is_empty() {
        primary.push_str(": ");
        primary.push_str(&excerpt);
    }
}

/// Runs one version probe into `capture` and parses `version_line`.
/// Returns true when the run was cancelled mid-probe.
fn collect_version(
    engine: &Engine,
    cmd: &CommandSpec,
    version_line: usize,
    capture: &Path,
    label: &str,
    notes: &mut Vec<String>,
    dest: &mut String,
) -> bool {
    match run_phase(cmd, VERSION_TIMEOUT, &engine.cancel, capture) {
        Ok(PhaseEnd::Exit(exit_status)) if exit_status.success() => {
            match extract_version(capture, version_line) {
                Ok(version) => *dest = version,
                Err(error) => notes.push(format!("{label} version: {error}")),
            }
        }
        Ok(PhaseEnd::Exit(exit_status)) => {
            notes.push(format!(
                "{label} version failed ({})",
                describe_exit(&exit_status)
            ));
        }
        Ok(PhaseEnd::TimedOut) => notes.push(format!(
            "{label} version timed out after {}s",
            VERSION_TIMEOUT.as_secs()
        )),
        Ok(PhaseEnd::Cancelled) => return true,
        Err(error) => notes.push(format!("{label} version: {error}")),
    }
    false
}

/// Bounded streaming scan of a completed log for case-sensitive literal
/// failure markers. Reads fixed-size chunks and carries the raw overlap of
/// `max marker length - 1` bytes so a marker spanning a chunk boundary is
/// still matched; memory stays bounded regardless of log size. Returns the
/// first matched marker, or an error when verification could not be
/// completed: an unreadable log or an oversized marker must fail the job
/// instead of masquerading as a clean log.
fn scan_log_for_markers(path: &Path, markers: &[String]) -> io::Result<Option<String>> {
    let live: Vec<&str> = markers
        .iter()
        .map(String::as_str)
        .filter(|m| !m.is_empty())
        .collect();
    for marker in &live {
        if marker.len() > MAX_MARKER_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "failure marker of {} bytes exceeds the {MAX_MARKER_BYTES}-byte scan bound",
                    marker.len()
                ),
            ));
        }
    }
    let Some(max_len) = live.iter().map(|m| m.len()).max() else {
        return Ok(None);
    };
    let keep = max_len - 1;
    let mut file = File::open(path)?;
    let mut carried: Vec<u8> = Vec::with_capacity(SCAN_CHUNK + keep);
    let mut chunk = vec![0u8; SCAN_CHUNK];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                carried.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&carried);
                for marker in &live {
                    if text.contains(marker) {
                        return Ok(Some((*marker).to_string()));
                    }
                }
                if carried.len() > keep {
                    let start = carried.len() - keep;
                    carried.drain(..start);
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(None)
}

/// Spawns one command phase in its own process group, stdin null, output
/// streamed to `out_path`, and polls exit against the deadline and the
/// cancellation flag.
fn run_phase(
    cmd: &CommandSpec,
    timeout: Duration,
    cancel: &AtomicBool,
    out_path: &Path,
) -> io::Result<PhaseEnd> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(out_path)?;
    let mut command = build_command(cmd, &file)?;
    let mut child = command.spawn()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(exit_status) = child.try_wait()? {
            return Ok(PhaseEnd::Exit(exit_status));
        }
        if cancel.load(Ordering::Relaxed) {
            terminate(&mut child);
            return Ok(PhaseEnd::Cancelled);
        }
        if Instant::now() >= deadline {
            terminate(&mut child);
            return Ok(PhaseEnd::TimedOut);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// SIGTERM the whole process group, escalate to SIGKILL after the grace
/// period, and reap the direct child. Bounded at every step. The TERM grace
/// belongs to the whole group: a leader that exits on the first signal does
/// not end it while descendants are still running. The child stays unreaped
/// until every group signal has been delivered: an unreaped child pins its
/// pid, so `-pid` can never reach a recycled, unrelated process group.
fn terminate(child: &mut Child) {
    let pid = child.id() as i32;
    if pid > 0 {
        unsafe {
            libc::kill(-pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + TERM_GRACE;
        while Instant::now() < deadline && group_alive(pid) {
            thread::sleep(POLL_INTERVAL);
        }
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
        let kill_deadline = Instant::now() + KILL_GRACE;
        while Instant::now() < kill_deadline && group_alive(pid) {
            thread::sleep(Duration::from_millis(20));
        }
    }
    let reap_deadline = Instant::now() + KILL_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        if Instant::now() >= reap_deadline {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Parses the state and process-group id out of a /proc/<pid>/stat buffer.
/// comm (field 2) may contain spaces and parentheses, so parsing resumes
/// after its final ')'. Returns None when the buffer cannot be parsed.
fn stat_state_and_pgrp(stat: &[u8]) -> Option<(char, i32)> {
    let comm_end = stat.iter().rposition(|&byte| byte == b')')?;
    let rest = std::str::from_utf8(&stat[comm_end + 1..]).ok()?;
    let mut fields = rest.split_ascii_whitespace();
    let state = fields.next()?.chars().next()?;
    let _ppid = fields.next()?;
    let pgrp = fields.next()?.parse::<i32>().ok()?;
    Some((state, pgrp))
}

/// True while the process group `pgid` still has a live member. Exited
/// processes lingering as unreaped zombies do not count: they are already
/// dead. The scan reads /proc; when /proc cannot be consulted at all the
/// group is reported alive so callers fall back to the full time-bounded
/// grace instead of escalating early.
fn group_alive(pgid: i32) -> bool {
    let Ok(entries) = fs::read_dir("/proc") else {
        return true;
    };
    let mut scanned = 0usize;
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().parse::<i32>().is_err() {
            continue; // /proc also holds non-pid entries (self, sys, ...)
        }
        let Ok(stat) = fs::read(entry.path().join("stat")) else {
            continue; // exited between listing and read, or unreadable
        };
        scanned += 1;
        if let Some((state, pgrp)) = stat_state_and_pgrp(&stat) {
            if pgrp == pgid && state != 'Z' {
                return true;
            }
        }
    }
    scanned == 0
}

fn build_command(cmd: &CommandSpec, out: &File) -> io::Result<Command> {
    let program: PathBuf = {
        let raw = Path::new(&cmd.program);
        if raw.is_absolute() || !cmd.program.contains('/') {
            PathBuf::from(&cmd.program)
        } else {
            // Relative path with a separator: resolve against our current
            // directory before the child switches to the neutral cwd.
            std::env::current_dir()?.join(raw)
        }
    };
    let stdout_file = out.try_clone()?;
    let stderr_file = out.try_clone()?;
    let mut command = Command::new(program);
    command
        .args(&cmd.args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file))
        .stderr(Stdio::from(stderr_file))
        .current_dir(neutral_cwd());
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) != 0 {
                return Err(io::Error::last_os_error());
            }
            for signal in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP, libc::SIGPIPE] {
                libc::signal(signal, libc::SIG_DFL as libc::sighandler_t);
            }
            Ok(())
        });
    }
    Ok(command)
}

/// Updates must observe a home-neutral cwd, never the user's project.
fn neutral_cwd() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) if !home.is_empty() => PathBuf::from(home),
        _ => PathBuf::from("/"),
    }
}

fn write_log_header(path: &Path, spec: &ToolSpec) {
    if let Ok(mut file) = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        // Deliberately no argv echo: exact argv lives in report.json, and
        // echoing arbitrary argument text could false-positive the
        // failure_contains scan on the engine's own header line.
        let _ = writeln!(
            file,
            "# update-agents job {} (program {})",
            spec.id, spec.update.program
        );
    }
}

/// Parses the configured zero-based NONEMPTY line index from a captured
/// version output. Never fabricates: missing index is an error.
fn extract_version(path: &Path, version_line: usize) -> Result<String, String> {
    let bytes = read_bounded(path, MAX_VERSION_BYTES)
        .map_err(|error| format!("output unreadable ({error})"))?;
    let text = String::from_utf8_lossy(&bytes);
    let line = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .nth(version_line);
    match line {
        Some(line) => Ok(clip(sanitize_terminal(line), MAX_VERSION_CHARS)),
        None => Err(format!("no nonempty output line at index {version_line}")),
    }
}

fn read_bounded(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let mut buf: Vec<u8> = Vec::with_capacity(max.min(8192));
    let mut chunk = [0u8; 4096];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(max - buf.len());
                buf.extend_from_slice(&chunk[..take]);
                if buf.len() >= max {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(buf)
}

/// Returns at most the last `max_bytes` of the file as sanitized UTF-8
/// text. Unreadable or missing files yield an empty string; invalid UTF-8
/// becomes replacement characters with the lead byte resynchronized.
pub fn tail(path: &Path, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let max_bytes = max_bytes.min(MAX_TAIL_BYTES);
    let Ok(mut file) = File::open(path) else {
        return String::new();
    };
    let len = file.metadata().map(|meta| meta.len()).unwrap_or(0);
    let start = len.saturating_sub(max_bytes as u64);
    if start > 0 {
        let _ = file.seek(SeekFrom::Start(start));
    }
    let mut buf: Vec<u8> = Vec::with_capacity(max_bytes.min(65536));
    let mut chunk = [0u8; 8192];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(max_bytes - buf.len());
                buf.extend_from_slice(&chunk[..take]);
                if buf.len() >= max_bytes {
                    break;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let mut first = 0usize;
    while first < buf.len() && first < 4 && (buf[first] & 0b1100_0000) == 0b1000_0000 {
        first += 1;
    }
    sanitize_terminal(&String::from_utf8_lossy(&buf[first..]))
}

/// Strips ANSI/OSC/DCS escape sequences, drops other control characters and
/// converts lone CR to newline so progress redraws stay readable.
fn sanitize_terminal(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut text_start = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            0x1B => {
                if text_start < i {
                    out.push_str(&input[text_start..i]);
                }
                i = skip_escape_sequence(bytes, i);
                text_start = i;
            }
            b'\n' => {
                if text_start < i {
                    out.push_str(&input[text_start..i]);
                }
                out.push('\n');
                i += 1;
                text_start = i;
            }
            b'\r' => {
                if text_start < i {
                    out.push_str(&input[text_start..i]);
                }
                if bytes.get(i + 1) != Some(&b'\n') {
                    out.push('\n');
                }
                i += 1;
                text_start = i;
            }
            b'\t' => {
                if text_start < i {
                    out.push_str(&input[text_start..i]);
                }
                out.push('\t');
                i += 1;
                text_start = i;
            }
            0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F => {
                if text_start < i {
                    out.push_str(&input[text_start..i]);
                }
                i += 1;
                text_start = i;
            }
            _ => i += 1,
        }
    }
    if text_start < bytes.len() {
        out.push_str(&input[text_start..]);
    }
    out
}

/// Returns the index just past the escape sequence starting at `esc`.
fn skip_escape_sequence(bytes: &[u8], esc: usize) -> usize {
    let mut i = esc + 1;
    if i >= bytes.len() {
        return bytes.len();
    }
    match bytes[i] {
        b'[' => {
            i += 1;
            while i < bytes.len() && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            if i < bytes.len() {
                i += 1;
            }
            i
        }
        b']' | b'P' | b'X' | b'^' | b'_' => {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    0x07 => return i + 1,
                    0x1B => {
                        if bytes.get(i + 1) == Some(&b'\\') {
                            return i + 2;
                        }
                        return i;
                    }
                    _ => i += 1,
                }
            }
            i
        }
        _ => i + 1,
    }
}

/// Char-boundary-safe truncation with an ASCII ellipsis.
fn clip(s: String, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s;
    }
    let keep = max_chars.saturating_sub(3).max(1);
    let mut clipped: String = s.chars().take(keep).collect();
    clipped.push_str("...");
    clipped
}

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Bounded last-meaningful-line excerpt of a job log for messages.
fn log_excerpt(path: &Path) -> String {
    let text = tail(path, 512);
    let last = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    clip(collapse_whitespace(last), 160)
}

fn describe_exit(exit_status: &ExitStatus) -> String {
    if let Some(code) = exit_status.code() {
        return format!("exit {code}");
    }
    if let Some(sig) = exit_status.signal() {
        return format!("signal {sig}");
    }
    "unknown exit".to_string()
}

/// RFC 3339 UTC timestamp from Unix seconds (Howard Hinnant's civil date
/// algorithm); avoids a calendar dependency.
fn rfc3339_utc(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let rem = unix_secs % 86_400;
    let hour = rem / 3600;
    let minute = (rem % 3600) / 60;
    let second = rem % 60;
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Log file component for an id: catalog guarantees path-safe ids, this is
/// only defense in depth against path traversal in foreign descriptors.
fn file_component(id: &str) -> String {
    let component: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if component.is_empty() {
        "job".to_string()
    } else {
        component
    }
}

fn lock_state(state: &Mutex<RunState>) -> MutexGuard<'_, RunState> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Serialize)]
struct RunReport {
    schema_version: u8,
    started: String,
    finished: String,
    total_secs: u64,
    counts: ReportCounts,
    jobs: Vec<ReportJob>,
}

#[derive(Serialize)]
struct ReportCounts {
    total: usize,
    succeeded: usize,
    failed: usize,
    blocked: usize,
    skipped: usize,
    timed_out: usize,
    cancelled: usize,
    unfinished: usize,
}

#[derive(Serialize)]
struct ReportJob {
    id: String,
    label: String,
    status: Status,
    resource: String,
    update_argv: Vec<String>,
    version_argv: Option<Vec<String>>,
    version_line: usize,
    failure_contains: Vec<String>,
    before: String,
    after: String,
    exit_code: Option<i32>,
    elapsed_secs: f64,
    message: String,
    log: String,
    before_log: Option<String>,
    after_log: Option<String>,
}

/// Finalizes report.json atomically (tmp file + rename) with every record,
/// including failed, blocked, cancelled and timed-out jobs.
fn write_report(engine: &Engine) -> io::Result<()> {
    let snapshot = lock_state(&engine.state).clone();
    let finished_unix = unix_now();
    let mut counts = ReportCounts {
        total: snapshot.jobs.len(),
        succeeded: 0,
        failed: 0,
        blocked: 0,
        skipped: 0,
        timed_out: 0,
        cancelled: 0,
        unfinished: 0,
    };
    let mut jobs = Vec::with_capacity(snapshot.jobs.len());
    for job in &snapshot.jobs {
        match job.status {
            Status::Succeeded => counts.succeeded += 1,
            Status::Failed => counts.failed += 1,
            Status::Blocked => counts.blocked += 1,
            Status::Skipped => counts.skipped += 1,
            Status::TimedOut => counts.timed_out += 1,
            Status::Cancelled => counts.cancelled += 1,
            Status::Running | Status::Queued => counts.unfinished += 1,
        }
        let component = file_component(&job.spec.id);
        jobs.push(ReportJob {
            id: job.spec.id.clone(),
            label: job.spec.label.clone(),
            status: job.status,
            resource: job.spec.resource.clone(),
            update_argv: argv_of(&job.spec.update),
            version_argv: job.spec.version.as_ref().map(argv_of),
            version_line: job.spec.version_line,
            failure_contains: job.spec.failure_contains.clone(),
            before: job.before.clone(),
            after: job.after.clone(),
            exit_code: job.exit_code,
            elapsed_secs: job.elapsed.as_secs_f64(),
            message: job.message.clone(),
            log: job.log.display().to_string(),
            before_log: job.spec.version.as_ref().map(|_| {
                engine
                    .options
                    .run_dir
                    .join(format!("{component}.before.log"))
                    .display()
                    .to_string()
            }),
            after_log: job.spec.version.as_ref().map(|_| {
                engine
                    .options
                    .run_dir
                    .join(format!("{component}.after.log"))
                    .display()
                    .to_string()
            }),
        });
    }
    let report = RunReport {
        schema_version: 1,
        started: engine.started_iso.clone(),
        finished: rfc3339_utc(finished_unix),
        total_secs: finished_unix.saturating_sub(engine.started_unix),
        counts,
        jobs,
    };
    let json = serde_json::to_string_pretty(&report)
        .map_err(|error| io::Error::other(format!("report serialization failed: {error}")))?;
    let tmp = snapshot.run_dir.join("report.json.tmp");
    fs::write(&tmp, json)?;
    fs::rename(&tmp, snapshot.run_dir.join("report.json"))
}

fn argv_of(cmd: &CommandSpec) -> Vec<String> {
    let mut argv = Vec::with_capacity(cmd.args.len() + 1);
    argv.push(cmd.program.clone());
    argv.extend(cmd.args.iter().cloned());
    argv
}

/// Plain text summary: one line per tool, counts, and the log location.
pub fn summary(state: &RunState) -> String {
    let mut out = String::new();
    let id_width = state
        .jobs
        .iter()
        .map(|job| job.spec.id.chars().count())
        .max()
        .unwrap_or(0)
        .clamp(2, 24);
    for job in &state.jobs {
        let id = clip(job.spec.id.clone(), id_width);
        let status_word = match job.status {
            Status::Succeeded => "succeeded",
            Status::Failed => "failed",
            Status::Blocked => "blocked",
            Status::Skipped => "skipped",
            Status::Cancelled => "cancelled",
            Status::TimedOut => "timed out",
            Status::Running => "running",
            Status::Queued => "queued",
        };
        let mut detail = if !job.before.is_empty() && !job.after.is_empty() {
            format!("{} -> {}", job.before, job.after)
        } else if !job.message.is_empty() {
            clip(collapse_whitespace(&job.message), 120)
        } else if !job.before.is_empty() {
            format!("{} -> (not collected)", job.before)
        } else if !job.after.is_empty() {
            format!("(not collected) -> {}", job.after)
        } else {
            "-".to_string()
        };
        if job.elapsed > Duration::ZERO
            && !matches!(
                job.status,
                Status::Queued | Status::Blocked | Status::Skipped
            )
        {
            detail.push_str(&format!(" ({:.1}s)", job.elapsed.as_secs_f64()));
        }
        out.push_str(&format!("{id:<id_width$}  {status_word:<9}  {detail}\n"));
    }
    let count = |want: Status| state.jobs.iter().filter(|job| job.status == want).count();
    out.push_str(&format!(
        "\nsucceeded {}  failed {}  blocked {}  skipped {}  timed out {}  cancelled {}  ({} tools)\n",
        count(Status::Succeeded),
        count(Status::Failed),
        count(Status::Blocked),
        count(Status::Skipped),
        count(Status::TimedOut),
        count(Status::Cancelled),
        state.jobs.len()
    ));
    out.push_str(&format!(
        "report: {}\n",
        state.run_dir.join("report.json").display()
    ));
    out.push_str(&format!("logs: {}\n", state.run_dir.display()));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;

    static DIR_SEQ: AtomicU32 = AtomicU32::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "update-agents-engtest-{}-{}-{}",
            tag,
            std::process::id(),
            DIR_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn cmd(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn spec(id: &str, resource: &str, update_script: &str) -> ToolSpec {
        ToolSpec {
            id: id.to_string(),
            label: id.to_string(),
            update: cmd("/bin/sh", &["-c", update_script]),
            version: None,
            resource: resource.to_string(),
            version_line: 0,
            failure_contains: Vec::new(),
            preflight: Preflight::Ready,
        }
    }

    fn read_lines(path: &Path) -> Vec<String> {
        fs::read_to_string(path)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(50));
        }
    }

    /// True while any process on the system has `marker` in its argv.
    fn argv_alive(marker: &str) -> bool {
        let Ok(entries) = fs::read_dir("/proc") else {
            return false;
        };
        for entry in entries.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.bytes().all(|b| b.is_ascii_digit()) {
                continue;
            }
            let Ok(bytes) = fs::read(entry.path().join("cmdline")) else {
                continue;
            };
            let cmdline = String::from_utf8_lossy(&bytes).replace('\0', " ");
            if cmdline.contains(marker) {
                return true;
            }
        }
        false
    }

    /// True while `pid` still names a live (non-zombie) process: the exact
    /// condition cancellation must guarantee for group survivors.
    fn pid_alive(pid: i32) -> bool {
        match fs::read(format!("/proc/{pid}/stat")) {
            Ok(stat) => matches!(stat_state_and_pgrp(&stat), Some((state, _)) if state != 'Z'),
            Err(_) => false,
        }
    }

    #[test]
    fn same_resource_serializes() {
        let dir = temp_dir("serial");
        let probe = dir.join("probe.txt");
        let script = format!(
            "echo b >> '{}'; sleep 0.6; echo e >> '{}'",
            probe.display(),
            probe.display()
        );
        let specs = vec![spec("a", "shared", &script), spec("b", "shared", &script)];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let lines = read_lines(&probe);
        assert_eq!(
            lines,
            vec!["b", "e", "b", "e"],
            "shared resource overlapped: {lines:?}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn independent_resources_overlap() {
        let dir = temp_dir("overlap");
        let probe = dir.join("probe.txt");
        let script = format!(
            "echo b >> '{}'; sleep 1; echo e >> '{}'",
            probe.display(),
            probe.display()
        );
        let specs = vec![spec("a", "ra", &script), spec("b", "rb", &script)];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let lines = read_lines(&probe);
        assert_eq!(lines.len(), 4);
        assert_eq!(lines[0], "b");
        assert_eq!(lines[1], "b", "independent resources serialized: {lines:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn versions_follow_version_line() {
        let dir = temp_dir("versions");
        let mut one = spec("v", "vres", "echo updated");
        one.version = Some(cmd("/bin/sh", &["-c", "echo meta; echo 1.2.3"]));
        one.version_line = 1;
        let mut handle = start(
            vec![one],
            RunOptions {
                jobs: 1,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let state = lock_state(&handle.state);
        let job = &state.jobs[0];
        assert_eq!(job.status, Status::Succeeded);
        assert_eq!(job.before, "1.2.3", "second nonempty line must be picked");
        assert_eq!(job.after, "1.2.3");
        assert_eq!(job.exit_code, Some(0));
        drop(state);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn timeout_kills_process_group() {
        let dir = temp_dir("timeout");
        let specs = vec![spec("slow", "sres", "sleep 8888881 & wait")];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 1,
                timeout: Duration::from_secs(1),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        let began = Instant::now();
        handle.wait().unwrap();
        assert!(began.elapsed() < Duration::from_secs(15));
        {
            let state = lock_state(&handle.state);
            let job = &state.jobs[0];
            assert_eq!(job.status, Status::TimedOut);
            assert_eq!(job.exit_code, None);
        }
        assert!(wait_until(Duration::from_secs(8), || !argv_alive(
            "sleep 8888881"
        )));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn cancel_stops_running_and_queued() {
        let dir = temp_dir("cancel");
        let probe = dir.join("probe.txt");
        let running_script = format!("echo b >> '{}'; sleep 7777772 & wait", probe.display());
        let queued_script = format!("echo b >> '{}'; sleep 7777772", probe.display());
        let specs = vec![
            spec("long", "lres", &running_script),
            spec("queued", "qres", &queued_script),
        ];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 1,
                timeout: Duration::from_secs(60),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        thread::sleep(Duration::from_millis(300));
        handle.cancel.store(true, Ordering::Relaxed);
        handle.wait().unwrap();
        {
            let state = lock_state(&handle.state);
            assert_eq!(state.jobs[0].status, Status::Cancelled);
            assert_eq!(state.jobs[1].status, Status::Cancelled);
            assert_eq!(state.jobs[1].elapsed, Duration::ZERO);
            assert!(state.done);
        }
        let started = fs::read_to_string(&probe).unwrap_or_default();
        assert_eq!(
            started.lines().filter(|line| *line == "b").count(),
            1,
            "queued job must never start"
        );
        assert!(wait_until(Duration::from_secs(8), || !argv_alive(
            "sleep 7777772"
        )));
        let report = fs::read_to_string(dir.join("run").join("report.json")).unwrap();
        assert!(
            report.contains("\"cancelled\""),
            "cancelled records are persisted"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn spawn_failure_is_persisted() {
        let dir = temp_dir("spawnfail");
        let mut missing = spec("missing", "mres", "echo hi");
        missing.update = cmd("/nonexistent/update-agents-missing-binary", &[]);
        let specs = vec![missing, spec("ok", "ores", "echo fine")];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        {
            let state = lock_state(&handle.state);
            assert_eq!(state.jobs[0].status, Status::Failed);
            assert_eq!(state.jobs[0].exit_code, None);
            assert!(state.jobs[0].message.contains("failed to start"));
            assert_eq!(state.jobs[1].status, Status::Succeeded);
        }
        let report = fs::read_to_string(dir.join("run").join("report.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(value["jobs"][0]["status"].as_str(), Some("failed"));
        assert_eq!(value["jobs"][1]["status"].as_str(), Some("succeeded"));
        assert_eq!(value["counts"]["failed"].as_u64(), Some(1));
        assert_eq!(value["counts"]["succeeded"].as_u64(), Some(1));
        assert_eq!(
            value["jobs"][0]["update_argv"][0].as_str(),
            Some("/nonexistent/update-agents-missing-binary")
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn blocked_jobs_never_execute() {
        let dir = temp_dir("blocked");
        let probe = dir.join("probe.txt");
        let mut one = spec("blk", "bres", &format!("echo b >> '{}'", probe.display()));
        one.preflight = Preflight::Blocked("proxy running".to_string());
        let mut handle = start(
            vec![one],
            RunOptions {
                jobs: 1,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let state = lock_state(&handle.state);
        let job = &state.jobs[0];
        assert_eq!(job.status, Status::Blocked);
        assert_eq!(job.message, "proxy running");
        assert_eq!(job.exit_code, None);
        assert_eq!(job.before, "");
        assert_eq!(job.after, "");
        assert!(!probe.exists());
        drop(state);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Skipped jobs never execute update or version commands, and a ready
    /// peer sharing their resource can finish.
    #[test]
    fn skipped_job_executes_nothing_and_ready_peer_succeeds() {
        let dir = temp_dir("skipped");
        let probe = dir.join("probe.txt");
        let mut ghost = spec(
            "ghost",
            "shared",
            &format!("echo update-ghost >> '{}'", probe.display()),
        );
        ghost.version = Some(cmd(
            "/bin/sh",
            &[
                "-c",
                &format!("echo version-ghost >> '{}'; echo 0.0.0", probe.display()),
            ],
        ));
        ghost.preflight = Preflight::Skipped("executable 'ghost' not found".to_string());
        let mut live = spec(
            "live",
            "shared",
            &format!("echo update-live >> '{}'", probe.display()),
        );
        live.version = Some(cmd(
            "/bin/sh",
            &[
                "-c",
                &format!("echo version-live >> '{}'; echo 9.9.9", probe.display()),
            ],
        ));
        let mut handle = start(
            vec![ghost, live],
            RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let lines = read_lines(&probe);
        assert_eq!(
            lines,
            vec!["version-live", "update-live", "version-live"],
            "the skipped job must not run its update or version commands: {lines:?}"
        );
        let report = fs::read_to_string(dir.join("run").join("report.json")).unwrap();
        let value: serde_json::Value = serde_json::from_str(&report).unwrap();
        assert_eq!(value["jobs"][0]["status"].as_str(), Some("skipped"));
        assert_eq!(value["jobs"][1]["status"].as_str(), Some("succeeded"));
        assert_eq!(value["counts"]["skipped"].as_u64(), Some(1));
        assert_eq!(value["counts"]["succeeded"].as_u64(), Some(1));
        assert_eq!(value["counts"]["blocked"].as_u64(), Some(0));
        assert_eq!(value["counts"]["failed"].as_u64(), Some(0));
        assert_eq!(value["counts"]["unfinished"].as_u64(), Some(0));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn failure_marker_flags_false_success() {
        let dir = temp_dir("marker");
        // Marker ~320 KiB before EOF: no bounded tail could see it, only a
        // streaming scan of the whole completed log can.
        let mut false_success = spec(
            "kilo-like",
            "kres",
            "printf 'Upgrade failed\\n'; yes x | head -c 320000; echo finished",
        );
        false_success.failure_contains = vec!["Upgrade failed".to_string()];
        // Wrong case: markers are case-sensitive literals, so this succeeds.
        let mut case_probe = spec("case-ok", "cres", "printf 'Upgrade failed\\n'");
        case_probe.failure_contains = vec!["upgrade FAILED".to_string()];
        let specs = vec![false_success, case_probe];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let state = lock_state(&handle.state);
        let failed = &state.jobs[0];
        assert_eq!(failed.status, Status::Failed);
        assert_eq!(failed.exit_code, Some(0), "exit code stays truthful");
        assert!(failed.message.contains("Upgrade failed"));
        let succeeded = &state.jobs[1];
        assert_eq!(succeeded.status, Status::Succeeded);
        drop(state);
        let report = fs::read_to_string(dir.join("run").join("report.json")).unwrap();
        assert!(report.contains("failure_contains"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_is_bounded_and_sanitized() {
        let dir = temp_dir("tail");
        let path = dir.join("log.txt");
        // Byte string: the raw log contains invalid UTF-8 (0xFF 0xFE) and
        // control characters a String literal cannot express.
        let mut raw: Vec<u8> =
            b"plain\n\x1b[31mred\x1b[0m\n\x1b]0;title\x07osc\nline\rwith\x00ctl\x07end\n\xff\xfebad\n"
                .to_vec();
        raw.extend_from_slice(&[b'x'; 5000]);
        fs::write(&path, &raw).unwrap();
        let all = tail(&path, 1 << 20);
        assert!(all.contains("plain"));
        assert!(all.contains("red"));
        assert!(all.contains("osc"));
        assert!(all.contains("end"));
        assert!(!all.contains('\x1b'), "ESC sequences must be stripped");
        assert!(!all.contains('\x00'));
        assert!(!all.contains('\x07'));
        assert!(all.contains("line\nwith"), "lone CR becomes a newline");
        assert!(
            all.contains('\u{FFFD}'),
            "invalid UTF-8 becomes replacements"
        );
        let small = tail(&path, 8);
        assert!(small.chars().count() <= 8);
        let utf8_path = dir.join("utf8.txt");
        fs::write(&utf8_path, b"aaa\xC3\xA9\xC3\xA9").unwrap();
        assert_eq!(
            tail(&utf8_path, 2),
            "é",
            "tail resynchronizes on UTF-8 boundaries"
        );
        assert_eq!(tail(&path, 0), "");
        let _ = fs::remove_dir_all(&dir);
    }

    /// Regression: cancellation must not leak group survivors when the
    /// direct child dies on the group SIGTERM before its descendants do.
    /// A shell leader exits on TERM while a grandchild ignores TERM; the
    /// engine must hold the TERM grace for the whole owned group and then
    /// SIGKILL the survivor instead of returning with it still running.
    #[test]
    fn cancel_kills_group_survivors_after_leader_exit() {
        let dir = temp_dir("survivors");
        let log = dir.join("survivors.log");
        let survivor_pid_file = dir.join("survivor.pid");
        // The inner shell ignores SIGTERM and stays alive in a bounded
        // loop; the outer shell leader keeps the default disposition.
        let ignore_term =
            "trap \"\" TERM; n=0; while [ \"$n\" -lt 300 ]; do sleep 1; n=$((n+1)); done";
        let script = format!(
            "sh -c '{ignore_term}' &\necho $! > '{}'\nsleep 300",
            survivor_pid_file.display()
        );
        let cancel = Arc::new(AtomicBool::new(false));
        let trigger = cancel.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(300));
            trigger.store(true, Ordering::Relaxed);
        });
        let command = cmd("/bin/sh", &["-c", script.as_str()]);
        let end = run_phase(&command, Duration::from_secs(60), &cancel, &log).unwrap();
        assert!(matches!(end, PhaseEnd::Cancelled));
        let survivor: i32 = fs::read_to_string(&survivor_pid_file)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            wait_until(Duration::from_secs(5), || !pid_alive(survivor)),
            "grandchild that ignored TERM must not survive cancellation (pid {survivor})"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    /// The failure scan must verify every marker the catalogue accepts and
    /// must never silently drop or misreport one: a marker of exactly
    /// MAX_MARKER_BYTES is matched, while anything longer fails the job as
    /// an incomplete verification instead of passing as a clean log.
    #[test]
    fn marker_scan_enforces_length_bound() {
        let dir = temp_dir("markerbound");
        // yes M | head -c 8192 yields 4096 M's after newline stripping.
        let mut detected = spec("limit", "lres", "yes M | head -c 8192 | tr -d '\\n'");
        detected.failure_contains = vec!["M".repeat(MAX_MARKER_BYTES)];
        let mut refused = spec("toolong", "tres", "echo finished");
        refused.failure_contains = vec!["M".repeat(MAX_MARKER_BYTES + 1)];
        let specs = vec![detected, refused];
        let mut handle = start(
            specs,
            RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(30),
                run_dir: dir.join("run"),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let state = lock_state(&handle.state);
        let matched = &state.jobs[0];
        assert_eq!(matched.status, Status::Failed);
        assert_eq!(matched.exit_code, Some(0));
        assert!(matched.message.contains("matched failure marker"));
        let rejected = &state.jobs[1];
        assert_eq!(rejected.status, Status::Failed);
        assert_eq!(rejected.exit_code, Some(0));
        assert!(rejected.message.contains("could not verify"));
        assert!(rejected.message.contains("4097"));
        drop(state);
        let _ = fs::remove_dir_all(&dir);
    }
}
