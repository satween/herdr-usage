//! Per-token prices for the cost estimate of API-key / metered usage.
//!
//! Prices come from the public models.dev catalog, fetched at most once a day
//! into `$HERDR_PLUGIN_STATE_DIR/prices.json`, so a new model is priced without
//! a new plugin release. A built-in table covers the time before the first
//! download and machines without network access.
//!
//! Lookup order for a model id: catalog exact, built-in exact, catalog family
//! (the newest model of the same family), built-in family.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::thread::JoinHandle;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const CATALOG_URL: &str = "https://models.dev/api.json";
/// Providers whose models the catalog keeps: the ones whose agents log tokens
/// but no cost (Claude Code, Codex), plus Google for Pi / OpenCode fallbacks.
const PROVIDERS: [&str; 3] = ["anthropic", "openai", "google"];
const REFRESH_AFTER: Duration = Duration::from_secs(24 * 3600);
const RETRY_AFTER: Duration = Duration::from_secs(3600);
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// USD per 1M tokens: (input, output, cache_read, cache_write_5m); None = derive
/// from input (cache read 0.1x, 5-minute cache write 1.25x). A 1-hour cache
/// write is billed at 2x input, which `estimate_cost` applies on its own.
pub type Price = (f64, f64, Option<f64>, Option<f64>);

