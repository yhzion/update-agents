//! Data-driven tool catalogue.
//!
//! Every agent is described by a JSON descriptor in an `agents.d` directory;
//! supporting a new CLI requires only a new descriptor file, never a code
//! change. Built-in descriptors load from
//! `$XDG_DATA_HOME/update-agents/agents.d` (fallback
//! `~/.local/share/update-agents/agents.d`); user descriptors load from
//! `$XDG_CONFIG_HOME/update-agents/agents.d` (fallback
//! `~/.config/update-agents/agents.d`). An explicit directory replaces both.
//!
//! Loading is strict and fails closed: malformed JSON, unknown fields,
//! duplicate or unsafe ids, and invalid safety checks abort startup before
//! anything executes. Missing executables mark a tool `skipped` (the engine
//! records it and attempts nothing); failed read-only safety checks mark it
//! `blocked` instead of silently attempting an update.

use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
#[cfg(not(target_os = "macos"))]
use std::os::unix::fs::MetadataExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::Deserialize;

use crate::model::{CommandSpec, Preflight, ToolSpec};

/// Only descriptors written for this schema version load; anything else must
/// fail loudly instead of being misinterpreted.
const SUPPORTED_SCHEMA: u32 = 1;
const MAX_ID_LEN: usize = 64;
/// Upper bound for version_line so a typo cannot seek absurd depths.
const MAX_VERSION_LINE: usize = 10_000;
const GIT_CHECK_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_CHECK_POLL: Duration = Duration::from_millis(10);
const GIT_STDOUT_CAP: usize = 64 * 1024;
const GIT_STDERR_CAP: usize = 4 * 1024;
const CMDLINE_CAP: usize = 64 * 1024;
/// Longest fragment of command output quoted inside a reason string.
const REASON_CAP: usize = 240;

/// Load the catalogue. `None` reads the built-in directory (required) plus
/// the optional user directory; `Some(dir)` reads exactly that directory and
/// requires it to exist. Fails closed on any malformed descriptor.
pub fn load(directory: Option<&Path>) -> io::Result<Vec<ToolSpec>> {
    match directory {
        Some(dir) => load_from(Some(dir), None),
        None => {
            let builtin = builtin_dir()?;
            let user = user_dir()?;
            load_from(Some(builtin.as_path()), Some(user.as_path()))
        }
    }
}

fn load_from(builtin: Option<&Path>, user: Option<&Path>) -> io::Result<Vec<ToolSpec>> {
    let mut specs = Vec::new();
    let mut seen: HashMap<String, PathBuf> = HashMap::new();
    if let Some(dir) = builtin {
        load_dir(dir, true, &mut specs, &mut seen)?;
    }
    if let Some(dir) = user {
        load_dir(dir, false, &mut specs, &mut seen)?;
    }
    Ok(specs)
}

/// Built-in descriptor directory: `$XDG_DATA_HOME/update-agents/agents.d`,
/// falling back to `~/.local/share/update-agents/agents.d` when the env var
/// is unset, empty or relative (same resolution as the `--help` PATHS text).
fn builtin_dir() -> io::Result<PathBuf> {
    base_dir("XDG_DATA_HOME", ".local/share")
}

/// User descriptor directory: `$XDG_CONFIG_HOME/update-agents/agents.d`,
/// falling back to `~/.config/update-agents/agents.d` under the same rules.
fn user_dir() -> io::Result<PathBuf> {
    base_dir("XDG_CONFIG_HOME", ".config")
}

/// Shared XDG base-directory resolution. Delegates to the crate-root
/// `xdg_dir` so the catalogue defaults always match the documented PATHS;
/// its error is the meaningful "HOME is not set to an absolute path".
fn base_dir(var: &str, fallback: &str) -> io::Result<PathBuf> {
    crate::xdg_dir(var, fallback)
        .map(|dir| dir.join("update-agents").join("agents.d"))
        .map_err(invalid)
}

/// Read every `*.json` in `dir`, sorted by file name. The built-in directory
/// is required; the user directory may be missing or empty.
fn load_dir(
    dir: &Path,
    required: bool,
    specs: &mut Vec<ToolSpec>,
    seen: &mut HashMap<String, PathBuf>,
) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if required {
                return Err(io::Error::new(
                    e.kind(),
                    format!("agent directory not found: {} ({e})", dir.display()),
                ));
            }
            return Ok(());
        }
        Err(e) => {
            return Err(io::Error::new(
                e.kind(),
                format!("cannot read agent directory {}: {e}", dir.display()),
            ));
        }
    };

    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("cannot read agent directory {}: {e}", dir.display()),
            )
        })?;
        let path = entry.path();
        if is_descriptor_file(&path) {
            files.push(path);
        }
    }
    files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    for path in files {
        let spec = read_spec(&path)?;
        if let Some(first) = seen.get(&spec.id) {
            return Err(invalid(format!(
                "duplicate agent id '{}': defined in both {} and {}",
                spec.id,
                first.display(),
                path.display()
            )));
        }
        seen.insert(spec.id.clone(), path);
        specs.push(spec);
    }
    Ok(())
}

