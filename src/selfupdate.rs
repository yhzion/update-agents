//! Version notice and self-update from GitHub release assets.
//!
//! The release workflow attaches raw `update-agents-<target>` binaries plus a
//! `sha256sums.txt`. `self-update` downloads the asset matching the running
//! OS/arch, verifies its SHA-256, and atomically renames it over the current
//! executable (the running inode stays mapped, so the swap is safe on Unix).
//!
//! Startup checks are best-effort and cached once per day: no curl, no
//! network, or an unwritable state directory never blocks or fails a run.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

const LATEST_URL: &str = "https://api.github.com/repos/yhzion/update-agents/releases/latest";
const CHECK_FILE: &str = "update-check.json";
const CHECK_TTL_SECS: u64 = 24 * 60 * 60;
const CURL_TIMEOUT_SECS: &str = "8";
const DOWNLOAD_TIMEOUT_SECS: &str = "120";

/// Parsed `major.minor.patch` release version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
}

impl Version {
    pub fn current() -> Self {
        Self::parse(env!("CARGO_PKG_VERSION")).expect("package version is valid")
    }

    fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text
            .strip_prefix('v')
            .or_else(|| text.strip_prefix('V'))
            .unwrap_or(text);
        // Drop any pre-release or build metadata suffix (`-rc.1`, `+meta`).
        let core = text.split(['-', '+']).next().unwrap_or(text);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next().unwrap_or("0").parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Outcome of the startup check: whether this process performed a self-update
/// and must therefore not start any update work.
pub enum Startup {
    Continue,
    Updated,
}

#[derive(Default, Serialize, Deserialize)]
struct Cache {
    checked_at: u64,
    latest: String,
    /// Latest tag already surfaced (or declined), so it is not raised again.
    acknowledged_version: String,
}

/// Startup notice, then an interactive y/N self-update when requested. The
/// check itself is cached for a day; a declined or already-surfaced version is
/// not raised again until a newer one appears.
pub fn startup_check(interactive: bool) -> Startup {
    let Ok(state) = crate::state_dir() else {
        return Startup::Continue;
    };
    let current = Version::current();
    let mut cache = load_cache(&state);
    let now = now_secs();

    let tag = if !cache.latest.is_empty() && now.saturating_sub(cache.checked_at) < CHECK_TTL_SECS {
        cache.latest.clone()
    } else {
        match latest_tag() {
            Ok(tag) => {
                cache.checked_at = now;
                cache.latest = tag.clone();
                tag
            }
            // Best-effort: offline or curl-less machines stay silent.
            Err(_) => return Startup::Continue,
        }
    };

    let newer = Version::parse(&tag).is_some_and(|latest| latest > current);
    if !newer || cache.acknowledged_version == tag {
        save_cache(&state, &cache);
        return Startup::Continue;
    }
    cache.acknowledged_version = tag.clone();
    save_cache(&state, &cache);

    if !interactive {
        println!(
            "update-agents: {tag} is available (you have {current}); \
             update with `update-agents self-update`."
        );
        return Startup::Continue;
    }

    print!("update-agents: {tag} is available (you have {current}). Update now? [y/N] ");
    let _ = io::stdout().flush();
    let mut answer = String::new();
    let _ = io::stdin().read_line(&mut answer);
    let yes = matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
    if !yes {
        println!(
            "update-agents: keeping {current}; update later with `update-agents self-update`."
        );
        return Startup::Continue;
    }
    match run_self_update() {
        Ok(()) => {
            println!("update-agents: updated to {tag}; the next run uses the new version.");
            Startup::Updated
        }
        Err(e) => {
            eprintln!("update-agents: self-update failed: {e}");
            Startup::Continue
        }
    }
}

/// `update-agents self-update`: fetch the latest release and replace this
/// executable. Always fresh, never cached.
pub fn run_self_update() -> Result<(), String> {
    let release = fetch_latest()?;
    let tag = release
        .get("tag_name")
        .and_then(Value::as_str)
        .ok_or("release metadata has no tag_name")?
        .to_string();
    let latest =
        Version::parse(&tag).ok_or_else(|| format!("release tag '{tag}' is not a version"))?;
    let current = Version::current();
    if latest <= current {
        println!("update-agents: already up to date (v{current})");
        return Ok(());
    }
    install_asset(&release, &tag, &latest)
}

fn install_asset(release: &Value, tag: &str, latest: &Version) -> Result<(), String> {
    let target = target_triple().ok_or_else(|| {
        format!(
            "no prebuilt binary for {}/{}",
            std::env::consts::OS,
            std::env::consts::ARCH
        )
    })?;
    let asset = format!("update-agents-{target}");
    let url = asset_url(release, &asset)
        .ok_or_else(|| format!("release {tag} has no asset named {asset}"))?;
    let sums_url = asset_url(release, "sha256sums.txt")
        .ok_or_else(|| format!("release {tag} has no sha256sums.txt"))?;
    let sums = curl_text(&sums_url)?;
    let expected = checksum_for(&sums, &asset)
        .ok_or_else(|| format!("sha256sums.txt has no entry for {asset}"))?;

    let exe =
        std::env::current_exe().map_err(|e| format!("cannot locate current executable: {e}"))?;
    let exe = fs::canonicalize(&exe).unwrap_or(exe);
    let dir = exe
        .parent()
        .ok_or_else(|| format!("cannot determine install directory of {}", exe.display()))?;
    let tmp = dir.join(format!(".update-agents.{}.tmp", std::process::id()));

    if let Err(e) = curl_file(&url, &tmp) {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    let result = (|| -> Result<(), String> {
        let actual = sha256_file(&tmp)?;
        if !actual.eq_ignore_ascii_case(expected.trim()) {
            return Err(format!(
                "checksum mismatch for {asset} (expected {}, got {actual})",
                expected.trim()
            ));
        }
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("cannot mark {} executable: {e}", tmp.display()))?;
        fs::rename(&tmp, &exe).map_err(|e| format!("cannot replace {}: {e}", exe.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    } else {
        println!("update-agents: installed v{latest} at {}", exe.display());
    }
    result
}

fn target_triple() -> Option<&'static str> {
    Some(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        _ => return None,
    })
}

fn fetch_latest() -> Result<Value, String> {
    let body = curl_text(LATEST_URL)?;
    serde_json::from_str(&body).map_err(|e| format!("cannot parse release metadata: {e}"))
}

fn latest_tag() -> Result<String, String> {
    fetch_latest()?
        .get("tag_name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| "release metadata has no tag_name".to_string())
}

fn asset_url(release: &Value, name: &str) -> Option<String> {
    release
        .get("assets")?
        .as_array()?
        .iter()
        .find(|a| a.get("name").and_then(Value::as_str) == Some(name))
        .and_then(|a| a.get("browser_download_url").and_then(Value::as_str))
        .map(str::to_string)
}

fn checksum_for<'a>(sums: &'a str, asset: &str) -> Option<&'a str> {
    sums.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        let hash = fields.next()?;
        let name = fields.next()?.trim_start_matches('*');
        (name == asset).then_some(hash)
    })
}

