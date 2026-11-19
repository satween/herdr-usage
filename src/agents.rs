//! Collects usage for Claude Code, Codex, Pi and OpenCode from their local logs.
//!
//! Token counts come from the agents' own session logs, so this works for
//! API-key usage (metered) and subscription usage alike. Cost is estimated with a
//! local price table for API-key/metered usage; subscription usage is flat plan
//! cost, so only tokens are shown for it. When a subscription OAuth token is
//! present the real rate-limit windows are also fetched from the vendor endpoints.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Datelike, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

const SCAN_DAYS: f64 = 35.0; // a margin past the 30d window
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);
pub const WINDOWS: [&str; 3] = ["Today", "7d", "30d"];

/// The agents this plugin knows, in their configured default order.
pub const COLLECTORS: [&str; 4] = ["claude", "codex", "pi", "opencode"];

// ---------------------------------------------------------------------------
// pricing
// ---------------------------------------------------------------------------

/// USD per 1M tokens: (input, output, cache_read, cache_write_5m); None = derive
/// from input (cache read 0.1x, 5-minute cache write 1.25x). A 1-hour cache
/// write is billed at 2x input, which `estimate_cost` applies on its own.
type Price = (f64, f64, Option<f64>, Option<f64>);

const EXACT_PRICING: &[(&str, Price)] = &[
    ("claude-fable-5-2", (8.0, 40.0, Some(0.2), None)),
    ("claude-fable-5-1", (10.0, 50.0, None, None)),
    ("claude-fable-5", (10.0, 50.0, None, None)),
    ("claude-opus-5-5", (4.0, 20.0, Some(0.2), None)),
    ("claude-opus-5", (5.0, 25.0, None, None)),
    ("claude-opus-4-8", (5.0, 25.0, None, None)),
    ("claude-opus-4-7", (5.0, 25.0, None, None)),
    ("claude-opus-4-6", (5.0, 25.0, None, None)),
    ("claude-opus-4-5", (5.0, 25.0, None, None)),
    ("claude-opus-4-1", (15.0, 75.0, None, None)),
    ("claude-opus-4", (15.0, 75.0, None, None)),
    ("claude-sonnet-5-5", (2.0, 10.0, Some(0.1), None)),
    ("claude-sonnet-5", (2.0, 10.0, None, None)),
    ("claude-sonnet-4-6", (3.0, 15.0, None, None)),
    ("claude-sonnet-4-5", (3.0, 15.0, None, None)),
    ("claude-sonnet-4", (3.0, 15.0, None, None)),
    ("claude-haiku-5-5", (0.1, 0.5, None, None)),
    ("claude-haiku-4-5", (1.0, 5.0, None, None)),
    ("gpt-5", (1.25, 10.0, Some(0.125), None)),
    ("gpt-5-codex", (1.25, 10.0, Some(0.125), None)),
    ("gpt-4.1", (2.0, 8.0, Some(0.5), None)),
    ("gpt-4o", (2.5, 10.0, Some(1.25), None)),
    ("o3", (2.0, 8.0, Some(0.5), None)),
    ("o4-mini", (1.1, 4.4, Some(0.275), None)),
];

/// Fallback when the exact model is not in the table (newer model names): the
/// current generation of each family, so an unknown Claude is not billed at the
/// Opus 4 / 4.1 rate of $15/$75 that no current Opus charges.
const FAMILY_PRICING: &[(&str, Price)] = &[
    ("claude-fable", (8.0, 40.0, Some(0.2), None)),
    ("claude-opus", (4.0, 20.0, Some(0.2), None)),
    ("claude-sonnet", (2.0, 10.0, Some(0.1), None)),
    ("claude-haiku", (0.1, 0.5, None, None)),
    ("gpt-5", (1.25, 10.0, Some(0.125), None)),
    ("gpt-4.1", (2.0, 8.0, Some(0.5), None)),
    ("gpt-4o", (2.5, 10.0, Some(1.25), None)),
    ("o3", (2.0, 8.0, Some(0.5), None)),
    ("o4", (1.1, 4.4, Some(0.275), None)),
];

/// Drops a trailing `-YYYYMMDD` date stamp (claude-opus-4-1-20250805).
fn strip_date_suffix(model: &str) -> &str {
    let b = model.as_bytes();
    if b.len() >= 9 {
        let tail = &b[b.len() - 9..];
        if tail[0] == b'-' && tail[1..].iter().all(u8::is_ascii_digit) {
            return &model[..model.len() - 9];
        }
    }
    model
}

fn price_for(model: &str) -> Option<Price> {
    if model.is_empty() {
        return None;
    }
    let lower = model.to_lowercase();
    let last = lower.rsplit('/').next().unwrap_or("");
    let m = strip_date_suffix(last);
    if let Some((_, p)) = EXACT_PRICING.iter().find(|(name, _)| *name == m) {
        return Some(*p);
    }
    FAMILY_PRICING
        .iter()
        .find(|(prefix, _)| m.starts_with(prefix))
        .map(|(_, p)| *p)
}

/// (cost in USD, known?) for one message's tokens. `cache_write_1h` is the part
/// of `cache_write` that went to the 1-hour cache (2x input, not 1.25x).
pub fn estimate_cost(
    model: &str,
    inp: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    cache_write_1h: i64,
) -> (f64, bool) {
    let Some((i, o, cr, cw)) = price_for(model) else {
        return (0.0, false);
    };
    let cr = cr.unwrap_or(i * 0.1);
    let cw = cw.unwrap_or(i * 1.25);
    let write_1h = cache_write_1h.min(cache_write);
    let cost = (inp as f64 * i
        + cache_read as f64 * cr
        + (cache_write - write_1h) as f64 * cw
        + write_1h as f64 * i * 2.0
        + output as f64 * o)
        / 1_000_000.0;
    (cost, true)
}

// ---------------------------------------------------------------------------
// report model
// ---------------------------------------------------------------------------

pub const OTHER_MODELS: &str = "other"; // label for tokens outside the models on display

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelStat {
    pub model: String,
    pub inp: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub output: i64,
    pub cost: f64,
    /// tokens of models with no price table entry
    pub unpriced: i64,
}

impl ModelStat {
    pub fn new(model: &str) -> Self {
        ModelStat { model: model.to_string(), ..Default::default() }
    }

