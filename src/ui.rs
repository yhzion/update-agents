//! Live TUI dashboard for update runs (Ratatui + crossterm).
//!
//! Contract notes:
//! - Consumes [`RunHandle`] read-only: snapshots `handle.state` under short
//!   locks and mutates only `handle.cancel`. The engine thread keeps running;
//!   `main` joins it via `RunHandle::wait` after this function returns.
//! - Starts drawing while the run is active, then shows a completion countdown.
//!   Any key exits after completion; without input it exits after five seconds.
//! - Refreshes at most 8 Hz while running and redraws only on real changes;
//!   the completion countdown redraws once per second. Detail logs
//!   come from `engine::tail` with bounded, rate-limited disk reads. The only
//!   progress indicator is the factual completed-tool count: no fabricated
//!   percentages anywhere.
//! - RAII (plus a panic hook) restores raw mode, the alternate screen and the
//!   cursor on every exit path. Terminal/output errors and panics inside the
//!   event loop cancel the run via `handle.cancel` and are returned to `main`
//!   as an error for reporting.
//! - Not-detected rows are hidden unless tools were selected explicitly; the
//!   footer names them.
//!
//! Key bindings: j/k or arrows select, Enter/l toggles the bounded detail
//! log, PgUp/PgDn scrolls it (pages the list otherwise), ? toggles help,
//! q/Esc asks for confirmation while running, Ctrl-C cancels immediately,
//! and after completion any key exits (automatically after five seconds).

use std::fmt::Write as _;
use std::io::{self, Stdout, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Gauge, List, ListItem, ListState, Padding, Paragraph};
use ratatui::{Frame, Terminal};

use crate::engine::RunHandle;
use crate::model::{Job, RunState, Status};

/// Cadence while the run is active: at most 8 snapshot/draw rounds per second.
const TICK: Duration = Duration::from_millis(125);
/// Countdown cadence once the run is finished.
const IDLE_POLL: Duration = Duration::from_secs(1);
const COMPLETION_WAIT: Duration = Duration::from_secs(5);
/// Minimum interval between detail-log disk reads while the run is active.
const DETAIL_REFRESH: Duration = Duration::from_millis(250);
/// Bound for detail-log disk reads (bytes of tail).
const DETAIL_BYTES: usize = 8 * 1024;
/// Bound for the per-row live activity read (bytes of tail).
const ACTIVITY_BYTES: usize = 192;
/// Longest activity text kept per row (rendering clips further per column).
const ACTIVITY_MAX: usize = 160;
/// Cap for per-row copies of engine-provided text (version/message strings).
const TEXT_CAP: usize = 256;
/// Prefix of the engine's internal per-job log header (see `write_log_header`
/// in engine.rs); hidden from the overview activity, kept in the detail log.
const LOG_HEADER_PREFIX: &str = "# update-agents job ";
/// Column width of the status cell; "not detected" is the longest word.
const STATUS_W: usize = 12;
/// Column width of the elapsed-time cell.
const TIME_W: usize = 8;
/// Minimum width for the activity column to be shown at all.
const ACTIVITY_MIN: usize = 10;
/// The one accent color of the dashboard.
const ACCENT: Color = Color::Cyan;

/// Terminal handle used by the dashboard.
type Term = Terminal<CrosstermBackend<Stdout>>;

/// Runs until completion followed by any key or a five-second countdown, or
/// until a terminal error occurs. Only the cancel flag can mutate engine state.
///
/// A panic anywhere in the snapshot/render/input path is caught here and
/// mapped into a terminal error: unwinding through `main` would kill the
/// process, release the single-instance lock and leave the engine's updater
/// children running invisibly. The panic hook has already restored the
/// terminal by the time the panic is caught.
pub fn run(handle: &RunHandle, explicit: bool) -> io::Result<()> {
    install_panic_hook();
    let mut term: Term = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    enable_raw_mode()?;
    let _guard = TermGuard;
    execute!(io::stdout(), EnterAlternateScreen)?;
    term.hide_cursor()?;

    let mut dash = Dashboard::new(handle, explicit);
    let outcome = catch_unwind(AssertUnwindSafe(|| event_loop(&mut dash, &mut term)));
    let outcome = match outcome {
        Ok(result) => result,
        Err(_) => Err(io::Error::other("terminal ui panicked; run cancelled")),
    };
    if outcome.is_err() {
        // The terminal is unusable (I/O error or panic); stop the run so
        // `main` can report a coherent state instead of leaving invisible
        // updates in flight.
        handle.cancel.store(true, Ordering::Relaxed);
    }
    outcome
}

