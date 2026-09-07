use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, Serialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Preflight {
    Ready,
    Skipped(String),
    Blocked(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct ToolSpec {
    pub id: String,
    pub label: String,
    pub update: CommandSpec,
    pub version: Option<CommandSpec>,
    pub resource: String,
    pub version_line: usize,
    pub failure_contains: Vec<String>,
    pub preflight: Preflight,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Running,
    Succeeded,
    Failed,
    Blocked,
    Skipped,
    Cancelled,
    TimedOut,
}

impl Status {
    /// True when a job must stay out of every human-visible list: not-detected
    /// tools are only shown when their IDs were explicitly requested.
    pub fn hidden_from_list(self, explicit: bool) -> bool {
        !explicit && self == Status::Skipped
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub spec: ToolSpec,
    pub status: Status,
    pub started: Option<Instant>,
    pub elapsed: Duration,
    pub before: String,
    pub after: String,
    pub message: String,
    pub log: PathBuf,
    pub exit_code: Option<i32>,
}

#[derive(Clone, Debug)]
pub struct RunState {
    pub jobs: Vec<Job>,
    pub started: Instant,
    pub done: bool,
    pub run_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RunOptions {
    pub jobs: usize,
    pub timeout: Duration,
    pub run_dir: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::Status;

    #[test]
    fn only_not_detected_jobs_are_hidden_and_only_without_explicit_ids() {
        // Full-catalogue run: undetected executables stay out of the list.
        assert!(Status::Skipped.hidden_from_list(false));
        // Explicitly requested IDs always get an answer, never silence.
        assert!(!Status::Skipped.hidden_from_list(true));
        // Every other status stays visible in both modes.
        for status in [
            Status::Queued,
            Status::Running,
            Status::Succeeded,
            Status::Failed,
            Status::Blocked,
            Status::Cancelled,
            Status::TimedOut,
        ] {
            assert!(
                !status.hidden_from_list(false),
                "{status:?} must stay visible"
            );
            assert!(
                !status.hidden_from_list(true),
                "{status:?} must stay visible"
            );
        }
    }
}