    pub fn total(&self) -> i64 {
        self.inp + self.cache_read + self.cache_write + self.output
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowStat {
    pub label: String,
    pub models: IndexMap<String, ModelStat>,
}

/// One bucket of a model split: (model, tokens, whole percent).
pub type Share = (String, i64, i64);

impl WindowStat {
    pub fn new(label: &str) -> Self {
        WindowStat { label: label.to_string(), models: IndexMap::new() }
    }

    pub fn tokens(&self) -> i64 {
        self.models.values().map(ModelStat::total).sum()
    }

    pub fn cost(&self) -> f64 {
        self.models.values().map(|m| m.cost).sum()
    }

    pub fn unpriced(&self) -> i64 {
        self.models.values().map(|m| m.unpriced).sum()
    }

    /// Top `n` models plus the remainder as one OTHER_MODELS bucket, as (model,
    /// tokens, whole percent) apportioned by largest remainder, so the percents
    /// always add up to 100 of the window. Showing the top few shares each rounded
    /// on its own is what made the old line stop at 98%.
    pub fn model_shares(&self, n: usize) -> Vec<Share> {
        let mut items: Vec<(String, i64)> = self
            .models
            .iter()
            .filter(|(_, stat)| stat.total() > 0)
            .map(|(name, stat)| (name.clone(), stat.total()))
            .collect();
        items.sort_by_key(|kv| std::cmp::Reverse(kv.1)); // stable: ties keep insertion order
        let denom = self.tokens();
        if denom <= 0 {
            return Vec::new();
        }
        // Models past the cut belong to `other` rather than vanishing from the
        // total, which is what used to leave the line short of 100%.
        let mut buckets: Vec<(String, i64)> = items.iter().take(n).cloned().collect();
        let rest: i64 = items.iter().skip(n).map(|(_, t)| t).sum();
        if rest > 0 {
            buckets.push((OTHER_MODELS.to_string(), rest));
        }
        let exact: Vec<f64> =
            buckets.iter().map(|(_, t)| *t as f64 * 100.0 / denom as f64).collect();
        let mut result: Vec<Share> = buckets
            .into_iter()
            .zip(&exact)
            .map(|((name, tokens), share)| (name, tokens, share.trunc() as i64))
            .collect();
        // Hand the leftover points to the largest fractions first.
        let mut order: Vec<usize> = (0..exact.len()).collect();
        order.sort_by(|&a, &b| {
            let fa = exact[a] - exact[a].trunc();
            let fb = exact[b] - exact[b].trunc();
            fb.partial_cmp(&fa).unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut missing = 100 - result.iter().map(|r| r.2).sum::<i64>();
        let mut step = 0;
        while missing > 0 && !order.is_empty() {
            let i = order[step % order.len()];
            result[i].2 += 1;
            missing -= 1;
            step += 1;
        }
        result
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SubWindow {
    pub label: String,
    pub percent: f64,
    #[serde(serialize_with = "ser_ts", deserialize_with = "de_ts")]
    pub resets_at: Option<DateTime<Utc>>,
    pub detail: String,
}

impl SubWindow {
    fn new(label: impl Into<String>, percent: f64, resets_at: Option<DateTime<Utc>>) -> Self {
        SubWindow { label: label.into(), percent, resets_at, detail: String::new() }
    }
}

fn ser_ts<S: Serializer>(ts: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
    match ts {
        Some(t) => s.serialize_str(&t.to_rfc3339()),
        None => s.serialize_none(),
    }
}

fn de_ts<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(parse_ts(Some(&v)))
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct DailyStat {
    pub date: String, // YYYY-MM-DD, local
    pub tokens: i64,
}

fn unknown_mode() -> String {
    "unknown".to_string()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentReport {
    pub agent_id: String,
    pub title: String,
    /// api key | subscription | metered | local | unknown
    #[serde(default = "unknown_mode")]
    pub mode: String,
    pub note: String,
    /// short bracketed tag beside the mode, e.g. "unlimited"
    pub badge: String,
    pub windows: Vec<WindowStat>,
    pub sub_windows: Vec<SubWindow>,
    pub daily: Vec<DailyStat>,
}

impl Default for AgentReport {
    fn default() -> Self {
        AgentReport::new("", "")
    }
}

impl AgentReport {
    pub fn new(agent_id: &str, title: &str) -> Self {
        AgentReport {
            agent_id: agent_id.to_string(),
            title: title.to_string(),
            mode: unknown_mode(),
            note: String::new(),
            badge: String::new(),
            windows: Vec::new(),
            sub_windows: Vec::new(),
            daily: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

/// `value or 0` for a JSON token count.
fn num(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Number(n)) => n.as_i64().unwrap_or_else(|| n.as_f64().unwrap_or(0.0) as i64),
        _ => 0,
    }
}

fn number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        _ => None,
    }
}

/// A JSON value as Python's `str()` would see it for ids and names: strings
/// verbatim, numbers printed, anything empty/false/null treated as absent.
fn text(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
        Some(Value::Number(n)) if n.as_f64() != Some(0.0) => Some(n.to_string()),
        Some(Value::Bool(true)) => Some("True".to_string()),
        _ => None,
    }
}

fn obj(v: Option<&Value>) -> Option<&Map<String, Value>> {
    v.and_then(Value::as_object)
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64() != Some(0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn local_naive(n: NaiveDateTime) -> Option<DateTime<Utc>> {
    Local.from_local_datetime(&n).earliest().map(|t| t.with_timezone(&Utc))
}

/// A timestamp from a log: epoch seconds or milliseconds, or an ISO-8601 string.
/// A string without an offset is local time.
pub fn parse_ts(value: Option<&Value>) -> Option<DateTime<Utc>> {
    match value? {
        Value::Number(n) => {
            let mut v = n.as_f64()?;
            if v > 1e12 {
                v /= 1000.0; // milliseconds
            }
            if !v.is_finite() {
                return None;
            }
            let secs = v.floor();
            let nanos = ((v - secs) * 1e9).round().min(999_999_999.0) as u32;
            DateTime::from_timestamp(secs as i64, nanos)
        }
        Value::String(s) => parse_iso(s),
        _ => None,
    }
}

pub fn parse_iso(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim().replace('Z', "+00:00");
    const AWARE: [&str; 4] = [
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M%:z",
        "%Y-%m-%d %H:%M%:z",
    ];
    for f in AWARE {
        if let Ok(t) = DateTime::parse_from_str(&s, f) {
            return Some(t.with_timezone(&Utc));
        }
    }
    const NAIVE: [&str; 4] =
        ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M", "%Y-%m-%d %H:%M"];
    for f in NAIVE {
        if let Ok(n) = NaiveDateTime::parse_from_str(&s, f) {
            return local_naive(n);
        }
    }
    NaiveDate::parse_from_str(&s, "%Y-%m-%d")
        .ok()
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .and_then(local_naive)
}

fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

fn mtime(path: &Path) -> f64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn walk(root: &Path, ext: &str, cutoff: f64, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(root) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            walk(&path, ext, cutoff, out);
        } else if path.extension().is_some_and(|e| e == ext) {
            match fs::metadata(&path).and_then(|m| m.modified()) {
                Ok(t) => {
                    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
                    if secs >= cutoff && fs::metadata(&path).is_ok_and(|m| m.is_file()) {
                        out.push(path);
                    }
                }
                Err(_) => continue,
            }
        }
    }
}

fn recent_files(root: &Path, ext: &str) -> Vec<PathBuf> {
    if !root.is_dir() {
        return Vec::new();
    }
    let mut out = Vec::new();
    walk(root, ext, now_secs() - SCAN_DAYS * 86400.0, &mut out);
    out
}

fn iter_recent_jsonl(root: &Path) -> Vec<PathBuf> {
    recent_files(root, "jsonl")
}

/// Every JSON line of a log; a torn write of the live session is skipped.
fn lines(path: &Path) -> Vec<Value> {
    let Ok(file) = fs::File::open(path) else { return Vec::new() };
    let mut reader = BufReader::new(file);
    let mut out = Vec::new();
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                let line = String::from_utf8_lossy(&buf);
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<Value>(line) {
                    out.push(v);
                }
            }
        }
    }
    out
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/"))
}

fn env_dir(var: &str, default: impl FnOnce() -> PathBuf) -> PathBuf {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => default(),
    }
}

fn env_set(var: &str) -> bool {
    std::env::var_os(var).is_some_and(|v| !v.is_empty())
}

fn make_windows() -> Vec<WindowStat> {
    WINDOWS.iter().map(|l| WindowStat::new(l)).collect()
}

/// Cumulative windows by index: Today = same calendar day, 7d/30d = trailing
/// days (7d includes today, 30d includes 7d). Unparseable timestamps go to 30d
/// only; data older than 30d is dropped.
fn windows_for(ts: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Vec<usize> {
    let Some(ts) = ts else { return vec![2] };
    let local_now = now.with_timezone(&Local);
    let local_ts = ts.with_timezone(&Local);
    let age = (local_now - local_ts).num_microseconds().map(|u| u as f64 / 1e6).unwrap_or(f64::MAX);
    let mut out = Vec::new();
    if local_ts.date_naive() == local_now.date_naive() {
        out.push(0);
    }
    if age < 7.0 * 86400.0 {
        out.push(1);
    }
    if age < 30.0 * 86400.0 {
        out.push(2);
    }
    out
}

/// Count tokens on the calendar day they were spent.
///
/// The calendar shows the current month, which is independent of the rolling
/// 30d window: in a 31-day month's last days the 1st is already older than 30
/// days, and it must still show. A record with no timestamp has no day, so it
/// stays off the calendar rather than being piled onto today.
fn add_day(per_day: &mut HashMap<String, i64>, ts: Option<DateTime<Utc>>, now: DateTime<Utc>, tokens: i64) {
    let Some(ts) = ts else { return };
    if tokens <= 0 {
        return;
    }
    let local = ts.with_timezone(&Local).date_naive();
    let today = now.with_timezone(&Local).date_naive();
    if (local.year(), local.month()) != (today.year(), today.month()) || local > today {
        return;
    }
    *per_day.entry(local.format("%Y-%m-%d").to_string()).or_insert(0) += tokens;
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let next = NaiveDate::from_ymd_opt(ny, nm, 1).expect("valid month");
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid month");
    (next - first).num_days() as u32
}

/// The current calendar month, 1st through last day. Days that have not
/// happened yet carry zero tokens (rendered as empty squares).
fn build_daily(per_day: &HashMap<String, i64>, now: DateTime<Utc>) -> Vec<DailyStat> {
    let today = now.with_timezone(&Local).date_naive();
    let first = today.with_day(1).expect("day 1");
    (0..days_in_month(today.year(), today.month()))
        .map(|i| {
            let date = (first + chrono::Duration::days(i as i64)).format("%Y-%m-%d").to_string();
            let tokens = per_day.get(&date).copied().unwrap_or(0);
            DailyStat { date, tokens }
        })
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn add_usage(
    window: &mut WindowStat,
    model: &str,
    inp: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    cost: f64,
    cost_known: bool,
) {
    let model = if model.is_empty() { "unknown" } else { model };
    let stat = window.models.entry(model.to_string()).or_insert_with(|| ModelStat::new(model));
    stat.inp += inp;
    stat.cache_read += cache_read;
    stat.cache_write += cache_write;
    stat.output += output;
    stat.cost += cost;
    if !cost_known {
        stat.unpriced += inp + cache_read + cache_write + output;
    }
}

/// Adds one record to every window it belongs to.
#[allow(clippy::too_many_arguments)]
fn add_to_windows(
    windows: &mut [WindowStat],
    targets: &[usize],
    model: &str,
    inp: i64,
    cache_read: i64,
    cache_write: i64,
    output: i64,
    cost: f64,
    known: bool,
) {
    for &i in targets {
        add_usage(&mut windows[i], model, inp, cache_read, cache_write, output, cost, known);
    }
}

enum HttpError {
    Status(u16),
    Network,
}

fn http_get_json(url: &str, headers: &[(&str, String)]) -> Result<Result<Value, ()>, HttpError> {
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(HTTP_TIMEOUT)).build().into();
    let mut request = agent.get(url);
    for (name, value) in headers {
        request = request.header(*name, value);
    }
    match request.call() {
        Ok(mut response) => {
            let body = response.body_mut().read_to_string().map_err(|_| HttpError::Network)?;
            Ok(serde_json::from_str(&body).map_err(|_| ()))
        }
        Err(ureq::Error::StatusCode(code)) => Err(HttpError::Status(code)),
        Err(_) => Err(HttpError::Network),
    }
}

/// Runs a command for its stdout, giving up after `timeout`.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Option<(bool, String)> {
    let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::null()).stdin(Stdio::null()).spawn().ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some((status.success(), reader.join().unwrap_or_default())),
            Ok(None) if start.elapsed() < timeout => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

/// Python's `str.capitalize`: first letter upper, the rest lower.
fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars.flat_map(char::to_lowercase)).collect(),
        None => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Claude Code: ~/.claude/projects/<project>/<session>.jsonl