fn event_loop(dash: &mut Dashboard<'_>, term: &mut Term) -> io::Result<()> {
    let mut force_draw = true;
    loop {
        dash.refresh_logs();
        dash.snapshot();
        if force_draw || dash.drawn_sig != dash.sig {
            dash.draw(term)?;
            dash.drawn_sig = dash.sig;
            force_draw = false;
        }
        let wait = match dash.completion_remaining(Instant::now()) {
            Some(remaining) if remaining.is_zero() => return Ok(()),
            Some(remaining) => remaining.min(IDLE_POLL),
            None => TICK,
        };
        if event::poll(wait)? {
            match event::read()? {
                Event::Resize(_, _) => force_draw = true,
                Event::Key(key) if key.kind == KeyEventKind::Press && dash.on_key(key) => {
                    return Ok(());
                }
                _ => {}
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Dashboard state
// ---------------------------------------------------------------------------

/// One row of the catalogue, plus reusable render scratch buffers so steady
/// state redraws do not allocate.
struct Row {
    id: String,
    label: String,
    before: String,
    after: String,
    message: String,
    version: String,
    version_dim: bool,
    activity: String,
    status: Status,
    exit_code: Option<i32>,
    started: Option<Instant>,
    elapsed: Duration,
    log: PathBuf,
    // Padded, clipped cells rebuilt every frame into reused strings.
    c_id: String,
    c_version: String,
    c_status: String,
    c_time: String,
    c_activity: String,
    version_dirty: bool,
    message_dirty: bool,
}

impl Row {
    fn new(job: &Job) -> Self {
        let mut row = Row {
            id: job.spec.id.clone(),
            label: job.spec.label.clone(),
            before: String::new(),
            after: String::new(),
            message: String::new(),
            version: String::new(),
            version_dim: true,
            activity: String::new(),
            status: job.status,
            exit_code: job.exit_code,
            started: job.started,
            elapsed: job.elapsed,
            log: job.log.clone(),
            c_id: String::new(),
            c_version: String::new(),
            c_status: String::new(),
            c_time: String::new(),
            c_activity: String::new(),
            version_dirty: true,
            message_dirty: true,
        };
        copy_capped(&mut row.before, &job.before, TEXT_CAP);
        copy_capped(&mut row.after, &job.after, TEXT_CAP);
        copy_capped(&mut row.message, &job.message, TEXT_CAP);
        row
    }

    /// Copies the mutable parts of a job into this row. Cheap in steady state:
    /// strings only re-copy when the engine actually changed them.
    fn update(&mut self, job: &Job) {
        self.status = job.status;
        self.exit_code = job.exit_code;
        self.started = job.started;
        self.elapsed = job.elapsed;
        if self.before != job.before {
            copy_capped(&mut self.before, &job.before, TEXT_CAP);
            self.version_dirty = true;
        }
        if self.after != job.after {
            copy_capped(&mut self.after, &job.after, TEXT_CAP);
            self.version_dirty = true;
        }
        if self.message != job.message {
            copy_capped(&mut self.message, &job.message, TEXT_CAP);
            self.message_dirty = true;
        }
        if self.log != job.log {
            self.log.clone_from(&job.log);
        }
        if self.version_dirty {
            self.version_dirty = false;
            self.refresh_version();
        }
        if self.message_dirty {
            self.message_dirty = false;
            self.refresh_activity();
        }
        if self.status == Status::Running && self.activity.is_empty() {
            self.activity.push('-');
        }
    }

    /// `before -> after` when the update changed something, the known version
    /// otherwise, dimmed while the fresh version is still unknown.
    fn refresh_version(&mut self) {
        self.version.clear();
        if !self.after.is_empty() {
            if !self.before.is_empty() && self.before != self.after {
                let _ = write!(self.version, "{} → {}", self.before, self.after);
            } else {
                self.version.push_str(&self.after);
            }
            self.version_dim = false;
        } else if !self.before.is_empty() {
            self.version.push_str(&self.before);
            self.version_dim = matches!(self.status, Status::Running | Status::Queued);
        } else {
            self.version.push('-');
            self.version_dim = true;
        }
    }

    /// Static activity for non-running rows (block reasons, errors, results).
    /// Running rows keep the log-tail activity from `set_tail_activity`.
    fn refresh_activity(&mut self) {
        if self.status == Status::Running {
            return;
        }
        self.activity.clear();
        if self.message.is_empty() {
            self.activity.push('-');
        } else {
            push_clipped(&mut self.activity, self.message.trim(), ACTIVITY_MAX);
        }
    }

    /// Live activity: the last non-empty line of the job's log tail, skipping
    /// the engine's internal `# update-agents job` header so internal command
    /// paths never reach the overview. The detail log keeps the full file,
    /// header included. Header-only tails (quiet agents) fall back to '-' so
    /// a finished quiet row never shows the stale header either.
    fn set_tail_activity(&mut self, tail: &str) {
        self.activity.clear();
        let last = tail
            .lines()
            .rev()
            .map(str::trim)
            .find(|line| !line.is_empty() && !line.starts_with(LOG_HEADER_PREFIX));
        match last {
            Some(line) => push_clipped(&mut self.activity, line, ACTIVITY_MAX),
            None => self.activity.push('-'),
        }
    }

    /// Elapsed seconds shown in the time column: live for running rows,
    /// engine-recorded for finished ones.
    fn elapsed_secs(&self) -> u64 {
        match self.status {
            Status::Running => self.started.map_or(0, |s| s.elapsed().as_secs()),
            Status::Queued => 0,
            _ => self.elapsed.as_secs(),
        }
    }

    fn build_cells(&mut self, col: Columns) {
        push_cell_padded(&mut self.c_id, &self.id, col.id as usize);
        if col.version > 0 {
            push_cell_padded(&mut self.c_version, &self.version, col.version as usize);
        } else {
            self.c_version.clear();
        }
        push_cell_padded(&mut self.c_status, status_text(self.status), STATUS_W);
        if col.time > 0 {
            self.c_time.clear();
            if self.started.is_none() {
                self.c_time.push('-');
            } else {
                let secs = self.elapsed_secs();
                push_duration(&mut self.c_time, secs);
            }
            pad_left_inplace(&mut self.c_time, col.time as usize);
        } else {
            self.c_time.clear();
        }
        if col.activity > 0 {
            push_clipped(&mut self.c_activity, &self.activity, col.activity as usize);
        } else {
            self.c_activity.clear();
        }
    }
}

/// Aggregated counts for the header line and the completed-tools gauge.
#[derive(Clone, Copy, Default)]
struct Counts {
    total: usize,
    done: usize,
    ok: usize,
    failed: usize,
    blocked: usize,
    skipped: usize,
    running: usize,
    queued: usize,
    cancelled: usize,
    timed_out: usize,
}

impl Counts {
    fn count(&mut self, status: Status) {
        self.total += 1;
        match status {
            Status::Queued => self.queued += 1,
            Status::Running => self.running += 1,
            Status::Succeeded => {
                self.ok += 1;
                self.done += 1;
            }
            Status::Failed => {
                self.failed += 1;
                self.done += 1;
            }
            Status::Blocked => {
                self.blocked += 1;
                self.done += 1;
            }
            Status::Skipped => {
                self.skipped += 1;
                self.done += 1;
            }
            Status::Cancelled => {
                self.cancelled += 1;
                self.done += 1;
            }
            Status::TimedOut => {
                self.timed_out += 1;
                self.done += 1;
            }
        }
    }
}

/// Mutable dashboard state; the engine handle is only ever read, except for
/// the cancel flag.
struct Dashboard<'a> {
    handle: &'a RunHandle,
    /// True when the user named tool IDs explicitly: then not-detected rows
    /// stay visible everywhere instead of being hidden.
    explicit: bool,
    rows: Vec<Row>,
    sel: usize,
    /// Round-robin cursor for the one-per-tick activity tail read.
    rot: usize,
    /// Last known list viewport height (used by PgUp/PgDn list paging).
    list_h: u16,
    /// Last known detail log viewport height (used by PgUp/PgDn scrolling).
    log_h: u16,
    detail_open: bool,
    /// Detail log follows the tail until the user scrolls away.
    follow: bool,
    log_scroll: usize,
    help: bool,
    confirm: bool,
    tail_path: Option<PathBuf>,
    tail_text: String,
    tail_at: Instant,
    tail_pending: bool,
    counts: Counts,
    done: bool,
    exit_at: Option<Instant>,
    cancelled: bool,
    /// Run clock: live while running, latched at the first done snapshot so
    /// the finished screen shows the actual run length.
    run_elapsed: u64,
    /// Fingerprint of everything rendered; equal fingerprints skip the draw.
    sig: u64,
    drawn_sig: u64,
}

impl<'a> Dashboard<'a> {
    fn new(handle: &'a RunHandle, explicit: bool) -> Self {
        Dashboard {
            handle,
            explicit,
            rows: Vec::new(),
            sel: 0,
            rot: 0,
            list_h: 10,
            log_h: 10,
            detail_open: false,
            follow: true,
            log_scroll: 0,
            help: false,
            confirm: false,
            tail_path: None,
            tail_text: String::new(),
            tail_at: Instant::now(),
            tail_pending: false,
            counts: Counts::default(),
            done: false,
            exit_at: None,
            cancelled: false,
            run_elapsed: 0,
            sig: 0,
            drawn_sig: 1,
        }
    }

    /// Briefly locks the run state and copies the small per-row fields into
    /// the reusable row buffers. No disk I/O happens under the lock.
    fn snapshot(&mut self) {
        let handle = self.handle;
        let (finished, started) = {
            let state = lock_state(&handle.state);
            if self.rows.len() != state.jobs.len() {
                self.rows.clear();
                self.rows.extend(state.jobs.iter().map(Row::new));
                self.sel = (0..self.rows.len())
                    .find(|&i| self.is_visible(i))
                    .unwrap_or(0);
            }
            for (row, job) in self.rows.iter_mut().zip(&state.jobs) {
                row.update(job);
            }
            let mut counts = Counts::default();
            for row in &self.rows {
                counts.count(row.status);
            }
            self.counts = counts;
            (state.done, state.started)
        };
        if finished && !self.done {
            self.exit_at = Some(Instant::now() + COMPLETION_WAIT);
            self.help = false;
            self.confirm = false;
            // One final tail refresh so the detail view shows the last output.
            self.tail_pending = true;
            // Keep update duration separate from the completion countdown.
            self.run_elapsed = started.elapsed().as_secs();
        }
        self.done = finished;
        if !finished {
            self.run_elapsed = started.elapsed().as_secs();
        }
        self.cancelled = handle.cancel.load(Ordering::Relaxed);
        self.compute_sig();
    }

    fn completion_remaining(&self, now: Instant) -> Option<Duration> {
        self.exit_at
            .map(|deadline| deadline.saturating_duration_since(now))
    }

    fn completion_seconds(&self) -> u64 {
        self.completion_remaining(Instant::now())
            .map_or(0, |remaining| {
                remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0)
            })
    }

    /// FNV-1a fingerprint of every rendered byte; a stable fingerprint skips
    /// the actual draw. Bounded and allocation-free.
    fn compute_sig(&mut self) {
        let mut h = 0xcbf2_9ce4_8422_2325_u64;
        mix_u64(&mut h, self.counts.total as u64);
        mix_u64(&mut h, self.counts.done as u64);
        mix_u64(&mut h, self.counts.ok as u64);
        mix_u64(&mut h, self.counts.failed as u64);
        mix_u64(&mut h, self.counts.blocked as u64);
        mix_u64(&mut h, self.counts.skipped as u64);
        mix_u64(&mut h, self.counts.running as u64);
        mix_u64(&mut h, self.counts.queued as u64);
        mix_u64(&mut h, self.counts.cancelled as u64);
        mix_u64(&mut h, self.counts.timed_out as u64);
        mix_u64(&mut h, self.run_elapsed);
        mix_bool(&mut h, self.done);
        mix_u64(&mut h, self.completion_seconds());
        mix_bool(&mut h, self.cancelled);
        mix_bool(&mut h, self.help);
        mix_bool(&mut h, self.confirm);
        mix_bool(&mut h, self.detail_open);
        mix_bool(&mut h, self.follow);
        mix_u64(&mut h, self.sel as u64);
        mix_u64(&mut h, self.log_scroll as u64);
        mix_str(&mut h, &self.tail_text);
        for row in &self.rows {
            mix_u64(&mut h, u64::from(status_code(row.status)));
            mix_u64(&mut h, row.elapsed_secs());
            mix_bool(&mut h, row.exit_code.is_some());
            if let Some(code) = row.exit_code {
                mix_u64(&mut h, code as u64);
            }
            mix_str(&mut h, &row.version);
            mix_str(&mut h, &row.activity);
        }
        self.sig = h;
    }

    /// Bounded background I/O: the open detail log (at most 4 Hz, 8 KiB) and
    /// exactly one rotating running-row activity read per tick (192 B).
    fn refresh_logs(&mut self) {
        if self.detail_open {
            let due = self.tail_pending || (!self.done && self.tail_at.elapsed() >= DETAIL_REFRESH);
            if due {
                if let Some(path) = self.tail_path.clone() {
                    let text = crate::engine::tail(&path, DETAIL_BYTES);
                    if text != self.tail_text {
                        self.tail_text = text;
                    }
                    self.tail_at = Instant::now();
                    self.tail_pending = false;
                }
            }
        }
        if !self.done && !self.rows.is_empty() {
            let n = self.rows.len();
            for _ in 0..n {
                self.rot = (self.rot + 1) % n;
                if self.rows[self.rot].status == Status::Running {
                    let path = self.rows[self.rot].log.clone();
                    let text = crate::engine::tail(&path, ACTIVITY_BYTES);
                    self.rows[self.rot].set_tail_activity(&text);
                    break;
                }
            }
        }
    }

    /// Handles one key press; returns true when the dashboard should exit.
    fn on_key(&mut self, key: KeyEvent) -> bool {
        if self.done {
            return true;
        }
        // While running, Ctrl-C cancels immediately from any dialog or view.
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            self.cancel();
            return false;
        }
        if self.help {
            self.help = false;
            return false;
        }
        if self.confirm {
            match key.code {
                KeyCode::Char('y' | 'Y') => self.cancel(),
                KeyCode::Char('n' | 'N') | KeyCode::Esc | KeyCode::Char('q') => {
                    self.confirm = false;
                }
                _ => {}
            }
            return false;
        }
        match key.code {
            KeyCode::Char('j') | KeyCode::Down => self.move_selection(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_selection(-1),
            KeyCode::PageDown => self.page(1),
            KeyCode::PageUp => self.page(-1),
            KeyCode::Enter => self.toggle_detail(),
            KeyCode::Char('l') => self.toggle_detail(),
            KeyCode::Char('?') => self.help = !self.help,
            KeyCode::Char('q') | KeyCode::Esc if !self.cancelled => {
                // Never cancel silently: ask first, unless cancellation is
                // already in flight (then the header says CANCELLING).
                self.confirm = true;
            }
            _ => {}
        }
        false
    }

    fn cancel(&mut self) {
        self.confirm = false;
        self.cancelled = true;
        self.handle.cancel.store(true, Ordering::Relaxed);
    }

    /// False for rows hidden from the list: not-detected tools stay out of
    /// the list and the selection walk; the footer names them instead.
    fn is_visible(&self, i: usize) -> bool {
        !self.rows[i].status.hidden_from_list(self.explicit)
    }

    fn move_selection(&mut self, delta: isize) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        // Walk at most n steps (wrap-around) to the next visible row; when
        // nothing is visible the selection stays where it is.
        let mut target = self.sel;
        for _ in 0..n {
            target = (target as isize + delta).rem_euclid(n as isize) as usize;
            if self.is_visible(target) {
                break;
            }
        }
        self.sel = target;
        self.sync_detail();
    }

    fn page(&mut self, direction: isize) {
        if self.detail_open {
            let step = self.log_h.max(2) as usize - 1;
            if direction > 0 {
                let max = self.max_log_scroll();
                self.log_scroll = (self.log_scroll + step).min(max);
                self.follow = self.log_scroll >= max;
            } else {
                self.log_scroll = self.log_scroll.saturating_sub(step);
                self.follow = false;
            }
            return;
        }
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let step = self.list_h.max(2) as isize - 1;
        let target = if direction > 0 {
            (self.sel as isize + step).min(n as isize - 1)
        } else {
            (self.sel as isize - step).max(0)
        } as usize;
        // A clamped target may land on a hidden row: search for the nearest
        // visible row walking back towards the start of the page (down-page
        // searches backwards, up-page forwards); keep the selection when the
        // whole list is hidden.
        if self.is_visible(target) {
            self.sel = target;
        } else {
            let near = if direction > 0 {
                (0..target).rev().find(|&i| self.is_visible(i))
            } else {
                (target + 1..n).find(|&i| self.is_visible(i))
            };
            if let Some(i) = near {
                self.sel = i;
            }
        }
        self.sync_detail();
    }

    fn max_log_scroll(&self) -> usize {
        self.tail_text
            .lines()
            .count()
            .saturating_sub(self.log_h.max(1) as usize)
    }

    fn toggle_detail(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        self.detail_open = !self.detail_open;
        if self.detail_open {
            self.tail_pending = true;
            self.follow = true;
            self.log_scroll = 0;
            self.sync_detail();
        }
    }

    /// Points the detail tail at the selected row's log when it changed.
    fn sync_detail(&mut self) {
        if !self.detail_open || self.rows.is_empty() {
            return;
        }
        let path = self.rows[self.sel].log.clone();
        if self.tail_path.as_ref() != Some(&path) {
            self.tail_path = Some(path);
            self.tail_text.clear();
            self.tail_pending = true;
            self.log_scroll = 0;
            self.follow = true;
        }
    }

    fn draw(&mut self, term: &mut Term) -> io::Result<()> {
        term.draw(|frame| self.render(frame))?;
        Ok(())
    }

    // -- rendering ----------------------------------------------------------

    fn render(&mut self, frame: &mut Frame) {
        let full = frame.area();
        if full.width == 0 || full.height == 0 {
            return;
        }
        let chrome = chrome_layout(full);
        let columns = Columns::compute(full.width, &self.rows);
        let (list_area, detail_area) = self.split_detail(chrome.rows);
        self.list_h = list_area.height;
        if let Some(area) = chrome.header {
            self.render_header(frame, area);
        }
        if let Some(area) = chrome.gauge {
            self.render_gauge(frame, area);
        }
        if let Some(area) = chrome.colhead {
            self.render_colhead(frame, area, columns);
        }
        self.render_rows(frame, list_area, columns);
        if let Some(area) = chrome.footer {
            self.render_footer(frame, area);
        }
        if let Some(area) = detail_area {
            self.render_detail(frame, area);
        }
        if self.help {
            self.render_help(frame, full);
        }
        if self.confirm {
            self.render_confirm(frame, full);
        }
    }

    /// Bottom part of the list area becomes the detail log when open.
    fn split_detail(&self, list: Rect) -> (Rect, Option<Rect>) {
        if !self.detail_open || list.height == 0 {
            return (list, None);
        }
        let detail_h = if list.height <= 5 {
            list.height
        } else {
            (list.height * 3 / 5).clamp(4, list.height - 1)
        };
        let [top, bottom] =
            Layout::vertical([Constraint::Min(0), Constraint::Length(detail_h)]).areas(list);
        (top, Some(bottom))
    }

    fn render_header(&self, frame: &mut Frame, area: Rect) {
        let c = self.counts;
        let mut line = Segments::new(area.width as usize);
        line.push(Span::styled(
            "update-agents".to_string(),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        if self.done {
            line.push(Span::styled(
                "COMPLETED".to_string(),
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        }
        line.push_plain(format!("{}/{} done", c.done, c.total));
        if self.cancelled && !self.done {
            line.push(Span::styled(
                "CANCELLING".to_string(),
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ));
        }
        if c.running > 0 {
            line.push_colored(format!("run {}", c.running), ACCENT);
        }
        if c.ok > 0 {
            line.push_colored(format!("ok {}", c.ok), Color::Green);
        }
        if c.failed > 0 {
            line.push_colored(format!("fail {}", c.failed), Color::Red);
        }
        if c.timed_out > 0 {
            line.push_colored(format!("timeout {}", c.timed_out), Color::Red);
        }
        if c.blocked > 0 {
            line.push_colored(format!("block {}", c.blocked), Color::Yellow);
        }
        if c.skipped > 0 {
            line.push_colored(format!("not detected {}", c.skipped), Color::DarkGray);
        }
        if c.cancelled > 0 {
            line.push_colored(format!("cancelled {}", c.cancelled), Color::DarkGray);
        }
        if c.queued > 0 {
            line.push_colored(format!("queued {}", c.queued), Color::DarkGray);
        }
        line.push_dim(format!("up {}", format_duration(self.run_elapsed)));
        frame.render_widget(Paragraph::new(Line::from(line.spans)), area);
    }

    /// The single overall gauge: completed tools out of total, which is the
    /// only progress the contract allows (no transfer percentages).
    fn render_gauge(&self, frame: &mut Frame, area: Rect) {
        if self.counts.total == 0 {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("no tools", dim_style()))),
                area,
            );
            return;
        }
        let ratio = self.counts.done as f64 / self.counts.total as f64;
        let gauge = Gauge::default()
            .ratio(ratio)
            .label(format!(
                " {}/{} tools done ",
                self.counts.done, self.counts.total
            ))
            .gauge_style(Style::default().fg(ACCENT).bg(Color::DarkGray))
            .use_unicode(true);
        frame.render_widget(gauge, area);
    }

    fn render_colhead(&self, frame: &mut Frame, area: Rect, col: Columns) {
        let style = dim_style();
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(9);
        spans.push(Span::styled(pad_right("id", col.id as usize), style));
        if col.version > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(
                pad_right("version", col.version as usize),
                style,
            ));
        }
        spans.push(Span::raw(" "));
        spans.push(Span::styled(pad_right("status", STATUS_W), style));
        if col.time > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled(pad_left("time", col.time as usize), style));
        }
        if col.activity > 0 {
            spans.push(Span::raw(" "));
            spans.push(Span::styled("activity".to_string(), style));
        }
        frame.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn render_rows(&mut self, frame: &mut Frame, area: Rect, col: Columns) {
        if self.rows.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("no tools loaded", dim_style()))),
                area,
            );
            return;
        }
        if area.height == 0 {
            return;
        }
        for row in &mut self.rows {
            row.build_cells(col);
        }
        let visible: Vec<usize> = (0..self.rows.len())
            .filter(|&i| self.is_visible(i))
            .collect();
        if visible.is_empty() {
            frame.render_widget(
                Paragraph::new(Line::from(Span::styled("no detected tools", dim_style()))),
                area,
            );
            return;
        }
        let selected = self.sel;
        let items: Vec<ListItem<'_>> = visible
            .iter()
            .map(|&i| ListItem::new(row_line(&self.rows[i], col, i == selected)))
            .collect();
        let highlight = visible.iter().position(|&i| i == selected).unwrap_or(0);
        let mut state = ListState::default();
        state.select(Some(highlight));
        let list = List::new(items).highlight_style(Style::default().bg(Color::DarkGray));
        frame.render_stateful_widget(list, area, &mut state);
    }

    fn render_footer(&self, frame: &mut Frame, area: Rect) {
        if self.done {
            let notice = format!(
                "Auto-exit in {}s | Any key: exit",
                self.completion_seconds()
            );
            frame.render_widget(
                Paragraph::new(notice).style(
                    Style::default()
                        .fg(ACCENT)
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                ),
                area,
            );
            return;
        }
        let mut line = Segments::new(area.width as usize);
        // Hidden rows are silent in the list, so the footer must name them
        // before the key hints: people need to see what was not detected.
        let hidden: Vec<&str> = (0..self.rows.len())
            .filter(|&i| !self.is_visible(i))
            .map(|i| self.rows[i].id.as_str())
            .collect();
        if !hidden.is_empty() {
            line.push_dim(format!("not detected {}:", hidden.len()));
            for id in hidden {
                line.push_dim(id.to_string());
            }
        }
        if self.detail_open {
            line.push_dim("pgup/pgdn scroll".to_string());
            line.push_dim("enter/l close log".to_string());
        } else {
            line.push_dim("j/k select".to_string());
            line.push_dim("enter/l log".to_string());
            line.push_dim("? help".to_string());
        }
        if self.cancelled {
            line.push(Span::styled(
                "cancelling…".to_string(),
                Style::default().fg(Color::Yellow),
            ));
        } else {
            line.push_dim("q quit".to_string());
            line.push_dim("ctrl-c cancel".to_string());
        }
        frame.render_widget(Paragraph::new(Line::from(line.spans)), area);
    }

    fn render_detail(&mut self, frame: &mut Frame, area: Rect) {
        let Some(row) = self.rows.get(self.sel) else {
            return;
        };
        let title = format!(" {} · {} ", row.id, row.label);
        let status = row.status;
        let exit_code = row.exit_code;
        let version = row.version.clone();
        let message = row.message.clone();
        let block = Block::bordered()
            .border_style(dim_style())
            .title(Span::styled(
                title,
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ))
            .padding(Padding::horizontal(1));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }
        let [meta_area, log_area] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);

        let mut scratch = String::new();
        let mut meta: Vec<Span<'static>> = Vec::with_capacity(6);
        meta.push(Span::styled(
            status_text(status).to_string(),
            status_style(status),
        ));
        if let Some(code) = exit_code {
            meta.push(Span::raw(format!(" exit {code}")));
        }
        if !version.is_empty() && version != "-" {
            push_clipped(&mut scratch, &version, 96);
            meta.push(Span::raw(format!("  {scratch}")));
        }
        if !message.is_empty() {
            push_clipped(&mut scratch, &message, 160);
            meta.push(Span::styled(format!("  {scratch}"), dim_style()));
        }
        frame.render_widget(Paragraph::new(Line::from(meta)), meta_area);

        if log_area.height == 0 {
            return;
        }
        self.log_h = log_area.height;
        let max_scroll = self.max_log_scroll();
        if self.follow {
            self.log_scroll = max_scroll;
        } else {
            self.log_scroll = self.log_scroll.min(max_scroll);
        }
        let lines: Vec<Line<'_>> = if self.tail_text.is_empty() {
            vec![Line::from(Span::styled(
                if self.done {
                    "(no output)"
                } else {
                    "(no output yet)"
                },
                dim_style(),
            ))]
        } else {
            self.tail_text.lines().map(Line::raw).collect()
        };
        let scroll_y = u16::try_from(self.log_scroll).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((scroll_y, 0)), log_area);
    }

    fn render_help(&self, frame: &mut Frame, full: Rect) {
        const KEYS: [(&str, &str); 9] = [
            ("j / ↓", "select next row"),
            ("k / ↑", "select previous row"),
            ("enter / l", "toggle log detail"),
            ("pgup / pgdn", "scroll the log (page the list otherwise)"),
            ("?", "toggle this help"),
            ("q / esc", "quit (asks first while running)"),
            ("ctrl-c", "cancel the run now"),
            ("y / n", "confirm or dismiss the cancel question"),
            ("any key", "exit when done (automatic after 5s)"),
        ];
        let area = centered_rect(full, 58.min(full.width), KEYS.len() as u16 + 2);
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .border_style(dim_style())
            .title(Span::styled(" keys ", Style::default().fg(ACCENT)));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let lines: Vec<Line<'static>> = KEYS
            .iter()
            .map(|(key, desc)| {
                Line::from(vec![
                    Span::styled(format!("{key:<14}"), Style::default().fg(ACCENT)),
                    Span::raw((*desc).to_string()),
                ])
            })
            .collect();
        frame.render_widget(Paragraph::new(lines), inner);
    }

    fn render_confirm(&self, frame: &mut Frame, full: Rect) {
        let pending = self.counts.total.saturating_sub(self.counts.done);
        let area = centered_rect(full, 46.min(full.width), 5);
        frame.render_widget(Clear, area);
        let block = Block::bordered()
            .border_style(Style::default().fg(ACCENT))
            .title(Span::styled(
                " cancel update run? ",
                Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            ));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let key = Style::default().fg(ACCENT).add_modifier(Modifier::BOLD);
        let lines = vec![
            Line::from(format!("{pending} tool(s) running or queued")),
            Line::from(vec![
                Span::styled("y", key),
                Span::raw(" cancel now    "),
                Span::styled("n", key),
                Span::raw(" / esc keep going"),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// Computed column widths; 0 hides a column. Adapts from full dashboards down
/// to a handful of characters: id and status survive longest.
#[derive(Clone, Copy)]
struct Columns {
    id: u16,
    version: u16,
    time: u16,
    activity: u16,
}

impl Columns {
    fn compute(width: u16, rows: &[Row]) -> Self {
        let w = width as usize;
        let id_w = rows
            .iter()
            .map(|r| str_width(&r.id))
            .max()
            .unwrap_or(4)
            .clamp(4, 18)
            .min(w.saturating_sub(STATUS_W + 2))
            .max(3);
        let mut used = id_w + 1 + STATUS_W;
        let time_w = if w > used + TIME_W {
            used += TIME_W + 1;
            TIME_W
        } else {
            0
        };
        let version_w = if w >= used + 20 {
            rows.iter()
                .map(|r| str_width(&r.version))
                .max()
                .unwrap_or(8)
                .clamp(8, 22)
                .min(w - used - 12)
        } else {
            0
        };
        used += if version_w > 0 { version_w + 1 } else { 0 };
        let activity_w = w.saturating_sub(used + 1);
        Columns {
            id: id_w as u16,
            version: version_w as u16,
            time: time_w as u16,
            activity: if activity_w >= ACTIVITY_MIN {
                activity_w as u16
            } else {
                0
            },
        }
    }
}

/// Vertical chrome: header line, completed-tools gauge, column header, the
/// row list, and a key-hint footer. Degrades row by row on tiny terminals.
struct Chrome {
    header: Option<Rect>,
    gauge: Option<Rect>,
    colhead: Option<Rect>,
    rows: Rect,
    footer: Option<Rect>,
}

fn chrome_layout(area: Rect) -> Chrome {
    match area.height {
        0 | 1 => Chrome {
            header: None,
            gauge: None,
            colhead: None,
            rows: area,
            footer: None,
        },
        2 => {
            let [header, rows] =
                Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(area);
            Chrome {
                header: Some(header),
                gauge: None,
                colhead: None,
                rows,
                footer: None,
            }
        }
        3 => {
            let [header, rows, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);
            Chrome {
                header: Some(header),
                gauge: None,
                colhead: None,
                rows,
                footer: Some(footer),
            }
        }
        4 => {
            let [header, colhead, rows, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);
            Chrome {
                header: Some(header),
                gauge: None,
                colhead: Some(colhead),
                rows,
                footer: Some(footer),
            }
        }
        _ => {
            let [header, gauge, colhead, rows, footer] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
            ])
            .areas(area);
            Chrome {
                header: Some(header),
                gauge: Some(gauge),
                colhead: Some(colhead),
                rows,
                footer: Some(footer),
            }
        }
    }
}

fn row_line<'a>(row: &'a Row, col: Columns, selected: bool) -> Line<'a> {
    let mut spans: Vec<Span<'a>> = Vec::with_capacity(9);
    let id_style = if selected {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else if row.status == Status::Skipped {
        dim_style()
    } else {
        Style::default()
    };
    spans.push(Span::styled(row.c_id.as_str(), id_style));
    if col.version > 0 {
        spans.push(Span::raw(" "));
        let style = if row.version_dim {
            dim_style()
        } else {
            Style::default()
        };
        spans.push(Span::styled(row.c_version.as_str(), style));
    }
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        row.c_status.as_str(),
        status_style(row.status),
    ));
    if col.time > 0 {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(row.c_time.as_str(), Style::default()));
    }
    if col.activity > 0 {
        spans.push(Span::raw(" "));
        let style = if row.status == Status::Running {
            Style::default()
        } else {
            dim_style()
        };
        spans.push(Span::styled(row.c_activity.as_str(), style));
    }
    Line::from(spans)
}

