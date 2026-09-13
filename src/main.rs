//! update-agents CLI orchestration.
//!
//! Owns option parsing, catalogue loading via `catalog`, engine start via
//! `engine`, TUI/plain driving via `ui`, the single-instance flock, XDG state
//! paths, background (`--bg`) detach with startup acknowledgement, the
//! SIGINT/SIGTERM/SIGHUP cancel bridge, tmux completion notification, and exit
//! codes. Contains no tool-ID-specific logic anywhere.

mod catalog;
mod engine;
mod model;
mod ui;

use crate::engine::RunHandle;
use crate::model::{Job, Preflight, RunOptions, RunState, Status, ToolSpec};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, IsTerminal};
use std::net::{SocketAddr, TcpStream};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PKG_VERSION: &str = env!("CARGO_PKG_VERSION");
const DEFAULT_TIMEOUT_SECS: u64 = 600;
const ACK_ENV: &str = "UPDATE_AGENTS_ACK_FD";
const ACK_PREFIX: &str = "READY ";
const FAIL_PREFIX: &str = "FAIL ";
const ACK_TIMEOUT_SECS: u64 = 60;
/// Reply margin subtracted from the launcher's acknowledgement timeout to
/// form the background child's own admission deadline, so a FAIL reply can
/// still reach the launcher before it gives up.
const STARTUP_MARGIN_SECS: u64 = 5;
const LOCK_NAME: &str = "update-agents.lock";
const REPORT_NAME: &str = "report.json";
const SESSION_LOG_NAME: &str = "session.log";

const EX_OK: i32 = 0;
const EX_FAILURE: i32 = 1;
const EX_USAGE: i32 = 2;
const EX_BLOCKED: i32 = 3;

// ---------------------------------------------------------------------------
// Signal bridge: async-signal-safe atomic cancel propagation to the engine.
// ---------------------------------------------------------------------------

static SIG_CANCEL_CELL: AtomicPtr<AtomicBool> = AtomicPtr::new(std::ptr::null_mut());
static SIG_PENDING: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    // Idempotent cancellation request only: async-signal-safe atomics, no
    // allocation, no locks, no forced _exit. Repeat signals are no-ops; the
    // engine's SIGTERM/SIGKILL escalation finishes the children so the setsid
    // process groups, the run lock and the report stay managed.
    SIG_PENDING.store(true, Ordering::SeqCst);
    let cell = SIG_CANCEL_CELL.load(Ordering::SeqCst);
    if !cell.is_null() {
        unsafe { (*cell).store(true, Ordering::SeqCst) };
    }
}

