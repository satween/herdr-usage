//! herdr plugin entrypoint: universal agent usage (tokens, cost, models).
//!
//! Commands: once (print), watch (interactive pane), open (open the pane).

mod agents;
mod prices;
mod keys;
mod render;

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicI32, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use agents::{collect_all, is_collector, parse_iso, AgentReport, COLLECTORS};
use keys::{scan_keys, scroll_to, Action};
use render::{footnote, key_hints, render, Style};

const DEFAULT_REFRESH_SECONDS: u64 = 300;
const MIN_REFRESH_SECONDS: f64 = 30.0;
const CACHE_REUSE_SECONDS: f64 = 60.0;
const PANE_ENTRYPOINT: &str = "usage";

// ---------------------------------------------------------------------------
// config and cache
// ---------------------------------------------------------------------------

struct Config {
    refresh_seconds: u64,
    agents: Vec<String>,
    /// Fetch prices from the models.dev catalog (`"prices": "builtin"` turns
    /// it off and uses only the table compiled in).
    online_prices: bool,
}

/// `$HERDR_PLUGIN_CONFIG_DIR/config.json`: `refresh_seconds` (>= 30) and
/// `agents` (a subset of the known collectors, in display order), `prices`
/// (`"models.dev"`, the default, or `"builtin"` for no network fetch).
fn load_config() -> Config {
    let mut config = Config {
        refresh_seconds: DEFAULT_REFRESH_SECONDS,
        agents: COLLECTORS.iter().map(|s| s.to_string()).collect(),
        online_prices: true,
    };
    let Some(dir) = std::env::var_os("HERDR_PLUGIN_CONFIG_DIR").filter(|d| !d.is_empty()) else {
        return config;
    };
    let path = PathBuf::from(dir).join("config.json");
    if !path.is_file() {
        return config;
    }
    let user: Value = match fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|raw| {
        serde_json::from_str(&raw).map_err(|e| e.to_string())
    }) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("warning: ignoring {}: {e}", path.display());
            return config;
        }
    };
    if let Some(refresh) = user.get("refresh_seconds").and_then(Value::as_f64) {
        if refresh >= MIN_REFRESH_SECONDS {
            config.refresh_seconds = refresh as u64;
        }
    }
    if let Some(source) = user.get("prices").and_then(Value::as_str) {
        config.online_prices = source != "builtin";
    }
    if let Some(Value::Array(list)) = user.get("agents") {
        let known: Vec<String> =
            list.iter().filter_map(Value::as_str).filter(|a| is_collector(a)).map(str::to_string).collect();
        if !known.is_empty() {
            config.agents = known;
        }
    }
    config
}

#[derive(Serialize, Deserialize)]
struct CacheFile {
    fetched_at: Option<String>,
    #[serde(default)]
    reports: Vec<AgentReport>,
}

/// Last successful reports, so a reopened pane shows numbers at once.
struct Cache {
    path: Option<PathBuf>,
    reports: Vec<AgentReport>,
    fetched_at: Option<DateTime<Utc>>,
}

impl Cache {
    fn load() -> Self {
        let path = std::env::var_os("HERDR_PLUGIN_STATE_DIR")
            .filter(|d| !d.is_empty())
            .map(|d| PathBuf::from(d).join("cache.json"));
        let mut cache = Cache { path, reports: Vec::new(), fetched_at: None };
        if let Some(raw) = cache.path.as_ref().and_then(|p| fs::read_to_string(p).ok()) {
            if let Ok(file) = serde_json::from_str::<CacheFile>(&raw) {
                cache.fetched_at = file.fetched_at.as_deref().and_then(parse_iso);
                cache.reports = file.reports.into_iter().filter(|r| !r.agent_id.is_empty()).collect();
            }
        }
        cache
    }