/// Budget-checked, space-separated span sequence: segments that would no
/// longer fit are dropped, so narrow terminals keep the most important parts.
struct Segments {
    spans: Vec<Span<'static>>,
    used: usize,
    budget: usize,
}

impl Segments {
    fn new(budget: usize) -> Self {
        Segments {
            spans: Vec::new(),
            used: 0,
            budget,
        }
    }

    fn push(&mut self, span: Span<'static>) {
        let width = span.width();
        if !self.spans.is_empty() {
            if self.used + width + 1 > self.budget {
                return;
            }
            self.spans.push(Span::raw(" "));
            self.used += 1;
        }
        self.spans.push(span);
        self.used += width;
    }

    fn push_plain(&mut self, text: String) {
        self.push(Span::raw(text));
    }

    fn push_colored(&mut self, text: String, color: Color) {
        self.push(Span::styled(text, Style::default().fg(color)));
    }

    fn push_dim(&mut self, text: String) {
        self.push(Span::styled(text, dim_style()));
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Locks the run state; a poisoned lock still yields the data so the
/// dashboard stays usable after an engine-side panic.
fn lock_state(state: &Mutex<RunState>) -> MutexGuard<'_, RunState> {
    match state.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Copies at most `cap` characters of `src` into `dst`, reusing its buffer.
fn copy_capped(dst: &mut String, src: &str, cap: usize) {
    dst.clear();
    dst.extend(src.chars().take(cap));
}

fn str_width(s: &str) -> usize {
    Span::raw(s).width()
}

/// Appends `s` to `out`: control characters dropped, clipped to `max` display
/// columns. Text that fits exactly is kept whole; only text truly wider than
/// `max` gets a trailing ellipsis, and the trim is Unicode-safe: a character
/// that does not fit in the columns left beside the ellipsis (a two-cell
/// Hangul syllable, say) is dropped whole. Reuses `out`'s allocation.
fn push_clipped(out: &mut String, s: &str, max: usize) {
    out.clear();
    if max == 0 {
        return;
    }
    if push_fit(out, s, max) {
        return;
    }
    // The text is truly wider than `max`: keep the longest prefix that still
    // leaves one column for the ellipsis, dropping whole any character wider
    // than the space left beside it.
    out.clear();
    push_fit(out, s, max - 1);
    out.push('…');
}

/// Appends the visible characters of `s` to `out` until `max` display columns
/// are used up; control characters are dropped without consuming width.
/// Returns true when every visible character fit.
fn push_fit(out: &mut String, s: &str, max: usize) -> bool {
    let mut used = 0usize;
    let mut buf = [0u8; 4];
    for c in s.chars() {
        if c.is_control() {
            continue;
        }
        let w = str_width(c.encode_utf8(&mut buf));
        if used + w > max {
            return false;
        }
        used += w;
        out.push(c);
    }
    true
}

fn push_cell_padded(out: &mut String, s: &str, width: usize) {
    push_clipped(out, s, width);
    let pad = width.saturating_sub(str_width(out));
    for _ in 0..pad {
        out.push(' ');
    }
}

/// Right-aligns the already-clipped content of `out` inside `width` columns.
fn pad_left_inplace(out: &mut String, width: usize) {
    let pad = width.saturating_sub(str_width(out));
    for _ in 0..pad {
        out.insert(0, ' ');
    }
}

fn pad_right(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len() + width);
    push_clipped(&mut out, text, width);
    let pad = width.saturating_sub(str_width(&out));
    for _ in 0..pad {
        out.push(' ');
    }
    out
}

fn pad_left(text: &str, width: usize) -> String {
    let mut body = String::new();
    push_clipped(&mut body, text, width);
    let pad = width.saturating_sub(str_width(&body));
    let mut out = String::with_capacity(body.len() + pad);
    for _ in 0..pad {
        out.push(' ');
    }
    out.push_str(&body);
    out
}

/// Compact duration for the time column (`42s`, `3m05s`, `1h02m`).
fn push_duration(out: &mut String, secs: u64) {
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    let _ = if h > 0 {
        write!(out, "{h}h{m:02}m")
    } else if m > 0 {
        write!(out, "{m}m{s:02}s")
    } else {
        write!(out, "{s}s")
    };
}

fn format_duration(secs: u64) -> String {
    let mut out = String::new();
    push_duration(&mut out, secs);
    out
}

fn status_text(status: Status) -> &'static str {
    match status {
        Status::Queued => "queued",
        Status::Running => "running",
        Status::Succeeded => "ok",
        Status::Failed => "failed",
        Status::Blocked => "blocked",
        Status::Skipped => "not detected",
        Status::Cancelled => "cancelled",
        Status::TimedOut => "timeout",
    }
}