// ---------------------------------------------------------------------------

fn claude_dir() -> PathBuf {
    env_dir("CLAUDE_CONFIG_DIR", || home().join(".claude"))
}

fn load_claude_credentials() -> Option<Map<String, Value>> {
    let path = claude_dir().join(".credentials.json");
    let raw = if path.is_file() {
        fs::read_to_string(&path).ok()
    } else if cfg!(target_os = "macos") {
        run_with_timeout(
            Command::new("security").args(["find-generic-password", "-s", "Claude Code-credentials", "-w"]),
            Duration::from_secs(5),
        )
        .and_then(|(ok, out)| (ok && !out.trim().is_empty()).then(|| out.trim().to_string()))
    } else {
        None
    };
    let raw = raw.filter(|r| !r.is_empty())?;
    let data: Value = serde_json::from_str(&raw).ok()?;
    data.get("claudeAiOauth")?.as_object().cloned()
}

fn claude_sub_windows(creds: &Map<String, Value>) -> Vec<SubWindow> {
    let Some(token) = creds.get("accessToken").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
        return Vec::new();
    };
    if let Some(expires) = number(creds.get("expiresAt")) {
        if expires / 1000.0 < now_secs() {
            return Vec::new();
        }
    }
    let headers = [
        ("Authorization", format!("Bearer {token}")),
        ("anthropic-beta", "oauth-2025-04-20".to_string()),
        ("Accept", "application/json".to_string()),
        ("User-Agent", "herdr-agent-usage".to_string()),
    ];
    let Ok(Ok(Value::Object(data))) = http_get_json("https://api.anthropic.com/api/oauth/usage", &headers)
    else {
        return Vec::new();
    };
    let mut windows = Vec::new();
    match data.get("limits") {
        Some(Value::Array(limits)) if !limits.is_empty() => {
            for item in limits {
                let Some(item) = item.as_object() else { continue };
                let Some(percent) = number(item.get("percent")) else { continue };
                let kind = text(item.get("kind")).unwrap_or_else(|| "limit".to_string());
                let label = match kind.as_str() {
                    "session" => "5h window".to_string(),
                    "weekly_all" => "week (all)".to_string(),
                    other => capitalize(&other.replace('_', " ")),
                };
                windows.push(SubWindow::new(label, percent, parse_ts(item.get("resets_at"))));
            }
        }
        _ => {
            for (key, label) in [
                ("five_hour", "5h window"),
                ("seven_day", "week"),
                ("seven_day_opus", "week (opus)"),
                ("seven_day_sonnet", "week (sonnet)"),
            ] {
                let Some(entry) = obj(data.get(key)) else { continue };
                if let Some(util) = number(entry.get("utilization")) {
                    windows.push(SubWindow::new(label, util, parse_ts(entry.get("resets_at"))));
                }
            }
        }
    }
    windows
}

