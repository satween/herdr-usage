//! Turns agent reports into the pane's lines: per-agent calendars, rate-limit
//! bars, token windows and the model split.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Local, NaiveDate, Utc};

use crate::agents::{AgentReport, DailyStat, ModelStat, Share, SubWindow, WindowStat};

type Rgb = (u8, u8, u8);

pub const BAR_MIN: i64 = 8;
pub const BAR_MAX: i64 = 28;

// ---------------------------------------------------------------------------
// style
// ---------------------------------------------------------------------------

pub struct Style {
    pub enabled: bool,
    fill: &'static str,
    empty: &'static str,
    block: &'static str,
    block_open: &'static str,
    truecolor: bool,
}

/// Whether the locale allows UTF-8 output. Like Python's stdout encoding: an
/// unset or C/POSIX locale is coerced to UTF-8, only an explicit other charset
/// falls back to ASCII glyphs.
fn utf8_locale() -> bool {
    for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            if value.is_empty() {
                continue;
            }
            let v = value.to_lowercase().replace('-', "");
            return v.contains("utf8") || v == "c" || v == "posix" || !v.contains('.');
        }
    }
    true
}

impl Style {
    pub fn new(enabled: bool) -> Self {
        let utf8 = utf8_locale();
        // Heatmap tiles: filled = a day that happened, hollow = a day that has
        // not. Same ASCII fallback as the bars for terminals without UTF-8.
        let (fill, empty) = if utf8 { ("\u{2588}", "\u{2591}") } else { ("#", "-") };
        let (block, block_open) = if utf8 { ("\u{25a0}", "\u{25a1}") } else { ("#", ".") };
        // Alpha is a 24-bit trick: a translucent tile is the agent's colour mixed
        // towards the background, which wants more than 256 rungs to look smooth.
        // Herdr exports COLORTERM=truecolor to panes, so that is the normal path;
        // without it every step snaps to the nearest xterm-256 rung.
        let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
        let truecolor = enabled && (colorterm == "truecolor" || colorterm == "24bit");
        Style { enabled, fill, empty, block, block_open, truecolor }
    }

    pub fn paint(&self, text: &str, code: &str) -> String {
        if self.enabled && !text.is_empty() {
            format!("\x1b[{code}m{text}\x1b[0m")
        } else {
            text.to_string()
        }
    }

    /// Colour for glyphs the terminal never adjusts (block and box glyphs).
    /// Without 24-bit colour the tile falls back to its nearest xterm-256 rung,
    /// so the hue stays right and only the opacity steps collapse.
    fn paint_rgb(&self, text: &str, rgb: Rgb) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        if self.truecolor {
            return format!("\x1b[38;2;{};{};{}m{text}\x1b[0m", rgb.0, rgb.1, rgb.2);
        }
        self.paint(text, &format!("38;5;{}", nearest_xterm(rgb)))
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint(text, "1")
    }

    /// Bold text in an agent's own accent: the same colour its calendar is
    /// drawn in, so the title says which hue the grid below belongs to.
    fn accent(&self, text: &str, rgb: Rgb) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        let code = if self.truecolor {
            format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2)
        } else {
            format!("38;5;{}", nearest_xterm(rgb))
        };
        format!("\x1b[1;{code}m{text}\x1b[0m")
    }

    /// A key name in the hints: bold bright white, so the keys stand out from
    /// the dimmed words that say what they do.
    fn key(&self, text: &str) -> String {
        self.paint(text, "1;97")
    }

    pub fn dim(&self, text: &str) -> String {
        self.paint(text, "2")
    }

    fn by_percent(&self, text: &str, percent: f64) -> String {
        let code = if percent >= 80.0 {
            "31"
        } else if percent >= 50.0 {
            "33"
        } else {
            "32"
        };
        self.paint(text, code)
    }

    fn tile(&self, rgb: Rgb) -> String {
        self.paint_rgb(self.block, rgb)
    }
}

/// Display width of a styled line, with the colour codes left out.
pub fn visible(text: &str) -> usize {
    let mut n = 0;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            // \x1b[ ... m with only digits and ';' between
            let rest: String = chars.clone().skip(1).take_while(|c| c.is_ascii_digit() || *c == ';').collect();
            let after = chars.clone().nth(1 + rest.chars().count());
            if after == Some('m') {
                for _ in 0..rest.chars().count() + 2 {
                    chars.next();
                }
                continue;
            }
        }
        n += 1;
    }
    n
}