    fn fresh(&self, max_age: f64) -> Option<Vec<AgentReport>> {
        let fetched = self.fetched_at?;
        let age = (Utc::now() - fetched).num_milliseconds() as f64 / 1000.0;
        if !(0.0..max_age).contains(&age) {
            return None;
        }
        let order = |id: &str| COLLECTORS.iter().position(|a| *a == id).unwrap_or(99);
        let mut reports = self.reports.clone();
        reports.sort_by_key(|r| order(&r.agent_id));
        Some(reports)
    }

    fn put(&mut self, reports: &[AgentReport]) {
        self.reports = reports.to_vec();
        let now = Utc::now();
        self.fetched_at = Some(now);
        let Some(path) = &self.path else { return };
        let file = CacheFile { fetched_at: Some(now.to_rfc3339()), reports: reports.to_vec() };
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&file) {
            let _ = fs::write(path, json);
        }
    }
}

/// Starts the daily price catalog download when it is due (see `prices`).
fn refresh_prices(config: &Config) -> Option<JoinHandle<()>> {
    if config.online_prices {
        prices::refresh()
    } else {
        None
    }
}

fn collect(config: &Config, cache: &mut Cache, force: bool) -> Vec<AgentReport> {
    if !force {
        if let Some(fresh) = cache.fresh(CACHE_REUSE_SECONDS) {
            return fresh;
        }
    }
    let reports = collect_all(&config.agents);
    cache.put(&reports);
    reports
}

// ---------------------------------------------------------------------------
// terminal
// ---------------------------------------------------------------------------

fn env_size(var: &str) -> usize {
    std::env::var(var).ok().and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// (columns, lines), like Python's shutil.get_terminal_size((100, 24)).
fn terminal_size() -> (usize, usize) {
    let (mut cols, mut rows) = (env_size("COLUMNS"), env_size("LINES"));
    if cols == 0 || rows == 0 {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
        let (c, r) = if ok { (ws.ws_col as usize, ws.ws_row as usize) } else { (100, 24) };
        if cols == 0 {
            cols = if c > 0 { c } else { 100 };
        }
        if rows == 0 {
            rows = if r > 0 { r } else { 24 };
        }
    }
    (cols, rows)
}

fn isatty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

fn no_color() -> bool {
    std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
}

static SIGNALLED: AtomicI32 = AtomicI32::new(0);

extern "C" fn on_signal(signum: libc::c_int) {
    SIGNALLED.store(signum, Ordering::SeqCst);
}

/// Without SA_RESTART, so a signal wakes the poll and the loop can restore the
/// terminal before exiting.
fn install_signals() {
    for sig in [libc::SIGTERM, libc::SIGHUP, libc::SIGINT] {
        unsafe {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
    }
}

/// cbreak mode (no echo, no line buffering) for as long as it lives; also the
/// alternate screen, a hidden cursor and SGR mouse reporting for the wheel.
struct Screen {
    saved: Option<libc::termios>,
}

impl Screen {
    fn enter() -> Self {
        let saved = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                None
            } else {
                let mut raw = t;
                raw.c_lflag &= !(libc::ECHO | libc::ICANON);
                raw.c_cc[libc::VMIN] = 1;
                raw.c_cc[libc::VTIME] = 0;
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSAFLUSH, &raw);
                Some(t)
            }
        };
        // While the pane has mouse reporting, text selection needs Shift held,
        // as in any mouse-aware terminal program.
        out("\x1b[?1049h\x1b[?25l\x1b[?1000h\x1b[?1006h");
        Screen { saved }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        out("\x1b[?1000l\x1b[?1006l\x1b[?25h\x1b[?1049l");
        if let Some(t) = &self.saved {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSADRAIN, t);
            }
        }
    }
}

fn out(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(text.as_bytes());
    let _ = stdout.flush();
}