type ClaudeRecord = (Map<String, Value>, String, Option<DateTime<Utc>>);

fn collect_claude() -> AgentReport {
    let mut report = AgentReport::new("claude", "Claude Code");
    let root = claude_dir().join("projects");
    if !root.is_dir() {
        report.note = "no ~/.claude/projects found".to_string();
        return report;
    }

    let api_key = env_set("ANTHROPIC_API_KEY");
    let creds = if api_key { None } else { load_claude_credentials() };
    let estimate = if api_key {
        report.mode = "api key".to_string();
        true
    } else if creds.is_some() {
        report.mode = "subscription".to_string();
        false
    } else {
        true
    };

    let now = Utc::now();
    let mut windows = make_windows();
    let mut per_day = HashMap::new();
    // Claude Code writes one log entry per content block (thinking, text, each
    // tool_use) of a response, and every one of them repeats the response's full
    // usage; resumed and forked sessions and subagent logs copy entries again.
    // One API response is one (message.id, requestId), so usage is counted once
    // per key -- summing every entry roughly doubled the totals. Streaming can
    // leave an early copy with a partial output count, so the largest wins.
    let mut seen: IndexMap<(String, String), ClaudeRecord> = IndexMap::new();
    let mut anonymous: Vec<ClaudeRecord> = Vec::new();
    for path in iter_recent_jsonl(&root) {
        for entry in lines(&path) {
            if entry.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(message) = obj(entry.get("message")) else { continue };
            let Some(usage) = obj(message.get("usage")) else { continue };
            let model = text(message.get("model")).unwrap_or_default();
            let record = (usage.clone(), model, parse_ts(entry.get("timestamp")));
            let Some(msg_id) = text(message.get("id")) else {
                anonymous.push(record);
                continue;
            };
            let key = (msg_id, text(entry.get("requestId")).unwrap_or_default());
            let replace = match seen.get(&key) {
                None => true,
                Some(prev) => num(usage.get("output_tokens")) > num(prev.0.get("output_tokens")),
            };
            if replace {
                seen.insert(key, record);
            }
        }
    }
    for (usage, model, ts) in seen.into_values().chain(anonymous) {
        let targets = windows_for(ts, now);
        let inp = num(usage.get("input_tokens"));
        let cache_read = num(usage.get("cache_read_input_tokens"));
        let cache_write = num(usage.get("cache_creation_input_tokens"));
        let output = num(usage.get("output_tokens"));
        add_day(&mut per_day, ts, now, inp + cache_read + cache_write + output);
        if targets.is_empty() {
            continue;
        }
        let (cost, known) = if estimate {
            let write_1h = obj(usage.get("cache_creation"))
                .map(|split| num(split.get("ephemeral_1h_input_tokens")))
                .unwrap_or(0);
            estimate_cost(&model, inp, cache_read, cache_write, output, write_1h)
        } else {
            (0.0, false)
        };
        add_to_windows(&mut windows, &targets, &model, inp, cache_read, cache_write, output, cost, known);
    }
    report.windows = windows;
    report.daily = build_daily(&per_day, now);
    if let Some(creds) = &creds {
        report.sub_windows = claude_sub_windows(creds);
    }
    report
}