fn is_descriptor_file(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("json"))
        && fs::metadata(path).map(|m| m.is_file()).unwrap_or(false)
}

fn read_spec(path: &Path) -> io::Result<ToolSpec> {
    let text = fs::read_to_string(path)
        .map_err(|e| invalid(format!("cannot read {}: {e}", path.display())))?;
    let def: AgentDef = serde_json::from_str(&text)
        .map_err(|e| invalid(format!("cannot parse {}: {e}", path.display())))?;
    build_spec(def).map_err(|e| invalid(format!("{}: {e}", path.display())))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AgentDef {
    schema_version: u32,
    id: String,
    label: String,
    installed: String,
    update: CommandDef,
    #[serde(default)]
    version: Option<CommandDef>,
    #[serde(default)]
    version_line: Option<usize>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    failure_contains: Vec<String>,
    #[serde(default)]
    checks: Vec<CheckDef>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommandDef {
    program: String,
    #[serde(default)]
    args: Vec<String>,
}

/// Safety check as written in a descriptor. Parsed as a flat struct so
/// unknown fields are rejected reliably, then validated per kind.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckDef {
    kind: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default, rename = "cmdline_contains")]
    cmdline_contains: Option<Vec<String>>,
}

/// Validated safety check, ready to evaluate.
enum Check {
    GitClean { path: PathBuf },
    ProcessAbsent { cmdline_contains: Vec<String> },
}

fn build_spec(def: AgentDef) -> io::Result<ToolSpec> {
    if def.schema_version != SUPPORTED_SCHEMA {
        return Err(invalid(format!(
            "unsupported schema_version {} (expected {SUPPORTED_SCHEMA})",
            def.schema_version
        )));
    }
    validate_id(&def.id)?;
    if def.label.trim().is_empty() {
        return Err(invalid("label must be non-empty"));
    }
    if def.installed.is_empty() || def.installed.contains('\0') {
        return Err(invalid(
            "installed must be a non-empty executable name or path",
        ));
    }
    validate_command(&def.update, "update")?;
    if let Some(version) = &def.version {
        validate_command(version, "version")?;
    }
    let version_line = def.version_line.unwrap_or(0);
    if version_line > MAX_VERSION_LINE {
        return Err(invalid(format!(
            "version_line {version_line} exceeds {MAX_VERSION_LINE}"
        )));
    }
    let resource = def.resource.unwrap_or_else(|| def.id.clone());
    if resource.trim().is_empty() {
        return Err(invalid("resource must be non-empty"));
    }
    validate_literals(&def.failure_contains, "failure_contains")?;
    // The engine's marker scan (scan_log_for_markers) fails verification for
    // any failure_contains marker longer than its private MAX_MARKER_BYTES;
    // reject over-long markers here so a bad descriptor is a load-time
    // config error instead of a runtime failed verification.
    if let Some(marker) = def.failure_contains.iter().find(|m| m.len() > 4096) {
        return Err(invalid(format!(
            "failure_contains entry is {} bytes; the limit is 4096 (engine MAX_MARKER_BYTES)",
            marker.len()
        )));
    }
    let checks: Vec<Check> = def
        .checks
        .iter()
        .map(parse_check)
        .collect::<io::Result<Vec<_>>>()?;

    // Absence is a skipped state, never an install prompt. Resolution errors
    // above are definition bugs and abort loading instead. The first missing
    // executable names the reason; later gaps cannot override it.
    let mut preflight = Preflight::Ready;

    if resolve_program(&def.installed)?.is_none() {
        preflight = Preflight::Skipped(not_found_reason("executable", &def.installed));
    }

    let update_resolved = resolve_program(&def.update.program)?;
    if update_resolved.is_none() && matches!(preflight, Preflight::Ready) {
        preflight = Preflight::Skipped(not_found_reason("update program", &def.update.program));
    }

    let version_resolved = match &def.version {
        Some(version) => {
            let resolved = resolve_program(&version.program)?;
            if resolved.is_none() && matches!(preflight, Preflight::Ready) {
                preflight =
                    Preflight::Skipped(not_found_reason("version program", &version.program));
            }
            resolved
        }
        None => None,
    };

    // Read-only safety checks run only when every executable resolves: a
    // tool that cannot run cannot be made unsafe by a dirty worktree or a
    // live process. Any inability to establish safety blocks it.
    if matches!(preflight, Preflight::Ready) {
        for check in &checks {
            if let Some(reason) = evaluate_check(check) {
                preflight = Preflight::Blocked(reason);
                break;
            }
        }
    }

    let update_program = program_string(&update_resolved, &def.update.program);
    let version = def.version.map(|version| CommandSpec {
        program: program_string(&version_resolved, &version.program),
        args: version.args,
    });

    Ok(ToolSpec {
        id: def.id,
        label: def.label,
        update: CommandSpec {
            program: update_program,
            args: def.update.args,
        },
        version,
        resource,
        version_line,
        failure_contains: def.failure_contains,
        preflight,
    })
}