fn install_signal_handlers() {
    let handler: extern "C" fn(libc::c_int) = on_signal;
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = handler as usize;
    action.sa_flags = libc::SA_RESTART;
    for sig in [libc::SIGINT, libc::SIGTERM, libc::SIGHUP] {
        unsafe {
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

/// Point the signal handler at the engine cancel flag after `engine::start`.
///
/// One `Arc` reference is intentionally retained for the rest of the process:
/// a handler can load the pointer right before `unpublish_cancel` runs and
/// dereference it afterwards, so the flag allocation is kept alive until
/// process exit. The leak is bounded to a single `AtomicBool`.
fn publish_cancel(cell: &Arc<AtomicBool>) {
    std::mem::forget(Arc::clone(cell));
    SIG_CANCEL_CELL.store(Arc::as_ptr(cell) as *mut AtomicBool, Ordering::SeqCst);
    // A signal may have arrived before publication; backfill.
    if SIG_PENDING.load(Ordering::SeqCst) {
        cell.store(true, Ordering::SeqCst);
    }
}

/// Clear the handler's target. The `Arc` retained by `publish_cancel` keeps
/// the flag allocation valid for any in-flight handler dereference.
fn unpublish_cancel() {
    SIG_CANCEL_CELL.store(std::ptr::null_mut(), Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------

struct Options {
    help: bool,
    version: bool,
    list: bool,
    plain: bool,
    bg: bool,
    jobs: Option<usize>,
    timeout: Option<u64>,
    agents_dir: Option<PathBuf>,
    ids: Vec<String>,
}

fn parse_args(args: &[OsString]) -> Result<Options, String> {
    let mut o = Options {
        help: false,
        version: false,
        list: false,
        plain: false,
        bg: false,
        jobs: None,
        timeout: None,
        agents_dir: None,
        ids: Vec::new(),
    };
    let mut ids_only = false;
    let mut i = 0usize;
    while i < args.len() {
        let raw = &args[i];
        let text = raw.to_string_lossy();
        if ids_only {
            push_id(&mut o, &text)?;
            i += 1;
            continue;
        }
        let bytes = raw.as_bytes();
        if bytes.starts_with(b"--") {
            if let Some(eq) = bytes.iter().position(|&b| b == b'=') {
                let name = String::from_utf8_lossy(&bytes[..eq]).into_owned();
                let value = OsStr::from_bytes(&bytes[eq + 1..]);
                match name.as_str() {
                    "--jobs" => o.jobs = Some(parse_count(&value.to_string_lossy(), "--jobs")?),
                    "--timeout" => {
                        o.timeout = Some(parse_secs(&value.to_string_lossy(), "--timeout")?)
                    }
                    "--agents-dir" => o.agents_dir = Some(dir_value(value)?),
                    "--help" | "--version" | "--list" | "--dry-run" | "--plain" | "--bg" => {
                        return Err(format!("{name} does not take a value"));
                    }
                    _ => return Err(format!("unknown option '{name}' (try --help)")),
                }
                i += 1;
                continue;
            }
            match text.as_ref() {
                "--" => ids_only = true,
                "--help" => o.help = true,
                "--version" => o.version = true,
                "--list" | "--dry-run" => o.list = true,
                "--plain" => o.plain = true,
                "--bg" => o.bg = true,
                "--jobs" => {
                    i += 1;
                    let v = arg_value(args, i, "--jobs")?;
                    o.jobs = Some(parse_count(&v.to_string_lossy(), "--jobs")?);
                }
                "--timeout" => {
                    i += 1;
                    let v = arg_value(args, i, "--timeout")?;
                    o.timeout = Some(parse_secs(&v.to_string_lossy(), "--timeout")?);
                }
                "--agents-dir" => {
                    i += 1;
                    let v = arg_value(args, i, "--agents-dir")?;
                    o.agents_dir = Some(dir_value(v)?);
                }
                other => return Err(format!("unknown option '{other}' (try --help)")),
            }
            i += 1;
            continue;
        }
        if text.starts_with('-') && text.len() > 1 {
            return Err(format!("unknown option '{text}' (try --help)"));
        }
        push_id(&mut o, &text)?;
        i += 1;
    }
    Ok(o)
}

fn arg_value<'a>(args: &'a [OsString], i: usize, opt: &str) -> Result<&'a OsString, String> {
    args.get(i).ok_or_else(|| format!("{opt} needs a value"))
}

fn dir_value(v: &OsStr) -> Result<PathBuf, String> {
    if v.is_empty() {
        return Err("--agents-dir needs a directory path".to_string());
    }
    Ok(expand_home(v))
}

fn push_id(o: &mut Options, id: &str) -> Result<(), String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("empty tool id".to_string());
    }
    if !o.ids.iter().any(|x| x == id) {
        o.ids.push(id.to_string());
    }
    Ok(())
}

fn parse_count(v: &str, opt: &str) -> Result<usize, String> {
    let n: usize = v
        .trim()
        .parse()
        .map_err(|_| format!("{opt} expects a whole number, got '{v}'"))?;
    if n == 0 {
        return Err(format!("{opt} must be at least 1"));
    }
    Ok(n)
}

fn parse_secs(v: &str, opt: &str) -> Result<u64, String> {
    let n: u64 = v
        .trim()
        .parse()
        .map_err(|_| format!("{opt} expects a whole number of seconds, got '{v}'"))?;
    if n == 0 {
        return Err(format!("{opt} must be at least 1"));
    }
    Ok(n)
}

/// Expand a leading `~` or `$HOME` in a user-supplied path value only.
fn expand_home(value: &OsStr) -> PathBuf {
    let bytes = value.as_bytes();
    let home = match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() => PathBuf::from(h),
        _ => return PathBuf::from(value),
    };
    let rest: &[u8] = if bytes == &b"~"[..] || bytes == &b"$HOME"[..] {
        b""
    } else if bytes.starts_with(b"~/") {
        &bytes[2..]
    } else if bytes.starts_with(b"$HOME/") {
        &bytes[6..]
    } else {
        return PathBuf::from(value);
    };
    let mut p = home;
    if !rest.is_empty() {
        p.push(OsStr::from_bytes(rest));
    }
    p
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn home_dir() -> Result<PathBuf, String> {
    match std::env::var_os("HOME") {
        Some(h) if !h.is_empty() && Path::new(&h).is_absolute() => Ok(PathBuf::from(h)),
        _ => Err("HOME is not set to an absolute path".to_string()),
    }
}

/// XDG lookup with spec-compliant fallback (`empty or relative => default`).
fn xdg_dir(var: &str, fallback: &str) -> Result<PathBuf, String> {
    if let Some(v) = std::env::var_os(var) {
        let p = PathBuf::from(&v);
        if !v.is_empty() && p.is_absolute() {
            return Ok(p);
        }
    }
    Ok(home_dir()?.join(fallback))
}

fn state_dir() -> Result<PathBuf, String> {
    Ok(xdg_dir("XDG_STATE_HOME", ".local/state")?.join("update-agents"))
}

fn mkdir_private(path: &Path) -> io::Result<()> {
    fs::create_dir_all(path)?;
    fs::set_permissions(path, Permissions::from_mode(0o700))
}

fn make_run_dir(state_dir: &Path) -> Result<PathBuf, String> {
    let runs = state_dir.join("runs");
    mkdir_private(&runs).map_err(|e| format!("cannot create {}: {e}", runs.display()))?;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let dir = runs.join(format!("{}-{}", utc_timestamp(secs), std::process::id()));
    fs::create_dir(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let _ = fs::set_permissions(&dir, Permissions::from_mode(0o700));
    Ok(dir)
}

/// Days-to-civil conversion (Howard Hinnant algorithm), UTC, no dependencies.
fn utc_timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (hh, mi, ss) = (rem / 3_600, (rem % 3_600) / 60, rem % 60);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{hh:02}{mi:02}{ss:02}")
}

fn catalogue_source_desc(custom: Option<&Path>) -> String {
    if let Some(dir) = custom {
        return format!("catalogue: {}", dir.display());
    }
    let builtin = xdg_dir("XDG_DATA_HOME", ".local/share")
        .ok()
        .map(|d| d.join("update-agents").join("agents.d"));
    let user = xdg_dir("XDG_CONFIG_HOME", ".config")
        .ok()
        .map(|d| d.join("update-agents").join("agents.d"));
    match (builtin, user) {
        (Some(b), Some(u)) => {
            format!(
                "catalogue: {} (builtin) + {} (user)",
                b.display(),
                u.display()
            )
        }
        _ => "catalogue: builtin + user agents.d directories".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Single-instance lock (held until every child has exited)
// ---------------------------------------------------------------------------

struct RunLock {
    _file: File,
}

fn acquire_lock(state_dir: &Path) -> io::Result<Option<RunLock>> {
    let path = state_dir.join(LOCK_NAME);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .open(&path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(Some(RunLock { _file: file }))
    } else {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            Ok(None)
        } else {
            Err(err)
        }
    }
}

// ---------------------------------------------------------------------------
// Background detach (--bg)
// ---------------------------------------------------------------------------

/// Launcher side: spawn this same executable detached with `--plain`, wait for
/// its startup acknowledgement, then report PID + report path. Race-safe: the
/// ack travels over an anonymous pipe created by this process, never a pidfile.
fn bg_launch(args: &[OsString]) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("update-agents: cannot resolve own executable: {e}");
            return EX_FAILURE;
        }
    };

    // Forward every argument except --bg; the child always runs non-interactive.
    let mut child_args: Vec<OsString> = Vec::with_capacity(args.len() + 1);
    let mut plain = false;
    for a in args {
        if a.as_os_str() == OsStr::new("--bg") {
            continue;
        }
        if a.as_os_str() == OsStr::new("--plain") {
            plain = true;
        }
        child_args.push(a.clone());
    }
    if !plain {
        child_args.push(OsString::from("--plain"));
    }

    // pipe() without CLOEXEC so the write end survives the child's exec().
    let mut fds: [libc::c_int; 2] = [0; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
        eprintln!(
            "update-agents: cannot create ack pipe: {}",
            io::Error::last_os_error()
        );
        return EX_FAILURE;
    }
    let (read_fd, write_fd) = (fds[0], fds[1]);
    unsafe {
        libc::fcntl(read_fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let spawn = {
        let mut cmd = Command::new(&exe);
        cmd.args(&child_args)
            .env(ACK_ENV, write_fd.to_string())
            .stdin(Stdio::null());
        // SAFETY: this only registers the hook; it runs in the forked child
        // between fork() and exec() and performs exactly one operation,
        // libc::setsid(), which is async-signal-safe (no allocation, no
        // locks, no std re-entry), so it cannot deadlock or unwind there.
        unsafe {
            cmd.pre_exec(|| {
                // Own session: survives terminal close, no controlling tty,
                // immune to Ctrl-C aimed at the launcher's foreground group.
                if libc::setsid() < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
        cmd.spawn()
    };
    // The parent must not keep the write end, or EOF never arrives.
    unsafe { libc::close(write_fd) };
    let child = match spawn {
        Ok(c) => c,
        Err(e) => {
            unsafe { libc::close(read_fd) };
            eprintln!("update-agents: cannot start background process: {e}");
            return EX_FAILURE;
        }
    };
    let pid = child.id();
    drop(child);

    let ack = read_ack(read_fd, Duration::from_secs(ACK_TIMEOUT_SECS));
    unsafe { libc::close(read_fd) };

    match ack.as_deref() {
        Some(line) if line.starts_with(ACK_PREFIX) => {
            let dir = line[ACK_PREFIX.len()..].trim();
            println!("background update started (pid {pid})");
            println!("report: {dir}/{REPORT_NAME}");
            println!("log:    {dir}/{SESSION_LOG_NAME}");
            EX_OK
        }
        Some(line) if line.starts_with(FAIL_PREFIX) => {
            eprintln!(
                "update-agents: background run failed to start: {}",
                &line[FAIL_PREFIX.len()..]
            );
            EX_FAILURE
        }
        Some(line) => {
            eprintln!("update-agents: unexpected background acknowledgement: {line}");
            EX_FAILURE
        }
        None => {
            eprintln!(
                "update-agents: background process (pid {pid}) did not acknowledge startup (it exited early or took too long)"
            );
            EX_FAILURE
        }
    }
}

/// Background-child side: read the launcher's acknowledgement until EOF or a
/// deadline. Returns the first line, or None on EOF-without-data/timeout.
fn read_ack(fd: libc::c_int, timeout: Duration) -> Option<String> {
    let deadline = Instant::now() + timeout;
    let mut buf = [0u8; 512];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let now = Instant::now();
        if now >= deadline {
            return None;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let remain = (deadline - now).as_millis().min(i32::MAX as u128) as libc::c_int;
        let rc = unsafe { libc::poll(&mut pfd, 1, remain) };
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if rc == 0 {
            return None;
        }
        let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
        if n < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        if n == 0 {
            break; // EOF: child closed the ack fd
        }
        acc.extend_from_slice(&buf[..n as usize]);
        if acc.len() > 8_192 {
            break;
        }
    }
    let text = String::from_utf8_lossy(&acc);
    text.lines().next().map(|l| l.trim_end().to_string())
}

/// Best-effort single-line ack write, then close. EPIPE (launcher gone) is
/// ignored so the run continues detached.
fn bg_send_ack(fd: libc::c_int, line: &str) {
    let mut msg = line.as_bytes().to_vec();
    msg.push(b'\n');
    let mut off = 0usize;
    while off < msg.len() {
        let n = unsafe {
            libc::write(
                fd,
                msg[off..].as_ptr() as *const libc::c_void,
                msg.len() - off,
            )
        };
        if n <= 0 {
            if n < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break;
        }
        off += n as usize;
    }
    unsafe { libc::close(fd) };
}

/// Mark the inherited ack fd CLOEXEC in the background child so engine-spawned
/// updater processes never hold the pipe open.
fn mark_ack_fd_cloexec(fd: libc::c_int) {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFD);
        if flags >= 0 {
            libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC);
        }
    }
}

/// Redirect stdin from /dev/null and stdout/stderr into the private run log.
fn redirect_stdio(run_dir: &Path) -> io::Result<()> {
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(run_dir.join(SESSION_LOG_NAME))?;
    let null = File::open("/dev/null")?;
    unsafe {
        if libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO) < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// On finalize in a tmux session, notify with the invocation argv only
/// (executed via argv, never a shell). Best effort; never fatal.
fn tmux_notify(args: &[OsString]) {
    if std::env::var_os("TMUX").is_none() {
        return;
    }
    let invocation = args
        .iter()
        .map(|a| a.to_string_lossy())
        .collect::<Vec<_>>()
        .join(" ");
    let text = format!("update-agents finished: {invocation}");
    let _ = Command::new("tmux")
        .args(["display-message", text.as_str()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

// ---------------------------------------------------------------------------
// Driving helpers
// ---------------------------------------------------------------------------

fn snapshot(handle: &RunHandle) -> RunState {
    handle
        .state
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clone()
}

fn exit_code(state: &RunState) -> i32 {
    let mut failed = false;
    let mut blocked = false;
    for job in &state.jobs {
        match job.status {
            Status::Failed | Status::TimedOut | Status::Cancelled => failed = true,
            Status::Blocked => blocked = true,
            // Skipped agents (missing executables) resolve without changing
            // the exit code: they are neither failed nor blocked.
            _ => {}
        }
    }
    if failed {
        EX_FAILURE
    } else if blocked {
        EX_BLOCKED
    } else {
        EX_OK
    }
}

/// Wait for the engine, print the engine summary, derive the exit code.
fn finish(handle: &mut RunHandle, explicit: bool) -> i32 {
    let wait_result = handle.wait();
    let snap = snapshot(handle);
    println!();
    println!("{}", engine::summary(&snap, explicit));
    if let Err(e) = wait_result {
        eprintln!("update-agents: engine: {e}");
        return EX_FAILURE;
    }
    exit_code(&snap)
}

/// Non-interactive run: real status-transition lines, then the summary.
fn plain_run(handle: &RunHandle, jobs: usize, timeout_secs: u64, run_dir: &Path, explicit: bool) {
    let (count, hidden) = {
        let st = handle.state.lock().unwrap_or_else(|p| p.into_inner());
        let hidden = st
            .jobs
            .iter()
            .filter(|job| job.status.hidden_from_list(explicit))
            .count();
        (st.jobs.len(), hidden)
    };
    println!("{}", updating_header(count, hidden, jobs, timeout_secs));
    println!("run dir: {}", run_dir.display());
    let mut last: Vec<Status> = vec![Status::Queued; count];
    let mut cancel_noted = false;
    loop {
        let (lines, done) = {
            let st = handle.state.lock().unwrap_or_else(|p| p.into_inner());
            (collect_transitions(&mut last, &st.jobs, explicit), st.done)
        };
        for line in &lines {
            println!("{line}");
        }
        if done {
            return;
        }
        if !cancel_noted && handle.cancel.load(Ordering::SeqCst) {
            cancel_noted = true;
            println!("cancellation requested, stopping running updates");
        }
        std::thread::sleep(Duration::from_millis(150));
    }
}

fn transition_line(job: &Job) -> String {
    let id = job.spec.id.as_str();
    match job.status {
        Status::Queued => format!("{id}: queued"),
        Status::Running => format!("{id}: running"),
        Status::Succeeded => format!("{id}: ok{}", detail(job)),
        Status::Failed => format!("{id}: failed{}", detail(job)),
        Status::TimedOut => format!("{id}: timed out{}", detail(job)),
        Status::Cancelled => format!("{id}: cancelled{}", detail(job)),
        Status::Blocked => format!("{id}: blocked - {}", one_line(&job.message)),
        Status::Skipped => format!("{id}: not detected - {}", one_line(&job.message)),
    }
}

/// Header line for plain runs; names the not-detected agents when any exist.
fn updating_header(count: usize, hidden: usize, jobs: usize, timeout_secs: u64) -> String {
    if hidden > 0 {
        format!(
            "updating {count} agent(s), {hidden} not detected, jobs={jobs}, timeout={timeout_secs}s"
        )
    } else {
        format!("updating {count} agent(s), jobs={jobs}, timeout={timeout_secs}s")
    }
}

/// Diffs `jobs` against `last`, updating it in place, and returns the
/// transition lines to print. Not-detected lines are suppressed unless the
/// tools were selected explicitly.
fn collect_transitions(last: &mut [Status], jobs: &[Job], explicit: bool) -> Vec<String> {
    let mut lines = Vec::new();
    for (i, job) in jobs.iter().enumerate() {
        if last[i] != job.status {
            last[i] = job.status;
            if !job.status.hidden_from_list(explicit) {
                lines.push(transition_line(job));
            }
        }
    }
    lines
}

fn detail(job: &Job) -> String {
    if job.message.trim().is_empty() {
        String::new()
    } else {
        format!(" - {}", one_line(&job.message))
    }
}

/// Single-line, bounded rendering of an engine message or preflight reason.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(400));
    for (n, c) in s.chars().enumerate() {
        if n >= 300 {
            out.push_str("...");
            break;
        }
        match c {
            '\n' | '\r' | '\t' => out.push(' '),
            _ => out.push(c),
        }
    }
    out
}

/// Safe catalogue view: exact argv, no execution, no fabricated state.
fn print_catalogue(specs: &[ToolSpec], source: &str) {
    println!("update-agents {PKG_VERSION} - {} agent(s)", specs.len());
    println!("{source}");
    println!();
    let width = specs.iter().map(|s| s.id.len()).max().unwrap_or(5);
    for s in specs {
        println!("{:width$}  {}", s.id, s.label, width = width);
        println!(
            "{:width$}  update : {} {}",
            "",
            s.update.program,
            s.update.args.join(" "),
            width = width
        );
        match &s.version {
            Some(v) => println!(
                "{:width$}  version: {} {} (line {})",
                "",
                v.program,
                v.args.join(" "),
                s.version_line,
                width = width
            ),
            None => println!("{:width$}  version: (not reported)", "", width = width),
        }
        println!("{:width$}  resource: {}", "", s.resource, width = width);
        if !s.failure_contains.is_empty() {
            println!(
                "{:width$}  failure markers: {}",
                "",
                s.failure_contains
                    .iter()
                    .map(|m| format!("\"{m}\""))
                    .collect::<Vec<_>>()
                    .join(", "),
                width = width
            );
        }
        match &s.preflight {
            Preflight::Ready => println!("{:width$}  state  : ready", "", width = width),
            Preflight::Skipped(reason) => println!(
                "{:width$}  state  : not detected - {}",
                "",
                one_line(reason),
                width = width
            ),
            Preflight::Blocked(reason) => println!(
                "{:width$}  state  : blocked - {}",
                "",
                one_line(reason),
                width = width
            ),
        }
        println!();
    }
}

fn fail_startup(ack_fd: Option<i32>, msg: &str, code: i32) -> i32 {
    if let Some(fd) = ack_fd {
        bg_send_ack(fd, &format!("{FAIL_PREFIX}{msg}"));
    }
    eprintln!("update-agents: {msg}");
    code
}

fn fail_usage(msg: &str, ack_fd: Option<i32>) -> i32 {
    if let Some(fd) = ack_fd {
        bg_send_ack(fd, &format!("{FAIL_PREFIX}{msg}"));
    }
    eprintln!("update-agents: {msg}");
    EX_USAGE
}

/// True once the background-child admission deadline has passed: the launcher
/// has either already been told FAIL or is about to time out, so no new
/// update work may be admitted.
fn startup_deadline_exceeded(deadline: Option<Instant>) -> bool {
    deadline.is_some_and(|d| Instant::now() >= d)
}

/// Consume the background acknowledgement fd handed over by a `--bg`
/// launcher: parse `UPDATE_AGENTS_ACK_FD` once, then scrub it from this
/// process' environment so engine-spawned updater children never inherit it.
/// Without the scrub, a native updater that re-invokes `update-agents` would
/// mistake itself for a background child: its output would vanish into its
/// own run log and its ack write would hit an arbitrary inherited fd number.
///
/// SAFETY: `env::remove_var` mutates the process environment and is `unsafe`
/// in Rust 2024 because a concurrent `getenv`/`setenv` from another thread is
/// undefined behavior. This runs exactly once at the top of `real_main`,
/// before option parsing, the catalogue preflight, the engine and the UI
/// exist: the process is still single-threaded, so no other thread can touch
/// the environment.
fn consume_ack_fd_env() -> Option<libc::c_int> {
    let fd = std::env::var(ACK_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<libc::c_int>().ok());
    // SAFETY: single-threaded startup; see the function contract above.
    unsafe { std::env::remove_var(ACK_ENV) };
    fd
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    std::process::exit(real_main());
}

/// Minimal connectivity probe: one TCP connect attempt per target, then give
/// up. Targets are IP literals so an offline machine usually fails in
/// milliseconds (no route); a blackholed network costs at most the connect
/// timeout per target. Any single success counts as online; this is a
/// liveness check, not a content check.
fn has_network() -> bool {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(2);
    for target in ["1.1.1.1:443", "8.8.8.8:443"] {
        let Ok(addr) = target.parse::<SocketAddr>() else {
            continue;
        };
        if TcpStream::connect_timeout(&addr, PROBE_TIMEOUT).is_ok() {
            return true;
        }
    }
    false
}

fn real_main() -> i32 {
    install_signal_handlers();
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    let ack_fd = consume_ack_fd_env();

    let opts = match parse_args(&args) {
        Ok(o) => o,
        Err(e) => return fail_usage(&e, ack_fd),
    };
    if opts.help {
        print!("{HELP}");
        return EX_OK;
    }
    if opts.version {
        println!("update-agents {PKG_VERSION}");
        return EX_OK;
    }

    // Offline gate: quit quietly with one message and exit code 0 before any
    // catalogue load, run lock, preflight check, or TUI starts. Updaters can
    // only fail without network, and a cron-invoked background run must not
    // wake the machine into doomed work. The probe is at most two short TCP
    // connects to IP literals: no DNS, no HTTP, no child processes.
    // Background children skip it: their launcher already checked, and a
    // network loss after launch is the updaters' own failure to handle.
    if ack_fd.is_none() && !has_network() {
        println!("update-agents: no network access; exiting without updates");
        return EX_OK;
    }

    let bg_child = ack_fd.is_some();
    if opts.bg && !bg_child {
        if opts.list {
            return fail_usage("--bg cannot be combined with --list/--dry-run", ack_fd);
        }
        return bg_launch(&args);
    }

    run_updates(&args, &opts, bg_child, ack_fd)
}

/// Catalogue load, selection validation, lock, engine start, then TUI or plain
/// driving. Every failure path exits before any update can run; background
/// children also refuse to admit update work once the launcher's
/// acknowledgement window has passed.
fn run_updates(args: &[OsString], opts: &Options, bg_child: bool, ack_fd: Option<i32>) -> i32 {
    if let Some(fd) = ack_fd {
        mark_ack_fd_cloexec(fd);
    }

    // Background admission window: the launcher stops listening for the
    // acknowledgement after ACK_TIMEOUT_SECS, so this child must never admit
    // new update work later than that minus the reply margin. The anchor is
    // captured before any possibly-blocking startup step (the catalogue
    // preflight runs unbounded read-only checks) and re-checked after each
    // such step and immediately before the engine starts.
    let window_msg = "background startup window elapsed; not starting updates";
    let bg_admission_deadline = bg_child.then(|| {
        Instant::now() + Duration::from_secs(ACK_TIMEOUT_SECS.saturating_sub(STARTUP_MARGIN_SECS))
    });

    // Catalogue load precedes lock/update and selection validation.
    let specs = match catalog::load(opts.agents_dir.as_deref()) {
        Ok(s) => s,
        Err(e) => return fail_startup(ack_fd, &format!("catalogue load failed: {e}"), EX_USAGE),
    };

    if startup_deadline_exceeded(bg_admission_deadline) {
        return fail_startup(ack_fd, window_msg, EX_FAILURE);
    }

    let unknown: Vec<String> = opts
        .ids
        .iter()
        .filter(|id| !specs.iter().any(|s| &s.id == *id))
        .cloned()
        .collect();
    if !unknown.is_empty() {
        let known: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
        return fail_startup(
            ack_fd,
            &format!(
                "unknown tool(s): {}; known: {}",
                unknown.join(", "),
                known.join(", ")
            ),
            EX_USAGE,
        );
    }
    let selected: Vec<ToolSpec> = if opts.ids.is_empty() {
        specs.clone()
    } else {
        opts.ids
            .iter()
            .map(|id| {
                specs
                    .iter()
                    .find(|s| s.id == *id)
                    .expect("validated above")
                    .clone()
            })
            .collect()
    };
    let explicit = !opts.ids.is_empty();
    let source = catalogue_source_desc(opts.agents_dir.as_deref());

    if opts.list {
        print_catalogue(&selected, &source);
        return EX_OK;
    }
    if selected.is_empty() {
        return fail_startup(ack_fd, "no agent definitions found", EX_USAGE);
    }

    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let jobs = opts.jobs.unwrap_or(cores).min(selected.len()).max(1);
    let timeout_secs = opts.timeout.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let timeout = Duration::from_secs(timeout_secs);

    let state_dir = match state_dir() {
        Ok(d) => d,
        Err(e) => return fail_startup(ack_fd, &e, EX_FAILURE),
    };
    let run_dir = match make_run_dir(&state_dir) {
        Ok(d) => d,
        Err(e) => return fail_startup(ack_fd, &e, EX_FAILURE),
    };

    // Background child captures all output in the private run directory before
    // anything prints from here on.
    if bg_child {
        if let Err(e) = redirect_stdio(&run_dir) {
            return fail_startup(ack_fd, &format!("cannot attach run log: {e}"), EX_FAILURE);
        }
    }

    // Single process at a time; the lock is held until every child has exited.
    let _lock = match acquire_lock(&state_dir) {
        Ok(Some(l)) => l,
        Ok(None) => {
            return fail_startup(
                ack_fd,
                "another update-agents run is already in progress",
                EX_FAILURE,
            );
        }
        Err(e) => return fail_startup(ack_fd, &format!("run lock: {e}"), EX_FAILURE),
    };

    // Last admission gate: once engine::start returns, updater commands
    // execute. Neither a lapsed launcher window nor a shutdown signal that
    // arrived during preflight may launch new work.
    if startup_deadline_exceeded(bg_admission_deadline) {
        return fail_startup(ack_fd, window_msg, EX_FAILURE);
    }
    if SIG_PENDING.load(Ordering::SeqCst) {
        return fail_startup(
            ack_fd,
            "shutdown signal received before updates started",
            EX_FAILURE,
        );
    }

    let mut handle = match engine::start(
        selected,
        RunOptions {
            jobs,
            timeout,
            run_dir: run_dir.clone(),
        },
    ) {
        Ok(h) => h,
        Err(e) => return fail_startup(ack_fd, &format!("engine start failed: {e}"), EX_FAILURE),
    };
    publish_cancel(&handle.cancel);

    // Startup acknowledgement goes out only now: catalogue loaded, lock held,
    // engine running. The launcher reports this as a real start, not a spawn.
    if let Some(fd) = ack_fd {
        bg_send_ack(fd, &format!("{ACK_PREFIX}{}", run_dir.display()));
    }

    let tui = !opts.plain
        && !bg_child
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(false);

    let code = if tui {
        match ui::run(&handle, explicit) {
            Ok(()) => {
                println!();
                finish(&mut handle, explicit)
            }
            Err(e) => {
                handle.cancel.store(true, Ordering::SeqCst);
                if let Err(we) = handle.wait() {
                    eprintln!("update-agents: engine: {we}");
                }
                let snap = snapshot(&handle);
                println!();
                println!("{}", engine::summary(&snap, explicit));
                eprintln!("update-agents: terminal ui failed: {e}");
                exit_code(&snap).max(EX_FAILURE)
            }
        }
    } else {
        plain_run(&handle, jobs, timeout_secs, &run_dir, explicit);
        finish(&mut handle, explicit)
    };

    unpublish_cancel();

    if bg_child {
        tmux_notify(args);
    }
    code
}

const HELP: &str = "\
update-agents 0.1.0 - update AI coding agents in parallel

USAGE:
    update-agents [OPTIONS] [ID...]

Without IDs every catalogue agent updates. Known IDs only; bad options or
unknown IDs fail before anything runs. Agents whose executables are not
detected run nothing, never fail the run, and stay out of the list unless
requested by ID; the report still records them.

OPTIONS:
    --list, --dry-run     Print update/version commands and exit; change nothing
    --plain               Non-interactive status lines (automatic when stdin or
                          stdout is not a terminal, or TERM=dumb)
    --jobs N              Parallel updates. Default: CPU cores, capped at the
                          number of selected agents
    --timeout SECONDS     Stop an agent update after this long (default 600)
    --bg                  Detach this same executable with --plain: the shell
                          returns immediately with PID, report and log paths;
                          output is captured in the run directory
    --agents-dir DIR      Load agents only from DIR (replaces the builtin and
                          user catalogue defaults; forwarded to --bg children)
    --version             Print version and exit
    --help                This help

EXIT CODES:
    0 all updates succeeded (not-detected executables do not cause a
      nonzero status)
    1 an update failed, timed out or was cancelled; startup failures
      (I/O, run lock) also exit 1
    2 usage or catalogue error (nothing ran)
    3 nothing failed but at least one agent was blocked (see report)

PATHS:
    builtin catalogue     ~/.local/share/update-agents/agents.d
    user catalogue        ~/.config/update-agents/agents.d (optional, also loaded)
    --agents-dir DIR      replaces both of the above
    runs                  ~/.local/state/update-agents/runs/<time>-<pid>/
                          report.json + per-agent logs; one process at a time
                          is enforced with a lock while updates run

TUI COMPLETION:
    Once every update finishes, the dashboard shows a five-second countdown.
    Press any key to exit sooner. The final summary and log paths remain in
    the terminal. --plain and --bg do not wait for this countdown.

AGENT DESCRIPTORS:
    Adding an agent means adding one JSON file to a catalogue directory - no
    code changes. Files load in sorted order; unknown fields, duplicate IDs or
    malformed JSON abort the run. Schema:

        {\"schema_version\":1,\"id\":\"example\",\"label\":\"Example Agent\",
         \"installed\":\"example\",
         \"update\":{\"program\":\"example\",\"args\":[\"update\"]},
         \"version\":{\"program\":\"example\",\"args\":[\"--version\"]},
         \"version_line\":0,\"resource\":\"example\",\"checks\":[],
         \"failure_contains\":[]}

    installed/update required; version optional (no version reporting when
    absent); version_line is the zero-based nonempty line of `version` output
    to use; resource groups tools that must not update concurrently and
    defaults to the id; failure_contains are case-sensitive literals that mark
    a run Failed even when the updater exits 0; checks are read-only gates:
        {\"kind\":\"git_clean\",\"path\":\"~/.hermes/hermes-agent\"}
        {\"kind\":\"process_absent\",\"cmdline_contains\":[\"/opencodex/\",\"start\"]}

EXAMPLES:
    update-agents                     # interactive TUI, all agents
    update-agents claude codex        # only these two, TUI
    update-agents --plain pi omp      # non-interactive status lines
    update-agents --bg --jobs 8       # background run; prints PID + report path
    update-agents --list              # show commands, change nothing
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CommandSpec;

    /// Minimal finished job, mirroring the `ui` tests' `job_with` pattern.
    fn job(id: &str, status: Status, message: &str) -> Job {
        Job {
            spec: ToolSpec {
                id: id.to_string(),
                label: id.to_string(),
                update: CommandSpec {
                    program: id.to_string(),
                    args: vec!["update".to_string()],
                },
                version: None,
                resource: id.to_string(),
                version_line: 0,
                failure_contains: Vec::new(),
                preflight: Preflight::Ready,
            },
            status,
            started: None,
            elapsed: Duration::ZERO,
            before: String::new(),
            after: String::new(),
            message: message.to_string(),
            log: PathBuf::from(format!("/tmp/update-agents-{id}.log")),
            exit_code: None,
        }
    }

    #[test]
    fn transition_line_words_not_detected_for_skipped_jobs() {
        let ghost = job("ghost", Status::Skipped, "executable 'ghost' not found");
        assert_eq!(
            transition_line(&ghost),
            "ghost: not detected - executable 'ghost' not found"
        );
        // Only the skipped arm changes wording; every other status is intact.
        assert_eq!(
            transition_line(&job("live", Status::Succeeded, "")),
            "live: ok"
        );
        assert_eq!(
            transition_line(&job("held", Status::Blocked, "dirty tree")),
            "held: blocked - dirty tree"
        );
    }

    #[test]
    fn collect_transitions_suppresses_not_detected_unless_explicit() {
        let jobs = vec![
            job("ghost", Status::Skipped, "reason"),
            job("live", Status::Succeeded, ""),
        ];
        let mut last = vec![Status::Queued; jobs.len()];
        let plain = collect_transitions(&mut last, &jobs, false);
        assert_eq!(
            plain,
            vec!["live: ok"],
            "not-detected lines stay out of plain output without explicit IDs"
        );
        assert_eq!(last, vec![Status::Skipped, Status::Succeeded]);

        let mut last = vec![Status::Queued; jobs.len()];
        let explicit = collect_transitions(&mut last, &jobs, true);
        assert_eq!(
            explicit,
            vec!["ghost: not detected - reason", "live: ok"],
            "explicitly selected tools always get their line"
        );
        assert_eq!(last, vec![Status::Skipped, Status::Succeeded]);
    }

    #[test]
    fn updating_header_mentions_not_detected_count() {
        assert_eq!(
            updating_header(4, 0, 4, 600),
            "updating 4 agent(s), jobs=4, timeout=600s"
        );
        assert_eq!(
            updating_header(4, 1, 4, 600),
            "updating 4 agent(s), 1 not detected, jobs=4, timeout=600s"
        );
    }
}