// ---------------------------------------------------------------------------
// Codex: ~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl
// ---------------------------------------------------------------------------

fn codex_home() -> PathBuf {
    env_dir("CODEX_HOME", || home().join(".codex"))
}

fn load_codex_auth() -> Option<Map<String, Value>> {
    let raw = fs::read_to_string(codex_home().join("auth.json")).ok()?;
    serde_json::from_str::<Value>(&raw).ok()?.as_object().cloned()
}

fn codex_window_label(seconds: f64) -> String {
    if seconds <= 0.0 {
        return "window".to_string();
    }
    let seconds = seconds as i64;
    if seconds % 86400 == 0 {
        let days = seconds / 86400;
        return if days == 7 { "week".to_string() } else { format!("{days}d window") };
    }
    if seconds % 3600 == 0 {
        return format!("{}h window", seconds / 3600);
    }
    format!("{}m window", seconds / 60)
}

fn codex_window_of(entry: &Map<String, Value>, label: &str) -> Option<SubWindow> {
    let percent = number(entry.get("used_percent"))
        .or_else(|| number(entry.get("percent")))
        .or_else(|| number(entry.get("remaining_percent")).map(|r| 100.0 - r))?;
    let mut resets = parse_ts(entry.get("resets_at")).or_else(|| parse_ts(entry.get("reset_at")));
    if resets.is_none() {
        if let Some(after) = number(entry.get("reset_after_seconds")) {
            resets = Some(Utc::now() + chrono::Duration::milliseconds((after * 1000.0) as i64));
        }
    }
    // A snapshot window with no duration is still primary or secondary, so keep
    // the rank name rather than the generic "window".
    let name = text(entry.get("limit_name")).unwrap_or_else(|| match number(entry.get("limit_window_seconds")) {
        Some(seconds) => codex_window_label(seconds),
        None => label.to_string(),
    });
    Some(SubWindow::new(name, percent, resets))
}

/// Codex spells the same window two ways. The backend response -- and the
/// token_count event the CLI copies it into -- nests them as `primary` /
/// `secondary`; the app-server snapshot structs name them `primary_window` /
/// `secondary_window`. Both spellings are in the codex binary's serde metadata,
/// so read either. Percent is `used_percent`, or `remaining_percent` flipped.
const CODEX_WINDOW_KEYS: [[&str; 2]; 2] = [["primary", "primary_window"], ["secondary", "secondary_window"]];

/// Windows out of any Codex rate-limit shape: a /wham/usage response, its
/// `rate_limit` object, or a `rate_limits` object from a session log.
fn codex_rate_windows(payload: Option<&Value>) -> Vec<SubWindow> {
    let mut containers: Vec<&Map<String, Value>> = Vec::new();
    if let Some(payload) = obj(payload) {
        containers.push(payload);
        for key in ["rate_limit", "rate_limits"] {
            if let Some(inner) = obj(payload.get(key)) {
                containers.push(inner);
            }
        }
        if let Some(Value::Array(extra)) = payload.get("additional_rate_limits") {
            containers.extend(extra.iter().filter_map(Value::as_object));
        }
    }
    let mut windows = Vec::new();
    let mut seen: HashSet<(String, i64)> = HashSet::new();
    for container in containers {
        for spelling in CODEX_WINDOW_KEYS {
            for key in spelling {
                let Some(entry) = obj(container.get(key)) else { continue };
                let rank = capitalize(key.split('_').next().unwrap_or(key));
                let Some(window) = codex_window_of(entry, &rank) else { continue };
                let fingerprint = (window.label.clone(), (window.percent * 10.0).round() as i64);
                if seen.insert(fingerprint) {
                    windows.push(window);
                }
            }
        }
    }
    windows
}

/// A header tag for when the plan really has no window, so an empty bar list is
/// never mistaken for a request that went wrong.
fn codex_limit_badge(data: &Map<String, Value>) -> String {
    match obj(data.get("credits")) {
        Some(credits) if truthy(credits.get("unlimited")) => "unlimited".to_string(),
        _ => String::new(),
    }
}

/// The last window state the CLI itself saw, read from session logs: works with
/// no network at all, so a blocked endpoint still shows real bars.
fn codex_windows_from_logs(home: &Path) -> Vec<SubWindow> {
    let mut files = iter_recent_jsonl(&home.join("sessions"));
    files.sort_by(|a, b| mtime(b).partial_cmp(&mtime(a)).unwrap_or(std::cmp::Ordering::Equal));
    for path in files.iter().take(3) {
        let mut last: Option<Value> = None;
        for entry in lines(path) {
            if let Some(payload) = obj(entry.get("payload")) {
                if let Some(limits @ Value::Object(_)) = payload.get("rate_limits") {
                    last = Some(limits.clone());
                }
            }
        }
        let windows = codex_rate_windows(last.as_ref());
        if !windows.is_empty() {
            return windows;
        }
    }
    Vec::new()
}

/// Windows, a header badge, and a note -- an empty list from a failed request
/// must never be shown as "unlimited".
fn codex_sub_windows(auth: &Map<String, Value>) -> (Vec<SubWindow>, String, String) {
    let empty = || (Vec::new(), String::new(), String::new());
    let Some(tokens) = obj(auth.get("tokens")) else { return empty() };
    let Some(token) = tokens.get("access_token").and_then(Value::as_str).filter(|t| !t.is_empty()) else {
        return empty();
    };
    let mut headers = vec![
        ("Authorization", format!("Bearer {token}")),
        ("Accept", "application/json".to_string()),
        ("User-Agent", "herdr-agent-usage".to_string()),
    ];
    if let Some(account) = text(tokens.get("account_id")) {
        headers.push(("ChatGPT-Account-Id", account));
    }
    let note = |s: &str| (Vec::new(), String::new(), s.to_string());
    match http_get_json("https://chatgpt.com/backend-api/wham/usage", &headers) {
        Err(HttpError::Status(401 | 403)) => note("codex sign-in expired: run `codex login`"),
        Err(HttpError::Status(code)) => note(&format!("codex limits unavailable (HTTP {code})")),
        Err(HttpError::Network) => note("codex limits unreachable (no network to chatgpt.com)"),
        Ok(Err(())) => note("codex limits unreadable (unexpected response)"),
        Ok(Ok(Value::Object(data))) => {
            let windows = codex_rate_windows(Some(&Value::Object(data.clone())));
            (windows, codex_limit_badge(&data), String::new())
        }
        Ok(Ok(_)) => empty(),
    }
}