/// Waits up to `timeout_ms` for input; the bytes of one read, or None.
fn read_input(timeout_ms: i32) -> Option<Vec<u8>> {
    let mut fds = libc::pollfd { fd: libc::STDIN_FILENO, events: libc::POLLIN, revents: 0 };
    let ready = unsafe { libc::poll(&mut fds, 1, timeout_ms) };
    if ready <= 0 || fds.revents & libc::POLLIN == 0 {
        if ready > 0 && fds.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 {
            // stdin is gone: nobody can quit with a key any more
            SIGNALLED.store(libc::SIGHUP, Ordering::SeqCst);
        }
        return None;
    }
    let mut buf = [0u8; 1024];
    let n = unsafe { libc::read(libc::STDIN_FILENO, buf.as_mut_ptr().cast(), buf.len()) };
    if n <= 0 {
        if n == 0 {
            SIGNALLED.store(libc::SIGHUP, Ordering::SeqCst);
        }
        return None;
    }
    Some(buf[..n as usize].to_vec())
}

// ---------------------------------------------------------------------------
// commands
// ---------------------------------------------------------------------------

fn cmd_once(config: &Config) -> u8 {
    // At most once a day: wait for the new prices so this report uses them.
    if let Some(job) = refresh_prices(config) {
        let _ = job.join();
    }
    let reports = collect(config, &mut Cache::load(), false);
    let style = Style::new(isatty(libc::STDOUT_FILENO) && !no_color());
    let (width, _) = terminal_size();
    let mut lines = render(&reports, width, &style, Utc::now());
    let note = footnote(&reports, width, &style);
    if !note.is_empty() {
        lines.push(String::new());
        lines.push(note);
    }
    out(&(lines.join("\n") + "\n"));
    0
}

/// Rows the report gets: the pane less the footer, the footnote and the one
/// row kept free above them.
fn page_size(note: &str) -> usize {
    let (_, rows) = terminal_size();
    rows.saturating_sub(2 + usize::from(!note.is_empty())).max(1)
}

/// The footnote sits on the row right above the key hints, so both stay pinned
/// to the bottom however tall the pane is.
fn draw(lines: &[String], footer: &str, note: &str, offset: usize) {
    let (_, rows) = terminal_size();
    let tail: Vec<&str> = if note.is_empty() { Vec::new() } else { vec![note] };
    let start = offset.min(lines.len());
    let end = (offset + page_size(note)).min(lines.len());
    let body = &lines[start..end];
    let mut s = String::from("\x1b[H");
    for line in body {
        s += line;
        s += "\x1b[K\n";
    }
    s += &"\x1b[K\n".repeat(rows.saturating_sub(2 + tail.len() + body.len()));
    for line in tail {
        s += line;
        s += "\x1b[K\n";
    }
    s += footer;
    s += "\x1b[K\x1b[J";
    out(&s);
}

fn draw_centered(text: &str, style: &Style) {
    let (cols, rows) = terminal_size();
    let text: String = text.chars().take(cols).collect();
    let row = rows.div_ceil(2).max(1);
    let col = (cols.saturating_sub(text.chars().count()) / 2 + 1).max(1);
    out(&format!("\x1b[H\x1b[2J\x1b[{row};{col}H{}", style.dim(&text)));
}