fn curl_text(url: &str) -> Result<String, String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            CURL_TIMEOUT_SECS,
            "-H",
            "Accept: application/vnd.github+json",
            url,
        ])
        .output()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl failed for {url} (status {})", out.status));
    }
    String::from_utf8(out.stdout).map_err(|_| "curl output is not UTF-8".to_string())
}

fn curl_file(url: &str, path: &Path) -> Result<(), String> {
    let status = Command::new("curl")
        .args(["-fsSL", "--max-time", DOWNLOAD_TIMEOUT_SECS, "-o"])
        .arg(path)
        .arg(url)
        .status()
        .map_err(|e| format!("cannot run curl: {e}"))?;
    if !status.success() {
        return Err(format!("download failed for {url} (status {status})"));
    }
    Ok(())
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn cache_path(state: &Path) -> PathBuf {
    state.join(CHECK_FILE)
}

fn load_cache(state: &Path) -> Cache {
    fs::read_to_string(cache_path(state))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_cache(state: &Path, cache: &Cache) {
    let Ok(text) = serde_json::to_string(cache) else {
        return;
    };
    let _ = fs::create_dir_all(state);
    let _ = fs::write(cache_path(state), text);
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_orders_release_tags() {
        assert_eq!(
            Version::parse("v0.1.2"),
            Some(Version {
                major: 0,
                minor: 1,
                patch: 2
            })
        );
        assert_eq!(
            Version::parse("1.2"),
            Some(Version {
                major: 1,
                minor: 2,
                patch: 0
            })
        );
        assert_eq!(
            Version::parse("v2.0.0-rc.1+meta"),
            Some(Version {
                major: 2,
                minor: 0,
                patch: 0
            })
        );
        assert!(Version::parse("not-a-version").is_none());
        assert!(Version::parse("v0.1.10").unwrap() > Version::parse("v0.1.9").unwrap());
        assert!(Version::parse("v0.10.0").unwrap() > Version::parse("v0.2.0").unwrap());
    }

    #[test]
    fn finds_checksum_with_and_without_star() {
        let sums = "abc123  update-agents-x86_64-unknown-linux-gnu\n\
                    def456 *update-agents-aarch64-apple-darwin\n";
        assert_eq!(
            checksum_for(sums, "update-agents-x86_64-unknown-linux-gnu"),
            Some("abc123")
        );
        assert_eq!(
            checksum_for(sums, "update-agents-aarch64-apple-darwin"),
            Some("def456")
        );
        assert_eq!(checksum_for(sums, "missing"), None);
    }
}