const CODEX_KEYS: [&str; 5] = [
    "input_tokens",
    "cached_input_tokens",
    "cache_write_input_tokens",
    "output_tokens",
    "reasoning_output_tokens",
];

fn codex_delta(cur: &Map<String, Value>, prev: Option<&Map<String, Value>>) -> Map<String, Value> {
    CODEX_KEYS
        .iter()
        .map(|k| {
            let c = num(cur.get(*k));
            let v = match prev {
                None => c,
                Some(p) => (c - num(p.get(*k))).max(0),
            };
            (k.to_string(), Value::from(v))
        })
        .collect()
}

type UsageRecord = (Map<String, Value>, Option<String>, Option<DateTime<Utc>>);

/// (usage, model, timestamp) for each model call in one rollout file.
///
/// Newer CLIs write a `token_usage_record` per response: its `usage` is that one
/// call, `turn_token_usage` only the running total of the current turn (it
/// restarts every turn, so the last one is not the session). Each is counted by
/// its own timestamp -- a session that runs past midnight lands on both days --
/// and against the model of its turn. `response_id` is unique per call, so a
/// forked session that copies its parent's records does not count them again.
///
/// Older logs only have `token_count` events whose `total_token_usage` is the
/// session total so far; there the usage of each event is the growth since the
/// previous one.
fn codex_usage_records(path: &Path, seen_responses: &mut HashSet<String>) -> Vec<UsageRecord> {
    let mut turn_model: HashMap<String, String> = HashMap::new();
    let mut model: Option<String> = None;
    let mut records = Vec::new();
    let mut totals: Vec<UsageRecord> = Vec::new();
    let no_payload = Map::new();
    for entry in lines(path) {
        if !entry.is_object() {
            continue;
        }
        let etype = entry.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = obj(entry.get("payload")).unwrap_or(&no_payload);
        if etype == "turn_context" && truthy(payload.get("model")) {
            let m = text(payload.get("model")).unwrap_or_default();
            if let Some(turn) = text(payload.get("turn_id")) {
                turn_model.insert(turn, m.clone());
            }
            model = Some(m);
        } else if etype == "turn_context" {
            // a turn context without a model changes nothing
        } else if etype == "token_usage_record" {
            let Some(usage) = obj(payload.get("usage")) else { continue };
            if let Some(rid) = text(payload.get("response_id")) {
                if !seen_responses.insert(rid) {
                    continue;
                }
            }
            let turn = text(payload.get("turn_id")).unwrap_or_else(|| "None".to_string());
            let m = turn_model.get(&turn).cloned().or_else(|| model.clone());
            records.push((usage.clone(), m, parse_ts(entry.get("timestamp"))));
        } else {
            // event_msg/token_count (current) or a bare token_count (old CLI)
            let info = if etype == "event_msg" && payload.get("type").and_then(Value::as_str) == Some("token_count") {
                payload.get("info")
            } else if etype == "token_count" {
                entry.get("info")
            } else {
                None
            };
            if let Some(total) = obj(info).and_then(|i| obj(i.get("total_token_usage"))) {
                totals.push((total.clone(), model.clone(), parse_ts(entry.get("timestamp"))));
            }
        }
    }
    if !records.is_empty() {
        return records;
    }
    let mut out = Vec::new();
    let mut prev: Option<Map<String, Value>> = None;
    for (total, m, ts) in totals {
        let delta = codex_delta(&total, prev.as_ref());
        prev = Some(total);
        if delta.values().any(|v| num(Some(v)) != 0) {
            out.push((delta, m, ts));
        }
    }
    out
}

fn collect_codex() -> AgentReport {
    let mut report = AgentReport::new("codex", "Codex");
    let home = codex_home();
    let sessions = home.join("sessions");
    if !sessions.is_dir() {
        report.note = "no ~/.codex/sessions found".to_string();
        return report;
    }

    let auth = load_codex_auth().unwrap_or_default();
    let has_oauth = obj(auth.get("tokens")).is_some_and(|t| truthy(t.get("access_token")));
    let estimate = if has_oauth && auth.get("auth_mode").and_then(Value::as_str) != Some("apikey") {
        report.mode = "subscription".to_string();
        false
    } else if env_set("OPENAI_API_KEY") || truthy(auth.get("OPENAI_API_KEY")) {
        report.mode = "api key".to_string();
        true
    } else {
        true
    };

    let now = Utc::now();
    let mut windows = make_windows();
    let mut per_day = HashMap::new();
    let mut seen_responses = HashSet::new();
    for path in iter_recent_jsonl(&sessions) {
        let file_time = mtime(&path);
        for (usage, model, ts) in codex_usage_records(&path, &mut seen_responses) {
            // OpenAI usage nests its parts: cached_input_tokens is a share of
            // input_tokens and reasoning_output_tokens a share of output_tokens,
            // so the total is input + output (= total_tokens). Adding the parts
            // on top counted cached input twice.
            let inp_all = num(usage.get("input_tokens"));
            let cached = num(usage.get("cached_input_tokens")).min(inp_all);
            let cache_write = num(usage.get("cache_write_input_tokens")).min(inp_all - cached);
            let inp = inp_all - cached - cache_write;
            let output = num(usage.get("output_tokens"));
            let stamp = ts.or_else(|| parse_ts(Some(&Value::from(file_time))));
            add_day(&mut per_day, stamp, now, inp_all + output);
            let targets = windows_for(stamp, now);
            if targets.is_empty() {
                continue;
            }
            let model = model.unwrap_or_default();
            let (cost, known) =
                if estimate { estimate_cost(&model, inp, cached, cache_write, output, 0) } else { (0.0, false) };
            add_to_windows(&mut windows, &targets, &model, inp, cached, cache_write, output, cost, known);
        }
    }
    report.windows = windows;
    report.daily = build_daily(&per_day, now);
    if has_oauth {
        let (mut subs, badge, note) = codex_sub_windows(&auth);
        if subs.is_empty() {
            subs = codex_windows_from_logs(&home);
        }
        if subs.is_empty() {
            report.badge = badge;
            if !note.is_empty() && report.note.is_empty() {
                report.note = note;
            }
        }
        report.sub_windows = subs;
    }
    report
}