const BUILTIN_EXACT: &[(&str, Price)] = &[
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

/// Offline fallback when the exact model is not known: the current generation
/// of each family, so an unknown Claude is not billed at the Opus 4 / 4.1 rate
/// of $15/$75 that no current Opus charges.
const BUILTIN_FAMILY: &[(&str, Price)] = &[
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

// ---------------------------------------------------------------------------
// model ids
// ---------------------------------------------------------------------------

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

/// `anthropic/Claude-Opus-4-1-20250805` -> `claude-opus-4-1`.
fn normalize(model: &str) -> String {
    let lower = model.to_lowercase();
    let last = lower.rsplit('/').next().unwrap_or("");
    strip_date_suffix(last).to_string()
}

/// A model id with its version taken out: `claude-opus-5-5` -> `claude-opus`,
/// `gpt-6.2-sol` -> `gpt-sol`, `o4-mini` -> `o-mini`. Models of one family
/// share it whatever version they carry.
fn family_of(model: &str) -> String {
    model
        .split('-')
        .map(|part| part.trim_matches(|c: char| c.is_ascii_digit() || c == '.'))
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

// ---------------------------------------------------------------------------
// catalog
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
struct CatalogModel {
    input: f64,
    output: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_read: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    cache_write: Option<f64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    release_date: String,
}

impl CatalogModel {
    fn price(&self) -> Price {
        (self.input, self.output, self.cache_read, self.cache_write)
    }
}

/// `prices.json`: the trimmed catalog and when it was last fetched / tried.
#[derive(Default, Serialize, Deserialize)]
struct CatalogFile {
    #[serde(default)]
    fetched_at: Option<String>,
    #[serde(default)]
    checked_at: Option<String>,
    #[serde(default)]
    models: HashMap<String, CatalogModel>,
}

struct Catalog {
    exact: HashMap<String, Price>,
    family: HashMap<String, Price>,
    checked_at: Option<DateTime<Utc>>,
}

impl Catalog {
    fn new(file: &CatalogFile) -> Self {
        let exact = file.models.iter().map(|(id, m)| (id.clone(), m.price())).collect();
        // Newest release of each family; among same-day releases the shortest
        // id, the plain model rather than a variant of it.
        let mut newest: HashMap<String, (&str, &str)> = HashMap::new();
        for (id, m) in &file.models {
            let key = family_of(id);
            let better = match newest.get(&key) {
                None => true,
                Some((date, best)) => (m.release_date.as_str(), std::cmp::Reverse(id.len())) > (*date, std::cmp::Reverse(best.len())),
            };
            if better {
                newest.insert(key, (m.release_date.as_str(), id.as_str()));
            }
        }
        let family = newest.into_iter().map(|(key, (_, id))| (key, file.models[id].price())).collect();
        let checked_at = file.checked_at.as_deref().or(file.fetched_at.as_deref()).and_then(parse_time);
        Catalog { exact, family, checked_at }
    }
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|t| t.with_timezone(&Utc))
}

static CATALOG: RwLock<Option<Arc<Catalog>>> = RwLock::new(None);
static LOADED: Mutex<bool> = Mutex::new(false);
static FETCHING: Mutex<bool> = Mutex::new(false);

fn catalog_path() -> Option<PathBuf> {
    std::env::var_os("HERDR_PLUGIN_STATE_DIR")
        .filter(|d| !d.is_empty())
        .map(|d| PathBuf::from(d).join("prices.json"))
}

fn read_file() -> CatalogFile {
    catalog_path()
        .and_then(|p| fs::read_to_string(p).ok())
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

fn write_file(file: &CatalogFile) {
    let Some(path) = catalog_path() else { return };
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(json) = serde_json::to_string(file) {
        let tmp = path.with_extension("json.tmp");
        if fs::write(&tmp, json).is_ok() {
            let _ = fs::rename(&tmp, &path);
        }
    }
}

fn install(file: &CatalogFile) {
    *CATALOG.write().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(Catalog::new(file)));
}

/// The catalog in memory, read from `prices.json` on first use.
fn catalog() -> Option<Arc<Catalog>> {
    {
        let mut loaded = LOADED.lock().unwrap_or_else(|e| e.into_inner());
        if !*loaded {
            *loaded = true;
            install(&read_file());
        }
    }
    CATALOG.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// The models.dev document cut down to the providers we price, in USD per 1M
/// tokens. Models without an input and output price are left out.
fn parse_catalog(doc: &Value) -> HashMap<String, CatalogModel> {
    let mut models = HashMap::new();
    for provider in PROVIDERS {
        let Some(list) = doc.get(provider).and_then(|p| p.get("models")).and_then(Value::as_object) else {
            continue;
        };
        for (id, model) in list {
            let Some(cost) = model.get("cost") else { continue };
            let field = |name: &str| cost.get(name).and_then(Value::as_f64).filter(|v| v.is_finite() && *v >= 0.0);
            let (Some(input), Some(output)) = (field("input"), field("output")) else { continue };
            let entry = CatalogModel {
                input,
                output,
                cache_read: field("cache_read"),
                cache_write: field("cache_write"),
                release_date: model.get("release_date").and_then(Value::as_str).unwrap_or("").to_string(),
            };
            models.entry(normalize(id)).or_insert(entry);
        }
    }
    models
}

fn fetch() -> Option<HashMap<String, CatalogModel>> {
    let agent: ureq::Agent = ureq::Agent::config_builder().timeout_global(Some(FETCH_TIMEOUT)).build().into();
    let mut response = agent.get(CATALOG_URL).call().ok()?;
    let body = response.body_mut().with_config().limit(64 * 1024 * 1024).read_to_string().ok()?;
    let models = parse_catalog(&serde_json::from_str(&body).ok()?);
    (!models.is_empty()).then_some(models)
}

fn due(checked_at: Option<DateTime<Utc>>, has_models: bool) -> bool {
    let Some(checked) = checked_at else { return true };
    let age = (Utc::now() - checked).to_std().unwrap_or(Duration::ZERO);
    age >= if has_models { REFRESH_AFTER } else { RETRY_AFTER }
}

/// Starts a download of the catalog when the saved one is a day old (an hour
/// after a failed try). Returns the thread, or None when nothing is due or a
/// download is already running. Without a state directory nothing is fetched.
pub fn refresh() -> Option<JoinHandle<()>> {
    catalog_path()?;
    let current = catalog();
    let has_models = current.as_ref().is_some_and(|c| !c.exact.is_empty());
    if !due(current.as_ref().and_then(|c| c.checked_at), has_models) {
        return None;
    }
    {
        let mut fetching = FETCHING.lock().unwrap_or_else(|e| e.into_inner());
        if *fetching {
            return None;
        }
        *fetching = true;
    }
    Some(std::thread::spawn(|| {
        let mut file = read_file();
        let now = Utc::now().to_rfc3339();
        file.checked_at = Some(now.clone());
        if let Some(models) = fetch() {
            file.models = models;
            file.fetched_at = Some(now);
        }
        write_file(&file);
        install(&file);
        *FETCHING.lock().unwrap_or_else(|e| e.into_inner()) = false;
    }))
}

// ---------------------------------------------------------------------------
// lookup
// ---------------------------------------------------------------------------

fn lookup(model: &str, catalog: Option<&Catalog>) -> Option<Price> {
    if model.is_empty() {
        return None;
    }
    let m = normalize(model);
    let builtin = |table: &[(&str, Price)], key: &str| table.iter().find(|(name, _)| *name == key).map(|(_, p)| *p);
    catalog
        .and_then(|c| c.exact.get(&m).copied())
        .or_else(|| builtin(BUILTIN_EXACT, &m))
        .or_else(|| catalog.and_then(|c| c.family.get(&family_of(&m)).copied()))
        .or_else(|| BUILTIN_FAMILY.iter().find(|(prefix, _)| m.starts_with(prefix)).map(|(_, p)| *p))
}

pub fn price_for(model: &str) -> Option<Price> {
    lookup(model, catalog().as_deref())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Catalog {
        let doc: Value = serde_json::from_str(
            r#"{
              "anthropic": {"models": {
                "claude-opus-5-5": {"cost": {"input": 4, "output": 20, "cache_read": 0.2, "cache_write": 5}, "release_date": "2026-09-22"},
                "claude-opus-5":   {"cost": {"input": 5, "output": 25}, "release_date": "2026-03-01"},
                "claude-opus-4-1-20250805": {"cost": {"input": 15, "output": 75}, "release_date": "2025-08-05"},
                "claude-nocost":   {"release_date": "2026-01-01"}
              }},
              "openai": {"models": {
                "gpt-6.2-sol":  {"cost": {"input": 4, "output": 20}, "release_date": "2026-11-17"},
                "gpt-6.1-sol":  {"cost": {"input": 3, "output": 18}, "release_date": "2026-10-01"},
                "gpt-6.2":      {"cost": {"input": 3, "output": 15}, "release_date": "2026-11-17"},
                "gpt-6.2-x":    {"cost": {"input": 9, "output": 99}, "release_date": "2026-11-17"}
              }},
              "mistral": {"models": {"mistral-large": {"cost": {"input": 1, "output": 1}}}}
            }"#,
        )
        .unwrap();
        let file = CatalogFile { models: parse_catalog(&doc), ..Default::default() };
        assert!(!file.models.contains_key("claude-nocost"));
        assert!(!file.models.contains_key("mistral-large"));
        Catalog::new(&file)
    }

    #[test]
    fn families() {
        assert_eq!(family_of("claude-opus-5-5"), "claude-opus");
        assert_eq!(family_of("gpt-6.2-sol"), "gpt-sol");
        assert_eq!(family_of("o4-mini"), "o-mini");
        assert_eq!(family_of("gemini-3.8-flash"), "gemini-flash");
    }

    #[test]
    fn catalog_lookup() {
        let c = sample();
        // exact, with provider path and date stamp taken off
        assert_eq!(lookup("anthropic/claude-opus-4-1-20250805", Some(&c)).unwrap().0, 15.0);
        assert_eq!(lookup("claude-opus-5-5", Some(&c)).unwrap().2, Some(0.2));
        // an unreleased version gets the newest of its family
        assert_eq!(lookup("claude-opus-9", Some(&c)).unwrap().0, 4.0);
        assert_eq!(lookup("gpt-6.3-sol", Some(&c)).unwrap().0, 4.0);
        // same-day releases: the plain id over a variant
        assert_eq!(lookup("gpt-7", Some(&c)).unwrap().0, 3.0);
        // the built-in table still answers what the catalog lacks
        assert_eq!(lookup("claude-sonnet-4", Some(&c)).unwrap().0, 3.0);
        assert!(lookup("llama", Some(&c)).is_none());
    }

    #[test]
    fn builtin_only() {
        assert_eq!(lookup("anthropic/claude-opus-4-1-20250805", None).unwrap().0, 15.0);
        assert_eq!(lookup("claude-opus-9", None).unwrap().0, 4.0);
        assert!(lookup("llama", None).is_none());
    }

    #[test]
    fn cost_with_cache_writes() {
        let (cost, known) = estimate_cost("claude-sonnet-4", 1_000_000, 0, 1_000_000, 0, 500_000);
        assert!(known);
        assert!((cost - (3.0 + 0.5 * 3.75 + 0.5 * 6.0)).abs() < 1e-9);
    }

    #[test]
    fn refresh_schedule() {
        let hours = |h: i64| Some(Utc::now() - chrono::Duration::hours(h));
        assert!(due(None, false));
        assert!(!due(hours(2), true));
        assert!(due(hours(25), true));
        assert!(due(hours(2), false));
        assert!(!due(Some(Utc::now()), false));
    }
}
