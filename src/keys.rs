//! Keyboard and mouse input of the watch pane.
//!
//! Command bytes are matched one at a time, so a held or repeated key -- which
//! one read returns as a single chunk ("rr") -- still counts. Non-ASCII input
//! goes through `layout_keys` first, so a Russian-layout press counts as the
//! QWERTY key under it; unmapped letters stay inert.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Quit,
    Refresh,
    Up,
    Down,
    PageUp,
    PageDown,
    Top,
    Bottom,
    WheelUp,
    WheelDown,
}

pub const WHEEL_STEP: i64 = 3;
const WHEEL_UP: u32 = 64; // SGR mouse buttons, modifier bits masked off
const WHEEL_DOWN: u32 = 65;

/// q, Ctrl-C, Ctrl-Q quit. Enter and Ctrl-R are the layout-free ways to refresh:
/// their bytes are identical in every layout.
fn plain_key(byte: u8) -> Option<Action> {
    Some(match byte {
        b'q' | b'Q' | 0x03 | 0x11 => Action::Quit,
        b'r' | b'R' | 0x12 | 0x0d => Action::Refresh,
        // Scrolling as in less/vim: j/k a line, g/G to the ends, Space a page.
        b'j' | b'J' => Action::Down,
        b'k' | b'K' => Action::Up,
        b'g' => Action::Top,
        b'G' => Action::Bottom,
        b' ' => Action::PageDown,
        _ => return None,
    })
}

/// A Russian-layout press delivers the Cyrillic letter printed on the key, as
/// UTF-8. herdr never enables the kitty protocol's "report alternate keys", so
/// the translation happens here: only the command letters translate, by QWERTY
/// position; every other non-ASCII byte maps to nothing, so Russian text typed
/// into the pane can never trigger a command.
fn layout_key(ch: char) -> Option<u8> {
    Some(match ch {
        'й' => b'q',
        'к' => b'r',
        'Й' => b'Q',
        'К' => b'R',
        'о' => b'j',
        'л' => b'k',
        'О' => b'J',
        'Л' => b'K',
        'п' => b'g',
        'П' => b'G',
        _ => return None,
    })
}

/// The command letters a layout chunk stands for, ASCII passing through.
fn layout_keys(chunk: &[u8]) -> Vec<u8> {
    if chunk.iter().all(|b| *b <= 0x7f) {
        return chunk.to_vec();
    }
    match std::str::from_utf8(chunk) {
        Ok(text) => text.chars().filter_map(layout_key).collect(),
        Err(_) => Vec::new(), // a multi-byte letter split across two reads: no commands
    }
}

fn plain_keys(chunk: &[u8], out: &mut Vec<Action>) {
    out.extend(layout_keys(chunk).into_iter().filter_map(plain_key));
}

/// Arrows, PgUp/PgDn and Home/End, in both CSI (\x1b[A) and SS3 (\x1bOA,
/// application-cursor mode) spellings.
fn esc_action(seq: &[u8]) -> Option<Action> {
    Some(match seq {
        b"\x1b[A" | b"\x1bOA" => Action::Up,
        b"\x1b[B" | b"\x1bOB" => Action::Down,
        b"\x1b[5~" => Action::PageUp,
        b"\x1b[6~" => Action::PageDown,
        b"\x1b[H" | b"\x1bOH" | b"\x1b[1~" | b"\x1b[7~" => Action::Top,
        b"\x1b[F" | b"\x1bOF" | b"\x1b[4~" | b"\x1b[8~" => Action::Bottom,
        _ => return None,
    })
}

enum Seq {
    /// SGR mouse report: button, final byte M (press) or m (release).
    Mouse(u32, u8),
    Other,
}

fn digits(chunk: &[u8], mut i: usize) -> Option<(u32, usize)> {
    let start = i;
    while i < chunk.len() && chunk[i].is_ascii_digit() {
        i += 1;
    }
    if i == start {
        return None;
    }
    let n = std::str::from_utf8(&chunk[start..i]).ok()?.parse().unwrap_or(u32::MAX);
    Some((n, i))
}