// ---------------------------------------------------------------------------
// Pi: ~/.pi/agent/sessions/<dir>/<ts>_<id>.jsonl
// ---------------------------------------------------------------------------

/// Sets the mode of a report whose cost comes with its own logs.
fn finish_metered(report: &mut AgentReport, total_tokens: i64, total_cost: f64) {
    if total_tokens == 0 {
        report.note = "no assistant usage in recent sessions".to_string();
    } else if total_cost > 0.0 {
        report.mode = "metered".to_string();
    } else {
        report.mode = "local".to_string();
    }
}

fn collect_pi() -> AgentReport {
    let mut report = AgentReport::new("pi", "Pi");
    let root = home().join(".pi").join("agent").join("sessions");
    if !root.is_dir() {
        report.note = "no ~/.pi/agent/sessions found".to_string();
        return report;
    }

    let now = Utc::now();
    let mut windows = make_windows();
    let mut per_day = HashMap::new();
    let mut total_cost = 0.0;
    let mut total_tokens = 0;
    let mut seen = HashSet::new(); // a forked or branched session copies its parent's messages
    for path in iter_recent_jsonl(&root) {
        for entry in lines(&path) {
            if entry.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            let Some(message) = obj(entry.get("message")) else { continue };
            if message.get("role").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let Some(usage) = obj(message.get("usage")) else { continue };
            if let Some(key) = text(message.get("responseId")).or_else(|| text(entry.get("id"))) {
                if !seen.insert(key) {
                    continue;
                }
            }
            let model = text(message.get("model")).unwrap_or_default();
            let inp = num(usage.get("input"));
            let cache_read = num(usage.get("cacheRead"));
            let cache_write = num(usage.get("cacheWrite"));
            let output = num(usage.get("output"));
            // The same sum the per-model rows add up, so the calendar and the
            // windows can never disagree about what a token is.
            let n_tokens = inp + cache_read + cache_write + output;
            let ts = parse_ts(message.get("timestamp")).or_else(|| parse_ts(entry.get("timestamp")));
            add_day(&mut per_day, ts, now, n_tokens);
            let targets = windows_for(ts, now);
            if targets.is_empty() {
                continue;
            }
            let (cost, known) = match obj(usage.get("cost")).and_then(|c| number(c.get("total"))) {
                Some(total) => (total, true),
                None => estimate_cost(&model, inp, cache_read, cache_write, output, 0),
            };
            total_cost += cost;
            total_tokens += n_tokens;
            add_to_windows(&mut windows, &targets, &model, inp, cache_read, cache_write, output, cost, known);
        }
    }
    report.windows = windows;
    report.daily = build_daily(&per_day, now);
    finish_metered(&mut report, total_tokens, total_cost);
    report
}

// ---------------------------------------------------------------------------
// OpenCode: ~/.local/share/opencode/opencode.db (SQLite), or the JSON files
// under storage/message/ that versions before the database wrote.
// ---------------------------------------------------------------------------

fn opencode_data_dir() -> PathBuf {
    env_dir("XDG_DATA_HOME", || home().join(".local").join("share")).join("opencode")
}

/// Assistant messages by id. The `message` table holds the full record; newer
/// builds also mirror sessions into `session_message` (same ids, no `total`),
/// so that one only fills in ids the first one lacks.
fn opencode_db_messages(db: &Path, cutoff_ms: i64) -> IndexMap<String, Map<String, Value>> {
    use rusqlite::{Connection, OpenFlags};
    let mut out = IndexMap::new();
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let Ok(conn) = Connection::open_with_flags(db, flags) else { return out };
    let _ = conn.busy_timeout(Duration::from_secs(2));
    let tables: HashSet<String> = match conn.prepare("SELECT name FROM sqlite_master WHERE type='table'") {
        Ok(mut stmt) => match stmt.query_map([], |row| row.get::<_, String>(0)) {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => return out,
        },
        Err(_) => return out,
    };
    let mut queries = Vec::new();
    if tables.contains("message") {
        queries.push("SELECT id, data FROM message WHERE time_created >= ?1");
    }
    if tables.contains("session_message") {
        queries.push("SELECT id, data FROM session_message WHERE type = 'assistant' AND time_created >= ?1");
    }
    for query in queries {
        let Ok(mut stmt) = conn.prepare(query) else { return out };
        let rows = stmt.query_map([cutoff_ms], |row| {
            let id: rusqlite::types::Value = row.get(0)?;
            let data: rusqlite::types::Value = row.get(1)?;
            Ok((id, data))
        });
        let Ok(rows) = rows else { return out };
        for (id, data) in rows.flatten() {
            let id = match id {
                rusqlite::types::Value::Text(s) => s,
                rusqlite::types::Value::Integer(i) => i.to_string(),
                rusqlite::types::Value::Real(f) => f.to_string(),
                rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).into_owned(),
                rusqlite::types::Value::Null => "None".to_string(),
            };
            if out.contains_key(&id) {
                continue;
            }
            let parsed = match data {
                rusqlite::types::Value::Text(s) => serde_json::from_str::<Value>(&s).ok(),
                rusqlite::types::Value::Blob(b) => serde_json::from_slice::<Value>(&b).ok(),
                _ => None,
            };
            if let Some(Value::Object(message)) = parsed {
                out.insert(id, message);
            }
        }
    }
    out
}

fn opencode_file_messages(root: &Path) -> IndexMap<String, Map<String, Value>> {
    let mut out = IndexMap::new();
    for path in recent_files(root, "json") {
        let Ok(raw) = fs::read_to_string(&path) else { continue };
        let Ok(Value::Object(message)) = serde_json::from_str::<Value>(&raw) else { continue };
        let id = text(message.get("id"))
            .unwrap_or_else(|| path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default());
        out.insert(id, message);
    }
    out
}