fn validate_id(id: &str) -> io::Result<()> {
    let bad = || {
        invalid(format!(
            "invalid agent id '{id}': use 1-{MAX_ID_LEN} ASCII chars [A-Za-z0-9._-] starting with a letter or digit"
        ))
    };
    if id.is_empty() || id.len() > MAX_ID_LEN {
        return Err(bad());
    }
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return Err(bad()),
    }
    if !chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')) {
        return Err(bad());
    }
    Ok(())
}

fn validate_command(cmd: &CommandDef, role: &str) -> io::Result<()> {
    if cmd.program.is_empty() || cmd.program.contains('\0') {
        return Err(invalid(format!(
            "{role} program must be a non-empty executable name or absolute path"
        )));
    }
    for arg in &cmd.args {
        if arg.is_empty() || arg.contains('\0') {
            return Err(invalid(format!(
                "{role} arguments must be non-empty and free of NUL bytes"
            )));
        }
    }
    Ok(())
}

fn validate_literals(list: &[String], field: &str) -> io::Result<()> {
    for item in list {
        // An empty literal would match any output and flip successes to
        // failures, so it is rejected outright.
        if item.is_empty() || item.contains('\0') {
            return Err(invalid(format!(
                "{field} entries must be non-empty and free of NUL bytes"
            )));
        }
    }
    Ok(())
}

fn invalid(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

/// Expand a leading `~`, `~/` or `$HOME/` in a path value. Nothing else is
/// expanded: descriptor fields never get shell-style substitution.
fn expand_home(value: &str) -> Option<PathBuf> {
    let home = env::var_os("HOME").filter(|h| !h.is_empty())?;
    let home = PathBuf::from(home);
    if value == "~" || value == "$HOME" {
        return Some(home);
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return Some(home.join(rest));
    }
    if let Some(rest) = value.strip_prefix("$HOME/") {
        return Some(home.join(rest));
    }
    None
}

/// Directories searched for bare program names: PATH first, then the known
/// version-manager bin dirs that are commonly absent from non-login PATH.
fn program_search_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(path) = env::var_os("PATH") {
        dirs.extend(env::split_paths(&path).filter(|d| !d.as_os_str().is_empty()));
    }
    if let Some(home) = env::var_os("HOME").filter(|h| !h.is_empty()) {
        let home = PathBuf::from(home);
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join(".grok/bin"));
        dirs.push(home.join(".bun/bin"));
    }
    dirs
}