/// The escape sequence starting at `start` (a \x1b byte), as its end and kind:
/// SGR mouse reports, CSI and SS3. Matched whole, so the letters inside one (the
/// "A" of an arrow, the "M" of a mouse report) never read as commands.
fn match_seq(chunk: &[u8], start: usize) -> Option<(usize, Seq)> {
    let next = *chunk.get(start + 1)?;
    if next == b'[' {
        // \x1b[<b;x;y[Mm]
        if chunk.get(start + 2) == Some(&b'<') {
            let mouse = (|| {
                let (button, i) = digits(chunk, start + 3)?;
                (chunk.get(i) == Some(&b';')).then_some(())?;
                let (_, i) = digits(chunk, i + 1)?;
                (chunk.get(i) == Some(&b';')).then_some(())?;
                let (_, i) = digits(chunk, i + 1)?;
                let fin = *chunk.get(i)?;
                matches!(fin, b'M' | b'm').then_some((i + 1, Seq::Mouse(button, fin)))
            })();
            if mouse.is_some() {
                return mouse;
            }
        }
        // \x1b[[0-9;?]*[A-Za-z~]
        let mut i = start + 2;
        while i < chunk.len() && (chunk[i].is_ascii_digit() || chunk[i] == b';' || chunk[i] == b'?') {
            i += 1;
        }
        let fin = *chunk.get(i)?;
        return (fin.is_ascii_alphabetic() || fin == b'~').then_some((i + 1, Seq::Other));
    }
    if next == b'O' {
        let fin = *chunk.get(start + 2)?;
        return fin.is_ascii_alphabetic().then_some((start + 3, Seq::Other));
    }
    None
}

/// The commands carried by one read, in the order their bytes arrived. A bare
/// Esc or Ctrl-C quits, but only as the whole chunk: arrows arrive as escape
/// sequences that start with \x1b and must not be quits.
pub fn scan_keys(chunk: &[u8]) -> Vec<Action> {
    if chunk == b"\x1b" || chunk == b"\x03" {
        return vec![Action::Quit];
    }
    let mut actions = Vec::new();
    let mut pos = 0;
    let mut i = 0;
    while i < chunk.len() {
        if chunk[i] != 0x1b {
            i += 1;
            continue;
        }
        let Some((end, seq)) = match_seq(chunk, i) else {
            i += 1;
            continue;
        };
        plain_keys(&chunk[pos..i], &mut actions);
        match seq {
            Seq::Mouse(button, b'M') => match button & !(4 | 8 | 16) {
                WHEEL_UP => actions.push(Action::WheelUp),
                WHEEL_DOWN => actions.push(Action::WheelDown),
                _ => {}
            },
            Seq::Mouse(..) => {}
            Seq::Other => actions.extend(esc_action(&chunk[i..end])),
        }
        pos = end;
        i = end;
    }
    let tail: Vec<u8> = chunk[pos..].iter().copied().filter(|b| *b != 0x1b).collect();
    plain_keys(&tail, &mut actions);
    actions
}

/// The new top line after a scroll action, kept inside the content.
pub fn scroll_to(offset: i64, action: Option<Action>, page: i64, total: i64) -> i64 {
    let step = (page - 1).max(1);
    let offset = match action {
        Some(Action::Top) => 0,
        Some(Action::Bottom) => total,
        Some(Action::Up) => offset - 1,
        Some(Action::Down) => offset + 1,
        Some(Action::WheelUp) => offset - WHEEL_STEP,
        Some(Action::WheelDown) => offset + WHEEL_STEP,
        Some(Action::PageUp) => offset - step,
        Some(Action::PageDown) => offset + step,
        _ => offset,
    };
    offset.min((total - page).max(0)).max(0)
}

#[cfg(test)]
mod tests {
    use super::Action::*;
    use super::*;

    #[test]
    fn keys() {
        assert_eq!(scan_keys(b"\x1b"), vec![Quit]);
        assert_eq!(scan_keys(b"rr"), vec![Refresh, Refresh]);
        assert_eq!(scan_keys(b"\x1b[A\x1bOB"), vec![Up, Down]);
        assert_eq!(scan_keys(b"\x1b[<64;10;5M\x1b[<65;10;5m"), vec![WheelUp]);
        assert_eq!(scan_keys("к".as_bytes()), vec![Refresh]);
        assert_eq!(scan_keys("кq".as_bytes()), vec![Refresh]); // mixed chunk: ASCII dropped
        assert_eq!(scan_keys(b"\x1b[5~j"), vec![PageUp, Down]);
        assert_eq!(scan_keys(b"\x1b[1;5A"), vec![]);
    }

    #[test]
    fn scroll_clamps() {
        assert_eq!(scroll_to(0, Some(Up), 10, 30), 0);
        assert_eq!(scroll_to(0, Some(Bottom), 10, 30), 20);
        assert_eq!(scroll_to(5, Some(PageDown), 10, 30), 14);
        assert_eq!(scroll_to(5, None, 10, 8), 0);
    }
}