fn chars_len(s: &str) -> usize {
    s.chars().count()
}

fn take_chars(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn rstrip(s: String) -> String {
    s.trim_end_matches(' ').to_string()
}

/// Python's round(): half to even.
fn round_even(x: f64) -> i64 {
    x.round_ties_even() as i64
}

// ---------------------------------------------------------------------------
// numbers
// ---------------------------------------------------------------------------

pub fn fmt_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn fmt_cost(value: f64) -> String {
    if value >= 1000.0 {
        format!("${:.1}k", value / 1000.0)
    } else if value >= 100.0 {
        format!("${value:.0}")
    } else {
        format!("${value:.1}")
    }
}

fn fmt_reset(resets_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> String {
    let Some(resets_at) = resets_at else { return String::new() };
    let seconds = (resets_at - now).num_milliseconds() / 1000; // truncates like int()
    if seconds <= 0 {
        return "resets now".to_string();
    }
    if seconds < 86400 {
        let minutes = seconds / 60;
        let (hours, minutes) = (minutes / 60, minutes % 60);
        return if hours > 0 {
            format!("resets in {hours}h {minutes:02}m")
        } else {
            format!("resets in {minutes}m")
        };
    }
    let at = (resets_at + chrono::Duration::seconds(30)).with_timezone(&Local);
    format!("resets {}", at.format("%a %H:%M"))
}

fn render_sub_window(window: &SubWindow, width: usize, style: &Style, now: DateTime<Utc>) -> String {
    let percent = window.percent.clamp(0.0, 100.0);
    let bar_width = (width as i64 - 40).clamp(BAR_MIN, BAR_MAX);
    let filled = round_even(bar_width as f64 * percent / 100.0).clamp(0, bar_width);
    let bar = style.by_percent(&style.fill.repeat(filled as usize), percent)
        + &style.dim(&style.empty.repeat((bar_width - filled) as usize));
    let tail = if window.detail.is_empty() { fmt_reset(window.resets_at, now) } else { window.detail.clone() };
    let pct = style.by_percent(&format!("{percent:3.0}%"), percent);
    rstrip(format!("  {:<12} {bar} {pct}  {}", window.label, style.dim(&tail)))
}

/// Money for one window, or "" when there is nothing honest to say: plans that
/// meter by subscription and agents that run local models have no per-token
/// price, and a window whose models are all unpriced has no total. `~` marks a
/// window that mixes priced with unpriced models, so the figure covers only part
/// of its tokens. That the figures are estimates rather than bills is true of
/// every number here, so no number carries a marker for it.
fn cost_label(report: &AgentReport, window: &WindowStat) -> String {
    if matches!(report.mode.as_str(), "subscription" | "local") || window.tokens() == 0 {
        return String::new();
    }
    if !matches!(report.mode.as_str(), "api key" | "metered" | TOTAL_MODE) {
        return String::new();
    }
    let unpriced = window.unpriced();
    if unpriced == 0 {
        return fmt_cost(window.cost());
    }
    if unpriced < window.tokens() {
        return format!("~{}", fmt_cost(window.cost()));
    }
    String::new()
}

/// One row of the window table. Columns are padded on the plain text and only
/// then styled, so escape codes never count as width. Numbers align right, so
/// the units and the decimal points stack.
fn render_window_line(window: &WindowStat, report: &AgentReport, style: &Style) -> String {
    let tokens = style.bold(&format!("{:>7}", fmt_tokens(window.tokens()))) + " tok";
    let cost = cost_label(report, window);
    rstrip(format!("  {:<6}{tokens}  {cost:>7}", window.label))
}

/// A model's display name: a path keeps only its last part. No length cap here
/// -- the legend cuts a name only when the column has no room for it.
fn short_model(name: &str) -> String {
    match name.rsplit('/').next() {
        Some(last) if !last.is_empty() => last.to_string(),
        _ => name.to_string(),
    }
}

// ---------------------------------------------------------------------------
// colour
// ---------------------------------------------------------------------------

/// Model shares get their own hues rather than the agent's: the bar compares
/// models with each other, not agents with each other.
const MODEL_PALETTE: [Rgb; 5] = [
    (0x61, 0xAF, 0xEF), // sky
    (0xF2, 0x8D, 0x6B), // coral
    (0x8F, 0xD9, 0x7A), // leaf
    (0xC7, 0x92, 0xEB), // orchid
    (0xE8, 0xC8, 0x5A), // sand
];
const MODEL_BAR_MAX: i64 = 38; // only when the column width is unknown
/// The bar runs to the right edge of the pane but stops this many columns short
/// of it -- the same margin the calendar keeps from the left edge -- and the
/// legend under it is held to the bar's width.
const RIGHT_MARGIN: usize = 2;
/// The bar is drawn a little translucent, so a solid block does not outshout
/// the figures it breaks down. The legend tiles use the same colour.
const MODEL_ALPHA: f64 = 0.8;

/// The all-agents block is drawn in white and its shades: lightest for the
/// biggest model, darker down the list. Six rungs: five models plus `other`.
const TOTAL_MODEL_PALETTE: [Rgb; 6] = [
    (0xFF, 0xFF, 0xFF),
    (0xD8, 0xD8, 0xD8),
    (0xB4, 0xB4, 0xB4),
    (0x92, 0x92, 0x92),
    (0x74, 0x74, 0x74),
    (0x5A, 0x5A, 0x5A),
];

/// Theme graphite: the pane's own background. Herdr never adjusts block-glyph
/// colours, so what a step looks like is exactly the colour computed here.
const TERMINAL_BG: Rgb = (0x13, 0x16, 0x19);
/// Theme fg, for agents with no accent of their own (and the all-agents total).
const HEAT_DEFAULT: Rgb = (0xE6, 0xE7, 0xE9);
pub const TOTAL_ID: &str = "total";
pub const TOTAL_MODE: &str = "all agents";
const HEAT_STEPS: usize = 4;
/// Alpha cannot be shared between hues: 10 % of Codex green is plainly green
/// while 10 % of Claude clay is near black. So the steps are spaced in contrast
/// against the pane, solved per accent.
const HEAT_MIN_CONTRAST: f64 = 2.0;
const HEAT_MAX_CONTRAST: f64 = 7.0;
/// #585858 on a hollow tile: 2.6:1, the shape says "not yet".
const FUTURE_COLOR: u8 = 240;

/// One accent per agent: the header is painted in it and the calendar is drawn
/// from it, so the hue of a grid is readable from the title above it.
fn agent_accent(agent_id: &str) -> Option<Rgb> {
    match agent_id {
        "claude" => Some((215, 135, 95)),  // clay
        "codex" => Some((95, 215, 95)),    // green
        "pi" => Some((175, 135, 255)),     // violet
        "opencode" => Some((95, 175, 215)), // steel blue
        TOTAL_ID => Some(HEAT_DEFAULT),
        _ => None,
    }
}

/// What `fg` looks like drawn at `alpha` over the pane.
fn mix(fg: Rgb, alpha: f64) -> Rgb {
    let ch = |f: u8, b: u8| round_even(b as f64 + (f as f64 - b as f64) * alpha).clamp(0, 255) as u8;
    (ch(fg.0, TERMINAL_BG.0), ch(fg.1, TERMINAL_BG.1), ch(fg.2, TERMINAL_BG.2))
}

/// WCAG relative luminance, which is what contrast ratios are made of.
fn luminance(rgb: Rgb) -> f64 {
    let channel = |v: u8| {
        let c = v as f64 / 255.0;
        if c <= 0.03928 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * channel(rgb.0) + 0.7152 * channel(rgb.1) + 0.0722 * channel(rgb.2)
}

fn contrast(fg: Rgb) -> f64 {
    let (mut a, mut b) = (luminance(fg), luminance(TERMINAL_BG));
    if a < b {
        std::mem::swap(&mut a, &mut b);
    }
    (a + 0.05) / (b + 0.05)
}

/// The agent's accent at HEAT_STEPS strengths, evenly spaced in contrast: the
/// bottom clears the background enough to read as a colour, the top is capped.
fn heat_levels(agent_id: &str) -> Vec<Rgb> {
    let rgb = agent_accent(agent_id).unwrap_or(HEAT_DEFAULT);
    let top = contrast(rgb).clamp(HEAT_MIN_CONTRAST, HEAT_MAX_CONTRAST);
    (0..HEAT_STEPS)
        .map(|step| {
            let target =
                HEAT_MIN_CONTRAST * (top / HEAT_MIN_CONTRAST).powf(step as f64 / (HEAT_STEPS - 1) as f64);
            let (mut low, mut high) = (0.0, 1.0);
            for _ in 0..24 {
                // contrast rises with alpha, so bisect for it
                let middle = (low + high) / 2.0;
                if contrast(mix(rgb, middle)) < target {
                    low = middle;
                } else {
                    high = middle;
                }
            }
            mix(rgb, (low + high) / 2.0)
        })
        .collect()
}

/// Snaps a colour onto the 256-colour cube for terminals without 24-bit colour.
fn nearest_xterm(rgb: Rgb) -> u32 {
    const CUBE: [i64; 6] = [0, 95, 135, 175, 215, 255];
    let (r0, g0, b0) = (rgb.0 as i64, rgb.1 as i64, rgb.2 as i64);
    let mut best = i64::MAX;
    let mut code = 16;
    for (i, r) in CUBE.iter().enumerate() {
        for (j, g) in CUBE.iter().enumerate() {
            for (k, b) in CUBE.iter().enumerate() {
                let d = (r - r0).pow(2) + (g - g0).pow(2) + (b - b0).pow(2);
                if d < best {
                    best = d;
                    code = 16 + 36 * i as u32 + 6 * j as u32 + k as u32;
                }
            }
        }
    }
    for n in 0..24 {
        let v = 8 + 10 * n as i64;
        let d = 3 * (v - r0).pow(2);
        if d < best {
            code = 232 + n;
            best = d;
        }
    }
    code
}

fn model_rgb(i: usize, palette: &[Rgb]) -> Rgb {
    mix(palette[i % palette.len()], MODEL_ALPHA)
}

// ---------------------------------------------------------------------------
// model split
// ---------------------------------------------------------------------------

/// Index of the first largest size.
fn first_max(sizes: &[i64]) -> usize {
    let mut best = 0;
    for (i, s) in sizes.iter().enumerate() {
        if *s > sizes[best] {
            best = i;
        }
    }
    best
}

/// The buckets as one 100 %-stacked bar, from the column the window rows start
/// in to RIGHT_MARGIN short of the pane edge. A bucket keeps a cell even at 1 %.
/// Without colour every segment is the same glyph, so no bar is drawn.
fn models_bar(buckets: &[Share], style: &Style, width: usize, palette: &[Rgb]) -> String {
    if !style.enabled || buckets.is_empty() {
        return String::new();
    }
    let prefix = "  ";
    let cells = if width > 0 { width as i64 - prefix.len() as i64 - RIGHT_MARGIN as i64 } else { MODEL_BAR_MAX };
    if cells < 6 {
        return String::new();
    }
    let mut sizes: Vec<i64> =
        buckets.iter().map(|(_, _, percent)| round_even(*percent as f64 / 100.0 * cells as f64).max(1)).collect();
    while sizes.iter().sum::<i64>() > cells {
        let i = first_max(&sizes);
        sizes[i] -= 1;
    }
    while sizes.iter().sum::<i64>() < cells {
        let i = first_max(&sizes);
        sizes[i] += 1;
    }
    // Drawn in the legend's own tile, not a full block, so the bar sits at the
    // same height as the legend rows and the gaps match.
    let bar: String = sizes
        .iter()
        .enumerate()
        .map(|(i, size)| style.paint_rgb(&style.block.repeat((*size).max(0) as usize), model_rgb(i, palette)))
        .collect();
    format!("{prefix}{bar}")
}

/// (percent, tokens) column widths shared by every agent's model legend, so
/// the columns line up on the whole page.
fn legend_widths(windows: &[&WindowStat]) -> (usize, usize) {
    let buckets: Vec<Share> =
        windows.iter().flat_map(|w| w.model_shares(5)).filter(|b| b.2 > 0).collect();
    if buckets.is_empty() {
        return (0, 0);
    }
    (
        buckets.iter().map(|b| format!("{}%", b.2).len()).max().unwrap_or(0),
        buckets.iter().map(|b| fmt_tokens(b.1).len()).max().unwrap_or(0),
    )
}

/// One line per bucket under the bar: a tile in the colour of that bucket's
/// segment, then its share, its tokens, and the model last. A narrow column
/// sheds the token count first and only then the tail of a name.
fn model_legend(buckets: &[Share], style: &Style, width: usize, widths: (usize, usize), palette: &[Rgb]) -> Vec<String> {
    let names: Vec<String> = buckets.iter().map(|b| short_model(&b.0)).collect();
    let widest = names.iter().map(|n| chars_len(n)).max().unwrap_or(0);
    let pct_w = buckets.iter().map(|b| format!("{}%", b.2).len()).chain([widths.0]).max().unwrap_or(0);
    let tok_w = buckets.iter().map(|b| fmt_tokens(b.1).len()).chain([widths.1]).max().unwrap_or(0);
    let lead = 2 + 1 + 1 + pct_w + 1; // "  ■ " + percent + space
    let show_tokens = width == 0 || lead + tok_w + 1 + widest <= width;
    let name_w: i64 = if width == 0 {
        widest as i64
    } else {
        (widest as i64).min(width as i64 - lead as i64 - if show_tokens { tok_w as i64 + 1 } else { 0 })
    };
    if name_w < 4 {
        return Vec::new();
    }
    let name_w = name_w as usize;
    buckets
        .iter()
        .zip(&names)
        .enumerate()
        .map(|(i, ((_, tokens, percent), name))| {
            let tile = style.tile(model_rgb(i, palette));
            let label =
                if chars_len(name) <= name_w { name.clone() } else { take_chars(name, name_w - 1) + "\u{2026}" };
            // The share starts one space after the tile, so it pads on the right.
            let mut row = format!("  {tile} {:<pct_w$} ", format!("{percent}%"));
            if show_tokens {
                row += &style.dim(&format!("{:>tok_w$}", fmt_tokens(*tokens)));
                row += " ";
            }
            row + &label
        })
        .collect()
}

/// How the 30d window splits across models: a caption naming the window, a
/// bar, then the legend. The list narrows from five models down to whatever the
/// column can name; models dropped at the bottom roll into `other`. A bucket
/// whose share rounded to 0 % is left out.
fn render_model_shares(window: &WindowStat, style: &Style, width: usize, widths: (usize, usize), palette: &[Rgb]) -> Vec<String> {
    for limit in [5, 4, 3, 2, 1] {
        let buckets: Vec<Share> = window.model_shares(limit).into_iter().filter(|b| b.2 > 0).collect();
        if buckets.is_empty() {
            return Vec::new();
        }
        let legend_w = if width > 0 { width.saturating_sub(RIGHT_MARGIN) } else { 0 };
        let legend = model_legend(&buckets, style, legend_w, widths, palette);
        if legend.is_empty() {
            continue;
        }
        let bar = models_bar(&buckets, style, width, palette);
        // A caption, because the 30d window is not the month the calendar
        // beside it shows: without it the split reads as the calendar's.
        let mut out = vec![String::new(), style.dim(&format!("  {} models usage:", window.label))];
        if !bar.is_empty() {
            out.push(bar);
        }
        out.extend(legend);
        return out;
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// calendar
// ---------------------------------------------------------------------------

fn today_local() -> String {
    Local::now().date_naive().format("%Y-%m-%d").to_string()
}

/// Calendar-style heatmap of the current month: one row per week, one column
/// per weekday, one filled square per day in the agent's accent with opacity
/// carrying the amount (sqrt-scaled to the month's busiest day). Only the grid
/// comes back -- the calendar carries no summary of its own.
fn render_daily(report: &AgentReport, style: &Style) -> Vec<String> {
    let daily = &report.daily;
    if daily.is_empty() {
        return Vec::new();
    }
    let today = today_local();
    let tokens: BTreeMap<&str, i64> = daily.iter().map(|d| (d.date.as_str(), d.tokens)).collect();
    let vmax = daily.iter().filter(|d| d.date <= today).map(|d| d.tokens).max().unwrap_or(0);
    let levels = heat_levels(&report.agent_id);
    let tile_for = |t: i64| {
        let n = if vmax <= 0 { 0.0 } else { (t as f64 / vmax as f64).sqrt() };
        let idx = ((n * levels.len() as f64) as usize).min(levels.len() - 1);
        style.tile(levels[idx])
    };

    // Grid placement: pad with the previous month's trailing days up to Monday.
    let first = NaiveDate::parse_from_str(&daily[0].date, "%Y-%m-%d").ok();
    let pad = first.map(|f| f.weekday().num_days_from_monday() as usize).unwrap_or(0);
    let ncols = 7.min(pad + daily.len());
    let mut grid: Vec<Vec<&str>> = vec![vec![""; ncols]; 7.max((pad + daily.len()).div_ceil(7))];
    for (i, d) in daily.iter().enumerate() {
        let (row, col) = ((pad + i) / 7, (pad + i) % 7);
        grid[row][col] = &d.date;
    }

    // "nov 2026": a bare month goes stale across a year.
    let month = first.map(|f| f.format("%b %Y").to_string().to_lowercase()).unwrap_or_default();
    // Centred over the block it names: cells are two characters and the
    // trailing space is stripped, hence ncols * 2 - 1.
    let grid_w = (ncols * 2).saturating_sub(1);
    let mut lines =
        vec![format!("  {}{}", " ".repeat(grid_w.saturating_sub(chars_len(&month)) / 2), style.dim(&month))];
    let mut body: Vec<String> = grid
        .iter()
        .map(|row| {
            let cells: String = row
                .iter()
                .map(|date| {
                    if date.is_empty() {
                        "  ".to_string()
                    } else if *date > today.as_str() {
                        style.paint(style.block_open, &format!("38;5;{FUTURE_COLOR}")) + " "
                    } else {
                        tile_for(tokens.get(date).copied().unwrap_or(0)) + " "
                    }
                })
                .collect();
            rstrip(cells)
        })
        .collect();
    while body.last().is_some_and(String::is_empty) {
        body.pop(); // drop trailing fully empty week rows
    }
    lines.extend(body.into_iter().map(|l| format!("  {l}")));
    lines
}

/// Colour key for one calendar, in that agent's accent: the steps between
/// "less" and "more", run together so the key is no wider than the grid, and
/// centred under it.
fn render_legend(style: &Style, agent_id: &str, cal_w: usize) -> Vec<String> {
    let steps: String = heat_levels(agent_id).into_iter().map(|rgb| style.tile(rgb)).collect();
    let key_w = "less ".len() + HEAT_STEPS + " more".len();
    let indent = if cal_w > 0 { (2 + (cal_w as i64 - 2 - key_w as i64).div_euclid(2)).max(0) as usize } else { 2 };
    vec![format!("{}{}{steps}{}", " ".repeat(indent), style.dim("less "), style.dim(" more"))]
}

/// Calendar on the left, stats on the right, or None if it will not fit. The
/// grid leads because its width is fixed, so every agent's stats line up in the
/// same column.
fn side_by_side(grid: &[String], stats: &[String], width: usize, gutter: usize, cal_w: usize) -> Option<Vec<String>> {
    if grid.is_empty() || stats.is_empty() {
        return None;
    }
    let cal_w = if cal_w > 0 { cal_w } else { grid.iter().map(|l| visible(l)).max().unwrap_or(0) };
    if cal_w + gutter + stats.iter().map(|l| visible(l)).max().unwrap_or(0) > width {
        return None;
    }
    let rows = (0..grid.len().max(stats.len()))
        .map(|i| {
            let left = grid.get(i).map(String::as_str).unwrap_or("");
            let right = stats.get(i).map(String::as_str).unwrap_or("");
            if right.is_empty() {
                left.to_string()
            } else {
                let pad = (cal_w + gutter).saturating_sub(visible(left)).max(1);
                format!("{left}{}{right}", " ".repeat(pad))
            }
        })
        .collect();
    Some(rows)
}

fn render_report(report: &AgentReport, width: usize, style: &Style, now: DateTime<Utc>, widths: (usize, usize)) -> Vec<String> {
    let mut header = match agent_accent(&report.agent_id) {
        Some(rgb) => style.accent(&report.title, rgb),
        None => style.bold(&report.title),
    };
    let palette: &[Rgb] = if report.agent_id == TOTAL_ID { &TOTAL_MODEL_PALETTE } else { &MODEL_PALETTE };
    if report.mode != "unknown" && report.mode != TOTAL_MODE {
        header += &style.dim(&format!("  [{}]", report.mode));
    }
    if !report.badge.is_empty() {
        header += &style.dim(&format!("  [{}]", report.badge));
    }
    // A blank line under the header: the block starts with a coloured caption,
    // which otherwise reads as a second header.
    let mut lines = vec![header, String::new()];
    let grid = render_daily(report, style);
    let cal_w = grid.iter().map(|l| visible(l)).max().unwrap_or(0);
    let gutter = 3;
    // Stats take whatever is left of the calendar; below a readable column the
    // two stack instead.
    let room = width as i64 - cal_w as i64 - gutter as i64;
    let stats_w = if !grid.is_empty() && room >= 24 { room as usize } else { width };

    let build_stats = |column_w: usize| {
        let mut stats = Vec::new();
        if !report.note.is_empty() {
            stats.push(style.dim(&format!("  {}", report.note)));
        }
        for sub in &report.sub_windows {
            stats.push(render_sub_window(sub, column_w, style, now));
        }
        if report.mode == "subscription"
            && report.sub_windows.is_empty()
            && report.note.is_empty()
            && report.badge.is_empty()
        {
            stats.push(style.dim("  no rate-limit window reported by the plan"));
        }
        for window in &report.windows {
            if window.tokens() != 0 {
                stats.push(render_window_line(window, report, style));
            }
        }
        if let Some(last) = report.windows.last() {
            stats.extend(render_model_shares(last, style, column_w, widths, palette));
        }
        stats
    };

    // The colour key belongs to the calendar, so it rides in the calendar's
    // column, one blank row under the grid.
    let left: Vec<String> = if grid.is_empty() {
        Vec::new()
    } else {
        let mut l = grid.clone();
        l.push(String::new());
        l.extend(render_legend(style, &report.agent_id, cal_w));
        l
    };
    let mut stats = build_stats(stats_w);
    let rows = side_by_side(&left, &stats, width, gutter, cal_w);
    if rows.is_none() && stats_w != width {
        // The column squeezed a line past the pane; redo it full width and stack.
        stats = build_stats(width);
    }
    match rows {
        Some(rows) => lines.extend(rows),
        None => {
            let both = !left.is_empty() && !stats.is_empty();
            lines.extend(left);
            if both {
                lines.push(String::new());
            }
            lines.extend(stats);
        }
    }
    lines
}

// ---------------------------------------------------------------------------
// page
// ---------------------------------------------------------------------------

/// What the page is ordered by: tokens over the last 30 days.
fn tokens_30d(report: &AgentReport) -> i64 {
    report.windows.last().map(WindowStat::tokens).unwrap_or(0)
}

const ACTIVE_GRACE_DAYS: u32 = 7;

/// Whether an agent earns a block on the page: tokens in the last 30 days, and
/// -- past the first week of the month -- tokens this month too, because its
/// calendar would otherwise be an empty grid.
fn is_active(report: &AgentReport, now: DateTime<Utc>) -> bool {
    if tokens_30d(report) <= 0 {
        return false;
    }
    if now.with_timezone(&Local).day() <= ACTIVE_GRACE_DAYS {
        return true;
    }
    report.daily.iter().map(|d| d.tokens).sum::<i64>() > 0
}

/// Every agent's tokens as one report: windows merged model by model, the
/// calendar summed day by day. Money is only counted where the agent itself
/// prices it; tokens of subscription and local agents go in as unpriced.
fn combine(reports: &[&AgentReport]) -> AgentReport {
    let mut total = AgentReport::new(TOTAL_ID, "All agents");
    total.mode = TOTAL_MODE.to_string();
    for report in reports {
        for window in &report.windows {
            if !total.windows.iter().any(|w| w.label == window.label) {
                total.windows.push(WindowStat::new(&window.label));
            }
        }
    }
    let mut per_day: BTreeMap<String, i64> = BTreeMap::new();
    for report in reports {
        let priced = matches!(report.mode.as_str(), "api key" | "metered");
        for window in &report.windows {
            let merged = total.windows.iter_mut().find(|w| w.label == window.label).expect("label added above");
            for (name, stat) in &window.models {
                let into = merged.models.entry(name.clone()).or_insert_with(|| ModelStat::new(name));
                into.inp += stat.inp;
                into.cache_read += stat.cache_read;
                into.cache_write += stat.cache_write;
                into.output += stat.output;
                if priced {
                    into.cost += stat.cost;
                    into.unpriced += stat.unpriced;
                } else {
                    into.unpriced += stat.total();
                }
            }
        }
        for day in &report.daily {
            *per_day.entry(day.date.clone()).or_insert(0) += day.tokens;
        }
    }
    total.daily = per_day.into_iter().map(|(date, tokens)| DailyStat { date, tokens }).collect();
    total
}

/// The blocks on the page, top to bottom: the all-agents total when more than
/// one agent has tokens, then the agents, busiest over 30 days first.
fn page_reports(reports: &[AgentReport], now: DateTime<Utc>) -> Vec<AgentReport> {
    // A collector that failed stays, so a broken log reader is never mistaken
    // for an idle agent. When the filter would leave nothing, show every agent:
    // empty calendars still say which agents are set up.
    let mut shown: Vec<&AgentReport> = if reports.iter().any(|r| is_active(r, now)) {
        reports.iter().filter(|r| is_active(r, now) || r.note.starts_with("error")).collect()
    } else {
        reports.iter().collect()
    };
    shown.sort_by_key(|r| std::cmp::Reverse(tokens_30d(r))); // stable: ties keep config order
    let mut out = Vec::new();
    // With a single agent the total would only repeat its block.
    if shown.iter().filter(|r| tokens_30d(r) > 0).count() > 1 {
        out.push(combine(&shown));
    }
    out.extend(shown.into_iter().cloned());
    out
}

/// The agent blocks, one under the other, with no title above them: the pane
/// is titled by herdr already.
pub fn render(reports: &[AgentReport], width: usize, style: &Style, now: DateTime<Utc>) -> Vec<String> {
    let shown = page_reports(reports, now);
    let last_windows: Vec<&WindowStat> = shown.iter().filter_map(|r| r.windows.last()).collect();
    let widths = legend_widths(&last_windows);
    let mut lines = Vec::new();
    for report in &shown {
        if !lines.is_empty() {
            lines.push(String::new());
        }
        lines.extend(render_report(report, width, style, now, widths));
    }
    lines
}

/// The fine print for the row above the key hints: the key to the `~` mark,
/// only when some figure on screen carries it.
pub fn footnote(reports: &[AgentReport], width: usize, style: &Style) -> String {
    let shown = page_reports(reports, Utc::now());
    let marked = shown.iter().any(|r| r.windows.iter().any(|w| cost_label(r, w).starts_with('~')));
    let text = "  (~ = partly unpriced)";
    if marked && chars_len(text) <= width {
        style.dim(text)
    } else {
        String::new()
    }
}

/// The footer: keys bold white, what they do dimmed, cut to the pane width on
/// the plain text so a narrow pane never wraps it.
pub fn key_hints(remaining: u64, width: usize, style: &Style, scrollable: bool) -> String {
    let refresh = format!(" refresh ({remaining}s) \u{b7} ");
    let mut segments: Vec<(&str, bool)> = vec![("r", true), (&refresh, false)];
    if scrollable {
        segments.extend([("\u{2191}\u{2193}", true), (" scroll \u{b7} ", false)]);
    }
    segments.extend([("q", true), ("/", false), ("Esc", true), (" quit", false)]);
    let mut out = String::new();
    let mut room = width;
    for (text, is_key) in segments {
        let text = take_chars(text, room);
        if text.is_empty() {
            break;
        }
        room -= chars_len(&text);
        out += &if is_key { style.key(&text) } else { style.dim(&text) };
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_ignores_sgr() {
        assert_eq!(visible("\x1b[1;38;5;173mClaude\x1b[0m x"), 8);
        assert_eq!(visible("■ □"), 3);
    }

    #[test]
    fn xterm_snaps_accents_back() {
        assert_eq!(nearest_xterm((215, 135, 95)), 173);
        assert_eq!(nearest_xterm((95, 215, 95)), 77);
        assert_eq!(nearest_xterm((0x13, 0x16, 0x19)), 233);
    }

    #[test]
    fn bar_fills_column() {
        let style = Style { enabled: true, fill: "#", empty: "-", block: "#", block_open: ".", truecolor: false };
        let shares = vec![("a".to_string(), 1, 1), ("b".to_string(), 99, 99)];
        let bar = models_bar(&shares, &style, 40, &MODEL_PALETTE);
        assert_eq!(visible(&bar), 38);
    }
}