fn is_executable_file(path: &Path) -> bool {
    match fs::metadata(path) {
        Ok(md) => md.is_file() && md.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

/// Resolve a program reference to an absolute, non-canonicalized path.
/// Path-like values (~, $HOME, absolute) are used as authored; bare names are
/// looked up in PATH and the known bin dirs. Symlink paths are kept as-is so
/// in-place upgrades keep working through the same symlink. `Ok(None)` means
/// "not found"; `Err` marks the definition itself invalid.
fn resolve_program(value: &str) -> io::Result<Option<PathBuf>> {
    if value.contains('/') {
        let path = match expand_home(value) {
            Some(path) => path,
            None => PathBuf::from(value),
        };
        if !path.is_absolute() {
            return Err(invalid(format!(
                "program '{value}' looks like a path but is not absolute; use ~, $HOME or an absolute path"
            )));
        }
        return Ok(if is_executable_file(&path) {
            Some(path)
        } else {
            None
        });
    }
    for dir in program_search_dirs() {
        let candidate = dir.join(value);
        if is_executable_file(&candidate) {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn program_string(resolved: &Option<PathBuf>, original: &str) -> String {
    match resolved {
        Some(path) => path.to_string_lossy().into_owned(),
        None => original.to_string(),
    }
}

fn not_found_reason(what: &str, name: &str) -> String {
    format!("{what} '{name}' not found in PATH or ~/.local/bin, ~/.grok/bin, ~/.bun/bin")
}

fn parse_check(def: &CheckDef) -> io::Result<Check> {
    match def.kind.as_str() {
        "git_clean" => {
            if def.cmdline_contains.is_some() {
                return Err(invalid("git_clean check must not set cmdline_contains"));
            }
            let raw = match def.path.as_deref() {
                Some(path) if !path.trim().is_empty() => path,
                _ => return Err(invalid("git_clean check requires a non-empty 'path'")),
            };
            if raw.contains('\0') {
                return Err(invalid("git_clean path must be free of NUL bytes"));
            }
            let path = match expand_home(raw) {
                Some(path) => path,
                None => PathBuf::from(raw),
            };
            if !path.is_absolute() {
                return Err(invalid(format!(
                    "git_clean path '{raw}' must start with ~, $HOME or be absolute"
                )));
            }
            Ok(Check::GitClean { path })
        }
        "process_absent" => {
            if def.path.is_some() {
                return Err(invalid("process_absent check must not set 'path'"));
            }
            let patterns = match def.cmdline_contains.as_deref() {
                Some(patterns) => patterns,
                None => return Err(invalid("process_absent check requires 'cmdline_contains'")),
            };
            if patterns.is_empty() {
                return Err(invalid(
                    "process_absent check requires at least one cmdline_contains entry",
                ));
            }
            validate_literals(patterns, "cmdline_contains")?;
            Ok(Check::ProcessAbsent {
                cmdline_contains: patterns.to_vec(),
            })
        }
        other => Err(invalid(format!("unknown safety check kind '{other}'"))),
    }
}

fn evaluate_check(check: &Check) -> Option<String> {
    match check {
        Check::GitClean { path } => check_git_clean(path),
        Check::ProcessAbsent { cmdline_contains } => check_process_absent(cmdline_contains),
    }
}

/// `git status --porcelain` (untracked included) on the descriptor's
/// worktree must produce no output. `--no-optional-locks` keeps the check
/// free of index writes. Any failure to establish cleanliness blocks.
fn check_git_clean(path: &Path) -> Option<String> {
    if !path.is_dir() {
        return Some(format!(
            "git_clean: {} does not exist or is not a directory",
            path.display()
        ));
    }
    let git = match resolve_program("git") {
        Ok(Some(git)) => git,
        Ok(None) => return Some("git_clean: git executable not found".to_string()),
        Err(e) => return Some(format!("git_clean: {e}")),
    };
    let mut child = match Command::new(&git)
        .args(["--no-optional-locks", "-C"])
        .arg(path)
        .args(["status", "--porcelain"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Some(format!("git_clean: cannot run git: {e}")),
    };

    let stdout = bounded_read(child.stdout.take(), GIT_STDOUT_CAP);
    let stderr = bounded_read(child.stderr.take(), GIT_STDERR_CAP);

    let deadline = Instant::now() + GIT_CHECK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("git status timed out".to_string());
            }
            Ok(None) => thread::sleep(GIT_CHECK_POLL),
            Err(e) => {
                let _ = child.kill();
                break Err(format!("cannot wait for git: {e}"));
            }
        }
    };

    let out = match join_bytes(stdout, "git status output") {
        Ok(bytes) => bytes,
        Err(reason) => return Some(format!("git_clean: {reason}")),
    };

    match status {
        Err(reason) => Some(format!("git_clean: {reason}")),
        Ok(status) if !status.success() => {
            let detail = join_bytes(stderr, "git status errors")
                .ok()
                .map(|bytes| one_line(&bytes))
                .unwrap_or_default();
            if detail.is_empty() {
                Some(format!(
                    "git_clean: git status failed with {}",
                    exit_text(status.code())
                ))
            } else {
                Some(format!(
                    "git_clean: git status failed with {}: {detail}",
                    exit_text(status.code())
                ))
            }
        }
        Ok(_) if out.is_empty() => None,
        Ok(_) => Some(format!(
            "git_clean: {} has uncommitted changes ({})",
            path.display(),
            one_line(&out)
        )),
    }
}

/// Own-user processes whose full command line contains every listed literal
/// mark the tool unsafe. Failure to inspect the process table, or to read the
/// command line of an own-user process, fails closed. The blocked reason
/// names the PID and the configured patterns but never the command line
/// itself: arguments can embed API keys or other private launch details.
#[cfg(target_os = "macos")]
fn check_process_absent(patterns: &[String]) -> Option<String> {
    check_process_absent_ps(patterns)
}

#[cfg(not(target_os = "macos"))]
fn check_process_absent(patterns: &[String]) -> Option<String> {
    check_process_absent_procfs(patterns)
}

/// macOS has no `/proc`; the process table comes from
/// `ps -axo pid=,uid=,command=`. Output lines look like
/// `  1234   501 /bin/launchd ...`: a pid and uid followed by the full command
/// line, separated by padding spaces. Lines without two numeric fields cannot
/// name an own process and are skipped; spawn, wait, and read failures fail
/// closed, like the Linux `/proc` path.
#[cfg(target_os = "macos")]
fn check_process_absent_ps(patterns: &[String]) -> Option<String> {
    let mut child = match Command::new("ps")
        .args(["-axo", "pid=,uid=,command="])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return Some(format!("process_absent: cannot run ps: {e}")),
    };

    let stdout = bounded_read(child.stdout.take(), CMDLINE_CAP);
    let stderr = bounded_read(child.stderr.take(), CMDLINE_CAP);

    let deadline = Instant::now() + GIT_CHECK_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break Err("ps timed out".to_string());
            }
            Ok(None) => thread::sleep(GIT_CHECK_POLL),
            Err(e) => {
                let _ = child.kill();
                break Err(format!("cannot wait for ps: {e}"));
            }
        }
    };

    let out = match join_bytes(stdout, "ps output") {
        Ok(bytes) => bytes,
        Err(reason) => return Some(format!("process_absent: {reason}")),
    };

    let status = match status {
        Err(reason) => return Some(format!("process_absent: {reason}")),
        Ok(status) => status,
    };
    if !status.success() {
        let detail = join_bytes(stderr, "ps errors")
            .ok()
            .map(|bytes| one_line(&bytes))
            .unwrap_or_default();
        return Some(if detail.is_empty() {
            format!(
                "process_absent: ps failed with {}",
                exit_text(status.code())
            )
        } else {
            format!(
                "process_absent: ps failed with {}: {detail}",
                exit_text(status.code())
            )
        });
    }

    let own_uid = unsafe { libc::getuid() };
    for line in String::from_utf8_lossy(&out).lines() {
        let mut fields = line.trim_start().splitn(3, char::is_whitespace);
        let pid: u32 = match fields.next().and_then(|field| field.parse().ok()) {
            Some(pid) => pid,
            // Not a process line: skip rather than fail on unrelated text.
            None => continue,
        };
        let uid: u32 = match fields.next().and_then(|field| field.parse().ok()) {
            Some(uid) => uid,
            None => continue,
        };
        if uid != own_uid {
            continue;
        }
        let cmdline = match fields.next() {
            Some(cmdline) => cmdline,
            None => {
                return Some(format!(
                    "process_absent: cannot read command line of own process {pid}"
                ));
            }
        };
        if cmdline.is_empty() {
            continue;
        }
        if patterns
            .iter()
            .all(|pattern| cmdline.contains(pattern.as_str()))
        {
            return Some(format!(
                "process_absent: live process {pid} matches [{}]",
                patterns.join(", ")
            ));
        }
    }
    None
}

