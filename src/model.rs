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