fn opencode_model(message: &Map<String, Value>) -> String {
    let model = text(message.get("modelID"))
        .or_else(|| obj(message.get("model")).and_then(|m| text(m.get("id"))));
    // Local servers report the weights file as the model: keep only its name.
    match model {
        Some(m) => m.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string(),
        None => "unknown".to_string(),
    }
}

fn collect_opencode() -> AgentReport {
    let mut report = AgentReport::new("opencode", "OpenCode");
    let home = opencode_data_dir();
    let db = home.join("opencode.db");
    let legacy = home.join("storage").join("message");
    if !db.is_file() && !legacy.is_dir() {
        report.note = "no ~/.local/share/opencode data found".to_string();
        return report;
    }

    let now = Utc::now();
    let cutoff = now_secs() - SCAN_DAYS * 86400.0;
    let mut messages = opencode_file_messages(&legacy);
    if db.is_file() {
        messages.extend(opencode_db_messages(&db, (cutoff * 1000.0) as i64));
    }

    let mut windows = make_windows();
    let mut per_day = HashMap::new();
    let mut total_cost = 0.0;
    let mut total_tokens = 0;
    for message in messages.values() {
        let is_assistant = match message.get("role") {
            None => true,
            Some(role) => role.as_str() == Some("assistant"),
        };
        if !is_assistant {
            continue;
        }
        let Some(tokens) = obj(message.get("tokens")) else { continue };
        let cache = obj(tokens.get("cache"));
        let inp = num(tokens.get("input"));
        let cache_read = cache.map(|c| num(c.get("read"))).unwrap_or(0);
        let cache_write = cache.map(|c| num(c.get("write"))).unwrap_or(0);
        // OpenCode keeps reasoning apart from output; both are generated tokens.
        let output = num(tokens.get("output")) + num(tokens.get("reasoning"));
        let n_tokens = inp + cache_read + cache_write + output;
        if n_tokens <= 0 {
            continue;
        }
        let ts = obj(message.get("time")).and_then(|t| parse_ts(t.get("created")));
        add_day(&mut per_day, ts, now, n_tokens);
        let targets = windows_for(ts, now);
        if targets.is_empty() {
            continue;
        }
        let model = opencode_model(message);
        let (cost, known) = match number(message.get("cost")) {
            Some(c) => (c, true),
            None => estimate_cost(&model, inp, cache_read, cache_write, output, 0),
        };
        total_cost += cost;
        total_tokens += n_tokens;
        add_to_windows(&mut windows, &targets, &model, inp, cache_read, cache_write, output, cost, known);
    }
    report.windows = windows;
    report.daily = build_daily(&per_day, now);
    finish_metered(&mut report, total_tokens, total_cost);
    report
}

// ---------------------------------------------------------------------------

pub fn is_collector(agent_id: &str) -> bool {
    COLLECTORS.contains(&agent_id)
}

fn run_collector(agent_id: &str) -> AgentReport {
    match agent_id {
        "claude" => collect_claude(),
        "codex" => collect_codex(),
        "pi" => collect_pi(),
        "opencode" => collect_opencode(),
        other => AgentReport::new(other, other),
    }
}

/// Python's `str.title` for a one-word agent id.
fn title_case(id: &str) -> String {
    capitalize(id)
}

/// Collects all agents concurrently (log scans + optional HTTP in parallel).
/// One broken agent must not kill the pane: a panic becomes an error note.
pub fn collect_all(agent_ids: &[String]) -> Vec<AgentReport> {
    let mut reports: Vec<AgentReport> = std::thread::scope(|scope| {
        let handles: Vec<_> = agent_ids
            .iter()
            .map(|id| (id, scope.spawn(move || run_collector(id))))
            .collect();
        handles
            .into_iter()
            .map(|(id, handle)| {
                handle.join().unwrap_or_else(|panic| {
                    let msg = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "collector panicked".to_string());
                    let mut r = AgentReport::new(id, &title_case(id));
                    r.note = format!("error: {msg}");
                    r
                })
            })
            .collect()
    });
    let order = |id: &str| agent_ids.iter().position(|a| a == id).unwrap_or(99);
    reports.sort_by_key(|r| order(&r.agent_id));
    reports
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(models: &[(&str, i64)]) -> WindowStat {
        let mut w = WindowStat::new("30d");
        for (name, tokens) in models {
            let mut m = ModelStat::new(name);
            m.inp = *tokens;
            w.models.insert(name.to_string(), m);
        }
        w
    }

    #[test]
    fn shares_add_up_to_100() {
        let w = window(&[("a", 333), ("b", 333), ("c", 334), ("d", 1)]);
        let shares = w.model_shares(3);
        assert_eq!(shares.iter().map(|s| s.2).sum::<i64>(), 100);
        assert_eq!(shares.last().unwrap().0, OTHER_MODELS);
        assert_eq!(shares[0].0, "c");
    }

    #[test]
    fn pricing_strips_date_and_path() {
        assert_eq!(price_for("anthropic/claude-opus-4-1-20250805").unwrap().0, 15.0);
        assert_eq!(price_for("claude-opus-9").unwrap().0, 4.0);
        assert!(price_for("llama").is_none());
        let (cost, known) = estimate_cost("claude-sonnet-4", 1_000_000, 0, 1_000_000, 0, 500_000);
        assert!(known);
        assert!((cost - (3.0 + 0.5 * 3.75 + 0.5 * 6.0)).abs() < 1e-9);
    }

    #[test]
    fn timestamps() {
        let t = parse_iso("2026-11-16T10:00:00.123Z").unwrap();
        assert_eq!(t.to_rfc3339(), "2026-11-16T10:00:00.123+00:00");
        assert_eq!(parse_ts(Some(&Value::from(1_700_000_000_000i64))).unwrap().timestamp(), 1_700_000_000);
    }

    #[test]
    fn codex_windows_dedupe_spellings() {
        let v: Value = serde_json::from_str(
            r#"{"rate_limit":{"primary_window":{"used_percent":12.0,"limit_window_seconds":18000},
                "secondary_window":{"used_percent":3,"limit_window_seconds":604800}},
                "primary":{"used_percent":12.0,"limit_window_seconds":18000}}"#,
        )
        .unwrap();
        let w = codex_rate_windows(Some(&v));
        let labels: Vec<_> = w.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["5h window", "week"]);
    }
}