fn status_code(status: Status) -> u8 {
    match status {
        Status::Queued => 0,
        Status::Running => 1,
        Status::Succeeded => 2,
        Status::Failed => 3,
        Status::Blocked => 4,
        Status::Skipped => 7,
        Status::Cancelled => 5,
        Status::TimedOut => 6,
    }
}

fn status_style(status: Status) -> Style {
    let color = match status {
        Status::Queued | Status::Cancelled => Color::DarkGray,
        Status::Running => ACCENT,
        Status::Succeeded => Color::Green,
        Status::Failed | Status::TimedOut => Color::Red,
        Status::Blocked => Color::Yellow,
        // Resolved without running anything: dim, clearly not a failure.
        Status::Skipped => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn dim_style() -> Style {
    Style::default().fg(Color::DarkGray)
}

fn centered_rect(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

fn mix_u64(h: &mut u64, v: u64) {
    for shift in (0..64).step_by(8) {
        *h ^= u64::from((v >> shift) as u8);
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
}

fn mix_bool(h: &mut u64, v: bool) {
    mix_u64(h, u64::from(v));
}

fn mix_str(h: &mut u64, s: &str) {
    for byte in s.as_bytes() {
        *h ^= u64::from(*byte);
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
    mix_u64(h, s.len() as u64);
}

// ---------------------------------------------------------------------------
// Terminal lifecycle
// ---------------------------------------------------------------------------

/// Restores the terminal when dropped: covers errors, early returns, panics.
struct TermGuard;

impl Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

/// Best-effort restore; safe to call more than once.
fn restore_terminal() {
    let _ = execute!(io::stdout(), crossterm::cursor::Show, LeaveAlternateScreen);
    let _ = disable_raw_mode();
    let _ = io::stdout().flush();
}

fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CommandSpec, Preflight, ToolSpec};
    fn job_with(status: Status, message: &str) -> Job {
        Job {
            spec: ToolSpec {
                id: "cursor-agent".to_string(),
                label: "Cursor Agent".to_string(),
                update: CommandSpec {
                    program: "cursor-agent".to_string(),
                    args: vec!["update".to_string()],
                },
                version: None,
                resource: "cursor-agent".to_string(),
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
            log: PathBuf::from("/tmp/update-agents-test.log"),
            exit_code: None,
        }
    }

    fn row_with(status: Status, message: &str) -> Row {
        Row::new(&job_with(status, message))
    }

    #[test]
    fn exact_fit_text_is_kept_whole() {
        let mut out = String::new();
        push_clipped(&mut out, "cursor-agent", 12);
        assert_eq!(out, "cursor-agent");
        // STATUS_W is exactly the width of "not detected", the longest
        // status word, so a not-detected row must render its full status,
        // not "not detecte…".
        push_clipped(&mut out, "not detected", STATUS_W);
        assert_eq!(out, "not detected");
        assert_eq!(str_width(&out), STATUS_W);
        // The id cell keeps an exact-fit id unpadded and untruncated.
        push_cell_padded(&mut out, "cursor-agent", 12);
        assert_eq!(out, "cursor-agent");
    }

    #[test]
    fn wider_text_fills_the_whole_column_with_an_ellipsis() {
        let mut out = String::new();
        push_clipped(&mut out, "cursor-agent", 8);
        assert_eq!(out, "cursor-…");
        assert_eq!(str_width(&out), 8);
        // One column wide: only the ellipsis itself fits.
        push_clipped(&mut out, "ab", 1);
        assert_eq!(out, "…");
    }

    #[test]
    fn wide_unicode_characters_never_split_across_the_boundary() {
        let mut out = String::new();
        // Two-cell Hangul syllables: an exact fit stays whole ...
        push_clipped(&mut out, "한글", 4);
        assert_eq!(out, "한글");
        // ... and clipping drops the two-cell character whole instead of
        // overflowing the column or leaving no room for the ellipsis.
        push_clipped(&mut out, "한글", 3);
        assert_eq!(out, "한…");
        assert_eq!(str_width(&out), 3);
    }

    #[test]
    fn control_characters_are_dropped_without_consuming_width() {
        let mut out = String::new();
        push_clipped(&mut out, "ok\u{7}!", 4);
        assert_eq!(out, "ok!");
        // A trailing control character does not count as clipping.
        push_clipped(&mut out, "ok\n", 4);
        assert_eq!(out, "ok");
    }

    #[test]
    fn tail_activity_hides_the_internal_job_header() {
        let header = "# update-agents job cursor-agent (program /opt/cursor-agent)\n";
        // Header-only tail: the overview shows the placeholder, never the
        // internal command path.
        let mut row = row_with(Status::Running, "");
        row.set_tail_activity(header);
        assert_eq!(row.activity, "-");
        // Real output after the header wins over the placeholder.
        row.set_tail_activity(&format!("{header}resolving versions\n"));
        assert_eq!(row.activity, "resolving versions");
        // A quiet agent that finishes successfully keeps a clean activity
        // cell instead of the stale header line.
        let mut done = row_with(Status::Succeeded, "");
        done.set_tail_activity(header);
        assert_eq!(done.activity, "-");
    }

    #[test]
    fn skipped_rows_count_as_done_and_show_their_reason() {
        let mut counts = Counts::default();
        counts.count(Status::Skipped);
        counts.count(Status::Succeeded);
        assert_eq!(counts.skipped, 1);
        assert_eq!(counts.done, 2);
        // The first snapshot pass surfaces the skip reason in the activity
        // cell, exactly like any other terminal message.
        let job = job_with(Status::Skipped, "missing executable");
        let mut row = Row::new(&job);
        row.update(&job);
        assert_eq!(row.activity, "missing executable");
    }

    #[test]
    fn completed_run_dismissal_precedes_dialogs_and_cancellation() {
        let dir = std::env::temp_dir().join(format!(
            "update-agents-completion-test-{}",
            std::process::id()
        ));
        let mut spec = job_with(Status::Skipped, "not installed").spec;
        spec.preflight = Preflight::Skipped("not installed".to_string());
        let mut handle = crate::engine::start(
            vec![spec],
            crate::model::RunOptions {
                jobs: 1,
                timeout: Duration::from_secs(1),
                run_dir: dir.clone(),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let mut dash = Dashboard::new(&handle, false);
        dash.help = true;
        dash.confirm = true;
        dash.snapshot();
        let dismissed = dash.on_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        let interrupted = dash.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        let cancelled = handle.cancel.load(Ordering::Relaxed);
        std::fs::remove_dir_all(dir).unwrap();
        assert!(
            dismissed,
            "any key must dismiss a completed run, even with a dialog open"
        );
        assert!(interrupted, "Ctrl-C must also dismiss a completed run");
        assert!(
            !cancelled,
            "dismissing a finished run must not request cancellation"
        );
    }

    #[test]
    fn selection_skips_not_detected_rows() {
        let dir = std::env::temp_dir().join(format!(
            "update-agents-selection-test-{}",
            std::process::id()
        ));
        let mut ghost = job_with(Status::Queued, "").spec;
        ghost.id = "ghost".to_string();
        ghost.preflight = Preflight::Skipped("nope".to_string());
        let live = |id: &str| {
            let mut s = job_with(Status::Queued, "").spec;
            s.id = id.to_string();
            s.update = CommandSpec {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "true".to_string()],
            };
            s.version = Some(CommandSpec {
                program: "sh".to_string(),
                args: vec!["-c".to_string(), "echo v".to_string()],
            });
            s
        };
        let mut handle = crate::engine::start(
            vec![live("live1"), ghost, live("live2")],
            crate::model::RunOptions {
                jobs: 2,
                timeout: Duration::from_secs(5),
                run_dir: dir.clone(),
            },
        )
        .unwrap();
        handle.wait().unwrap();
        let mut dash = Dashboard::new(&handle, false);
        dash.snapshot();
        assert!(dash.is_visible(0), "ready rows stay visible");
        assert_eq!(dash.sel, 0, "selection starts on the first visible row");
        assert!(!dash.is_visible(1), "the not-detected row must be hidden");
        dash.move_selection(1);
        assert_eq!(dash.sel, 2, "moving down skips the hidden row");
        dash.move_selection(-1);
        assert_eq!(dash.sel, 0, "moving up skips the hidden row again");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