/// Linux implementation: `/proc/<pid>/cmdline` walk, unchanged.
#[cfg(not(target_os = "macos"))]
fn check_process_absent_procfs(patterns: &[String]) -> Option<String> {
    let own_uid = match fs::metadata("/proc/self") {
        Ok(md) => md.uid(),
        Err(e) => return Some(format!("process_absent: cannot identify own user: {e}")),
    };
    let entries = match fs::read_dir("/proc") {
        Ok(entries) => entries,
        Err(e) => return Some(format!("process_absent: cannot inspect /proc: {e}")),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let pid: u32 = match entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse().ok())
        {
            Some(pid) => pid,
            None => continue,
        };
        let dir = entry.path();
        let uid = match fs::metadata(&dir) {
            Ok(md) => md.uid(),
            // Another user's process (hidepid) or one that just exited.
            Err(_) => continue,
        };
        if uid != own_uid {
            continue;
        }
        let cmdline = match read_cmdline(&dir) {
            Ok(cmdline) => cmdline,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue, // exited meanwhile
            Err(e) => {
                return Some(format!(
                    "process_absent: cannot read cmdline of own process {pid}: {e}"
                ));
            }
        };
        if cmdline.is_empty() {
            continue; // kernel thread or zombie
        }
        if patterns
            .iter()
            .all(|pattern| cmdline.contains(pattern.as_str()))
        {
            return Some(format!(
                "process_absent: live process {pid} matches [{}]",
                patterns.join(", ")
            ));
        }
    }
    None
}