fn cmd_watch(config: &Config) -> u8 {
    if !isatty(libc::STDIN_FILENO) || !isatty(libc::STDOUT_FILENO) {
        return cmd_once(config);
    }
    let style = Style::new(!no_color());
    let mut cache = Cache::load();
    install_signals();
    let screen = Screen::enter();

    let mut reports: Vec<AgentReport> = Vec::new();
    let mut offset: i64 = 0;
    let mut needs_fetch = true;
    let mut force = false;
    let mut last_fetch = Instant::now();
    let mut price_job = refresh_prices(config);
    let code = loop {
        let signal = SIGNALLED.load(Ordering::SeqCst);
        if signal != 0 {
            break if signal == libc::SIGINT { 0 } else { (128 + signal) as u8 };
        }
        let (width, _) = terminal_size();
        if needs_fetch {
            if reports.is_empty() {
                // First load: one line in the middle of the pane rather than a
                // placeholder on top and a status in the footer.
                draw_centered("Collecting usage\u{2026}", &style);
            } else {
                // A refresh keeps the numbers on screen and says so in the footer.
                let lines = render(&reports, width, &style, Utc::now());
                let note = footnote(&reports, width, &style);
                offset = scroll_to(offset, None, page_size(&note) as i64, lines.len() as i64);
                let status: String = "refreshing\u{2026}".chars().take(width).collect();
                draw(&lines, &style.dim(&status), &note, offset as usize);
            }
            if price_job.is_none() {
                price_job = refresh_prices(config);
            }
            reports = collect(config, &mut cache, force);
            last_fetch = Instant::now();
            needs_fetch = false;
            force = false;
            continue; // a signal during the fetch is honoured before drawing
        }
        let elapsed = last_fetch.elapsed().as_secs_f64();
        let remaining = (config.refresh_seconds as f64 - elapsed).max(0.0) as u64;
        let lines = render(&reports, width, &style, Utc::now());
        let note = footnote(&reports, width, &style);
        let page = page_size(&note) as i64;
        offset = scroll_to(offset, None, page, lines.len() as i64);
        let footer = key_hints(remaining, width, &style, lines.len() as i64 > page);
        draw(&lines, &footer, &note, offset as usize);
        if let Some(chunk) = read_input(1000) {
            let actions = scan_keys(&chunk);
            if actions.contains(&Action::Quit) {
                break 0;
            }
            if actions.contains(&Action::Refresh) {
                needs_fetch = true;
                force = true;
            }
            for action in &actions {
                offset = scroll_to(offset, Some(*action), page, lines.len() as i64);
            }
            if actions.iter().any(|a| !matches!(a, Action::Quit | Action::Refresh)) {
                continue; // redraw at once: a scroll must not wait a second
            }
        }
        if last_fetch.elapsed().as_secs_f64() >= config.refresh_seconds as f64 {
            needs_fetch = true;
        }
        if price_job.as_ref().is_some_and(JoinHandle::is_finished) {
            // New prices: recount now rather than at the next refresh.
            price_job = None;
            needs_fetch = true;
            force = true;
        }
    };
    drop(screen);
    code
}

fn cmd_open() -> u8 {
    let Some(plugin_id) = std::env::var("HERDR_PLUGIN_ID").ok().filter(|s| !s.is_empty()) else {
        eprintln!("HERDR_PLUGIN_ID is not set; run this through herdr");
        return 2;
    };
    let herdr = std::env::var("HERDR_BIN_PATH").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "herdr".into());
    // The pane is the point of the action: take the keyboard with it, so the
    // keys the footer offers (r, q) work without another click.
    let status = Command::new(herdr)
        .args(["plugin", "pane", "open", "--plugin", &plugin_id, "--entrypoint", PANE_ENTRYPOINT])
        .args(["--placement", "split", "--direction", "right", "--focus"])
        .status();
    match status {
        Ok(s) => s.code().unwrap_or(1) as u8,
        Err(e) => {
            eprintln!("cannot run herdr: {e}");
            1
        }
    }
}

const USAGE: &str = "usage: herdr-usage [once|watch|open]\n\n\
Show coding-agent usage (tokens, cost, models).\n\n  \
once   print the report and exit (default)\n  \
watch  interactive pane: r refresh, arrows/j/k scroll, q/Esc quit\n  \
open   open the pane through herdr\n";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let code = match args.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        [] | ["once"] => cmd_once(&load_config()),
        ["watch"] => cmd_watch(&load_config()),
        ["open"] => cmd_open(),
        ["-h" | "--help"] => {
            print!("{USAGE}");
            0
        }
        _ => {
            eprint!("{USAGE}");
            2
        }
    };
    ExitCode::from(code)
}