/// /proc/<pid>/cmdline is NUL-separated argv; render args space-joined so
/// configured literals match within a single argument.
#[cfg(not(target_os = "macos"))]
fn read_cmdline(dir: &Path) -> io::Result<String> {
    let mut file = fs::File::open(dir.join("cmdline"))?;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        match file.read(&mut chunk)? {
            0 => break,
            n => {
                let retain = CMDLINE_CAP.saturating_sub(buf.len());
                if retain > 0 {
                    buf.extend_from_slice(&chunk[..n.min(retain)]);
                }
                if buf.len() >= CMDLINE_CAP {
                    break;
                }
            }
        }
    }
    let args: Vec<String> = buf
        .split(|&byte| byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    Ok(args.join(" "))
}

/// Drain a pipe on a helper thread, retaining at most `cap` bytes so a huge
/// output can never balloon memory or deadlock the writer.
fn bounded_read<R>(stream: Option<R>, cap: usize) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut stream = match stream {
            Some(stream) => stream,
            None => return Ok(Vec::new()),
        };
        let mut buf = Vec::with_capacity(8192.min(cap));
        let mut chunk = [0u8; 8192];
        loop {
            match stream.read(&mut chunk) {
                Ok(0) => return Ok(buf),
                Ok(n) => {
                    let retain = cap.saturating_sub(buf.len());
                    if retain > 0 {
                        buf.extend_from_slice(&chunk[..n.min(retain)]);
                    }
                    // Keep draining past the cap so the writer never blocks.
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
    })
}

fn join_bytes(handle: JoinHandle<io::Result<Vec<u8>>>, what: &str) -> Result<Vec<u8>, String> {
    match handle.join() {
        Ok(Ok(bytes)) => Ok(bytes),
        Ok(Err(e)) => Err(format!("cannot read {what}: {e}")),
        Err(_) => Err(format!("{what} reader failed")),
    }
}

fn exit_text(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("exit status {code}"),
        None => "a signal".to_string(),
    }
}

/// First line of raw output, lossy, control-free, bounded - for reason
/// strings shown in the UI.
fn one_line(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut line = String::new();
    for c in text.chars() {
        if c == '\n' || c == '\r' {
            if !line.is_empty() {
                break;
            }
            continue;
        }
        if c.is_control() {
            line.push(' ');
        } else {
            line.push(c);
        }
    }
    line.trim().chars().take(REASON_CAP).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static SEQ: AtomicUsize = AtomicUsize::new(0);

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> io::Result<TempDir> {
            let n = SEQ.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0);
            let dir = env::temp_dir().join(format!(
                "update-agents-catalog-{tag}-{}-{n}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&dir)?;
            Ok(TempDir(dir))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn write_file(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).expect("write test file");
    }

    /// A real executable file the loader can stat; never executed.
    fn fake_exe(dir: &Path, name: &str) -> String {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write test exe");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod test exe");
        path.to_str().expect("utf8 temp path").to_string()
    }

    fn descriptor_json(id: &str, program: &str) -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "id": id,
            "label": format!("Label {id}"),
            "installed": program,
            "update": {"program": program, "args": ["update"]},
            "version": {"program": program, "args": ["--version"]},
            "version_line": 0,
            "resource": format!("res-{id}"),
            "failure_contains": [],
            "checks": []
        })
    }

    #[test]
    fn adding_a_descriptor_file_extends_the_catalogue() {
        let dir = TempDir::new("extend").expect("temp dir");
        let exe = fake_exe(dir.path(), "tool-bin");

        let specs = load(Some(dir.path())).expect("empty explicit dir loads");
        assert!(specs.is_empty());

        write_file(
            dir.path(),
            "b.json",
            &descriptor_json("beta", &exe).to_string(),
        );
        let specs = load(Some(dir.path())).expect("one descriptor loads");
        assert_eq!(specs.len(), 1);
        assert_eq!(specs[0].id, "beta");

        // A new file extends the catalogue with zero code changes.
        write_file(
            dir.path(),
            "a.json",
            &descriptor_json("alpha", &exe).to_string(),
        );
        let specs = load(Some(dir.path())).expect("two descriptors load");
        let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["alpha", "beta"],
            "descriptors load in sorted file order"
        );
    }

    #[test]
    fn malformed_json_fails_closed() {
        let dir = TempDir::new("malformed").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        write_file(
            dir.path(),
            "ok.json",
            &descriptor_json("ok", &exe).to_string(),
        );
        write_file(dir.path(), "broken.json", "{ not json");
        assert!(
            load(Some(dir.path())).is_err(),
            "one malformed descriptor must abort loading"
        );
    }

    #[test]
    fn unknown_fields_fail_closed() {
        let dir = TempDir::new("unknown-field").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("x", &exe);
        def["surprise"] = serde_json::json!(1);
        write_file(dir.path(), "x.json", &def.to_string());
        assert!(
            load(Some(dir.path())).is_err(),
            "unknown fields must be rejected"
        );
    }

    #[test]
    fn unknown_check_kind_fails_closed() {
        let dir = TempDir::new("check-kind").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("y", &exe);
        def["checks"] = serde_json::json!([{"kind": "martian", "path": "/tmp"}]);
        write_file(dir.path(), "y.json", &def.to_string());
        assert!(load(Some(dir.path())).is_err());
    }

    #[test]
    fn check_field_mismatch_fails_closed() {
        let dir = TempDir::new("check-fields").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("z", &exe);
        def["checks"] =
            serde_json::json!([{"kind": "git_clean", "path": "/tmp", "cmdline_contains": ["x"]}]);
        write_file(dir.path(), "z.json", &def.to_string());
        assert!(load(Some(dir.path())).is_err());
    }

    #[test]
    fn duplicate_ids_fail_closed() {
        let dir = TempDir::new("dup").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        write_file(
            dir.path(),
            "a.json",
            &descriptor_json("dup", &exe).to_string(),
        );
        write_file(
            dir.path(),
            "b.json",
            &descriptor_json("dup", &exe).to_string(),
        );
        assert!(
            load(Some(dir.path())).is_err(),
            "duplicate ids must abort loading"
        );
    }

    #[test]
    fn unsafe_ids_fail_closed() {
        let exe_dir = TempDir::new("unsafe-id-exe").expect("temp dir");
        let exe = fake_exe(exe_dir.path(), "bin");
        for id in ["../escape", "has space", ""] {
            let dir = TempDir::new("unsafe-id").expect("temp dir");
            write_file(dir.path(), "x.json", &descriptor_json(id, &exe).to_string());
            assert!(
                load(Some(dir.path())).is_err(),
                "id '{id}' must be rejected"
            );
        }
    }

    /// Reason string of a skipped preflight; panics on any other kind.
    fn skipped_reason(spec: &ToolSpec) -> &str {
        match &spec.preflight {
            Preflight::Skipped(reason) => reason,
            other => panic!("expected a skipped preflight, got {other:?}"),
        }
    }

    /// Reason string of a blocked preflight; panics on any other kind.
    fn blocked_reason(spec: &ToolSpec) -> &str {
        match &spec.preflight {
            Preflight::Blocked(reason) => reason,
            other => panic!("expected a blocked preflight, got {other:?}"),
        }
    }

    #[test]
    fn missing_installed_marks_skipped() {
        let dir = TempDir::new("not-installed").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("ghost", &exe);
        def["installed"] = serde_json::json!("update-agents-missing-binary-9d4e1f");
        write_file(dir.path(), "ghost.json", &def.to_string());
        let specs = load(Some(dir.path())).expect("loading succeeds; the tool is skipped");
        assert_eq!(specs.len(), 1);
        let reason = skipped_reason(&specs[0]);
        assert!(
            reason.contains("update-agents-missing-binary-9d4e1f"),
            "reason: {reason}"
        );
    }

    #[test]
    fn missing_program_marks_skipped() {
        let dir = TempDir::new("no-program").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("h", &exe);
        def["update"] =
            serde_json::json!({"program": "update-agents-missing-cmd-7c2b90", "args": ["update"]});
        write_file(dir.path(), "h.json", &def.to_string());
        let specs = load(Some(dir.path())).expect("loading succeeds; the tool is skipped");
        let reason = skipped_reason(&specs[0]);
        assert!(
            reason.contains("update-agents-missing-cmd-7c2b90"),
            "reason: {reason}"
        );
    }

    #[test]
    fn git_clean_on_missing_path_blocks() {
        let dir = TempDir::new("git-clean").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let missing = dir.path().join("no-such-repo");
        let mut def = descriptor_json("g", &exe);
        def["checks"] =
            serde_json::json!([{"kind": "git_clean", "path": missing.to_str().expect("utf8")}]);
        write_file(dir.path(), "g.json", &def.to_string());
        let specs = load(Some(dir.path())).expect("load");
        let reason = blocked_reason(&specs[0]);
        assert!(reason.contains("git_clean"), "reason: {reason}");
    }

    #[test]
    fn process_absent_without_match_is_safe() {
        let dir = TempDir::new("proc").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("p", &exe);
        def["checks"] = serde_json::json!([
            {"kind": "process_absent", "cmdline_contains": ["update-agents-catalog-probe-3f81c9"]}
        ]);
        write_file(dir.path(), "p.json", &def.to_string());
        let specs = load(Some(dir.path())).expect("load");
        assert!(
            matches!(specs[0].preflight, Preflight::Ready),
            "no matching process must not block"
        );
    }

    /// A missing executable is a skip, not a block: safety checks are not
    /// evaluated for a tool that could not run anyway, so a failing check
    /// must not turn the skip into a blocked status.
    #[test]
    fn missing_executable_is_skipped_even_with_failing_check() {
        let dir = TempDir::new("skip-beats-check").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let missing = dir.path().join("no-such-repo");
        let mut def = descriptor_json("ghost", &exe);
        def["installed"] = serde_json::json!("update-agents-missing-binary-5a7c22");
        def["checks"] =
            serde_json::json!([{"kind": "git_clean", "path": missing.to_str().expect("utf8")}]);
        write_file(dir.path(), "ghost.json", &def.to_string());
        let specs = load(Some(dir.path())).expect("load");
        let reason = skipped_reason(&specs[0]);
        assert!(
            reason.contains("update-agents-missing-binary-5a7c22"),
            "reason: {reason}"
        );
    }

    #[test]
    fn wrong_schema_version_fails_closed() {
        let dir = TempDir::new("schema").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("s", &exe);
        def["schema_version"] = serde_json::json!(2);
        write_file(dir.path(), "s.json", &def.to_string());
        assert!(load(Some(dir.path())).is_err());
    }

    #[test]
    fn empty_failure_literal_fails_closed() {
        let dir = TempDir::new("empty-literal").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");
        let mut def = descriptor_json("e", &exe);
        def["failure_contains"] = serde_json::json!([""]);
        write_file(dir.path(), "e.json", &def.to_string());
        assert!(
            load(Some(dir.path())).is_err(),
            "an empty literal would match any output"
        );
    }

    #[test]
    fn failure_contains_marker_length_boundary() {
        let dir = TempDir::new("marker-length").expect("temp dir");
        let exe = fake_exe(dir.path(), "bin");

        // Exactly at the engine's MAX_MARKER_BYTES cap the marker stays
        // live, so the descriptor must load.
        let mut def = descriptor_json("edge", &exe);
        def["failure_contains"] = serde_json::json!(["x".repeat(4096)]);
        write_file(dir.path(), "edge.json", &def.to_string());
        let specs = load(Some(dir.path())).expect("4096-byte marker loads");
        assert_eq!(specs[0].failure_contains, vec!["x".repeat(4096)]);

        // One byte over: the engine would fail verification at runtime, so
        // loading must already reject the descriptor.
        let mut def = descriptor_json("over", &exe);
        def["failure_contains"] = serde_json::json!(["x".repeat(4097)]);
        write_file(dir.path(), "over.json", &def.to_string());
        let err = load(Some(dir.path())).expect_err("4097-byte marker must fail to load");
        assert!(
            err.to_string().contains("4096"),
            "reason should cite the limit: {err}"
        );
    }

    #[test]
    fn missing_explicit_directory_fails() {
        let dir = TempDir::new("missing-dir").expect("temp dir");
        let absent = dir.path().join("agents.d");
        assert!(load(Some(absent.as_path())).is_err());
    }

    #[test]
    fn user_descriptors_merge_and_duplicates_fail_closed() {
        let builtin = TempDir::new("builtin").expect("temp dir");
        let user = TempDir::new("user").expect("temp dir");
        let exe = fake_exe(builtin.path(), "bin");
        write_file(
            builtin.path(),
            "a.json",
            &descriptor_json("alpha", &exe).to_string(),
        );
        write_file(
            user.path(),
            "b.json",
            &descriptor_json("beta", &exe).to_string(),
        );
        let specs = load_from(Some(builtin.path()), Some(user.path())).expect("merged load");
        let ids: Vec<&str> = specs.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, vec!["alpha", "beta"]);

        write_file(
            user.path(),
            "a2.json",
            &descriptor_json("alpha", &exe).to_string(),
        );
        assert!(
            load_from(Some(builtin.path()), Some(user.path())).is_err(),
            "a user id colliding with a builtin id must fail closed"
        );
    }

    #[test]
    fn missing_user_directory_is_allowed() {
        let builtin = TempDir::new("builtin-only").expect("temp dir");
        let exe = fake_exe(builtin.path(), "bin");
        write_file(
            builtin.path(),
            "a.json",
            &descriptor_json("alpha", &exe).to_string(),
        );
        let absent = builtin.path().join("no-such-user-dir");
        let specs = load_from(Some(builtin.path()), Some(absent.as_path()))
            .expect("missing user dir is allowed");
        assert_eq!(specs.len(), 1);
    }
}
