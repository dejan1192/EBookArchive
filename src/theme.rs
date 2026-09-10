//! Colors and the small drawing helpers that give the app one consistent look.
//!
//! Everything visual that is not a widget lives here: the palette, the two
//! progress bars (determinate and not), and the byte/rate/duration formatters
//! the download rows print.

use ratatui::style::{Color, Style};
use ratatui::text::Span;

// -- palette ---------------------------------------------------------------
//
// Chosen to sit well on a dark terminal without fighting it. Body text keeps
// the terminal's own foreground; only accents are pinned, so a light theme
// stays readable.

/// Primary accent: focus rings, the active tab, in-flight work.
pub const ACCENT: Color = Color::Rgb(86, 182, 194);
/// Accent for something finished or currently selected.
pub const ACCENT_BRIGHT: Color = Color::Rgb(127, 219, 232);
/// Success: a file that landed on disk.
pub const OK: Color = Color::Rgb(152, 195, 121);
/// In progress, or needing attention but not broken.
pub const WARN: Color = Color::Rgb(229, 192, 123);
/// Failures.
pub const ERR: Color = Color::Rgb(224, 108, 117);
/// Secondary text: hints, sizes, anything that should recede.
pub const MUTED: Color = Color::Rgb(120, 130, 148);
/// Even quieter than [`MUTED`] -- progress-bar troughs, inactive borders.
pub const FAINT: Color = Color::Rgb(72, 80, 94);
/// Source tags.
pub const VIOLET: Color = Color::Rgb(198, 120, 221);
/// Formats and other neutral metadata.
pub const BLUE: Color = Color::Rgb(97, 175, 239);
/// Background behind the selected row.
pub const SELECTION_BG: Color = Color::Rgb(44, 51, 64);
/// Foreground for text sitting on an accent background.
pub const ON_ACCENT: Color = Color::Rgb(22, 26, 33);

/// Left edge of the selected row, so the eye catches it without a full bar.
pub const CURSOR: &str = "▎";

/// Eighth-width blocks, so a bar can stop between two cells.
const EIGHTHS: [&str; 8] = ["", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
const FULL: &str = "█";
const TROUGH: &str = "░";

/// A determinate bar, `width` cells wide, filled to `fraction` of the way.
///
/// Resolution is eight times the cell count: the last filled cell is drawn as
/// a partial block, so a 10-cell bar moves in 80 visible steps instead of 10.
pub fn progress_bar(fraction: f64, width: usize, color: Color) -> Vec<Span<'static>> {
    let fraction = fraction.clamp(0.0, 1.0);
    let eighths = (fraction * width as f64 * 8.0).round() as usize;
    let full = eighths / 8;
    let remainder = eighths % 8;

    let mut filled = FULL.repeat(full.min(width));
    filled.push_str(EIGHTHS[remainder]);

    let drawn = full.min(width) + usize::from(remainder > 0);
    let trough = TROUGH.repeat(width.saturating_sub(drawn));

    vec![
        Span::styled(filled, Style::new().fg(color)),
        Span::styled(trough, Style::new().fg(FAINT)),
    ]
}

/// An indeterminate bar: a short comet sweeping back and forth.
///
/// Used when the server sends no `Content-Length`, so there is a real
/// distinction on screen between "5% done" and "size unknown".
pub fn pulse(frame: u64, width: usize, color: Color) -> Vec<Span<'static>> {
    if width == 0 {
        return Vec::new();
    }
    const COMET: usize = 3;

    // Bounce: walk 0..span, then back down, so the head never jumps.
    let span = width.saturating_sub(COMET).max(1);
    let cycle = span * 2;
    let step = (frame as usize) % cycle;
    let head = if step < span { step } else { cycle - step };

    let mut spans = Vec::new();
    if head > 0 {
        spans.push(Span::styled(TROUGH.repeat(head), Style::new().fg(FAINT)));
    }
    // Fade the tail so the direction of travel is readable.
    for (offset, glyph) in ["▓", "█", "▓"].iter().enumerate() {
        if head + offset < width {
            spans.push(Span::styled(glyph.to_string(), Style::new().fg(color)));
        }
    }
    let tail = width.saturating_sub(head + COMET);
    if tail > 0 {
        spans.push(Span::styled(TROUGH.repeat(tail), Style::new().fg(FAINT)));
    }
    spans
}

/// `1.2 MB/s`, or `--` before enough has arrived to measure.
pub fn rate(bytes: u64, elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 0.3 || bytes == 0 {
        return "--".to_string();
    }
    format!("{}/s", bytes_per(bytes as f64 / seconds))
}

/// `0:42`, or `--:--` when the remaining time cannot be estimated yet.
pub fn eta(seen: u64, total: u64, elapsed: std::time::Duration) -> String {
    let seconds = elapsed.as_secs_f64();
    if seconds < 0.3 || seen == 0 || total <= seen {
        return "--:--".to_string();
    }
    let remaining = (total - seen) as f64 / (seen as f64 / seconds);
    if !remaining.is_finite() || remaining > 359_999.0 {
        return "--:--".to_string();
    }
    let remaining = remaining as u64;
    match remaining / 3600 {
        0 => format!("{}:{:02}", remaining / 60, remaining % 60),
        hours => format!("{hours}:{:02}:{:02}", (remaining / 60) % 60, remaining % 60),
    }
}

/// Decimal byte sizes, matching what the catalogs themselves report.
fn bytes_per(value: f64) -> String {
    const UNITS: [&str; 5] = ["B", "kB", "MB", "GB", "TB"];
    let mut value = value;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    match unit {
        0 => format!("{value:.0} {}", UNITS[unit]),
        _ => format!("{value:.1} {}", UNITS[unit]),
    }
}

/// Dimmed hint text, e.g. the key legends along the bottom of a list.
pub fn hint(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::new().fg(MUTED))
}

/// A key name inside a hint legend.
pub fn key(text: &str) -> Span<'static> {
    Span::styled(text.to_string(), Style::new().fg(ACCENT).bold())
}

/// Cut a run of spans down to `width` display cells, marking the cut with `…`.
///
/// Measured in cells rather than `char`s so wide glyphs in a title cannot push
/// the right-hand column off the edge of the row.
pub fn clip(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    use unicode_width::UnicodeWidthChar;

    let total: usize = spans.iter().map(|span| span.width()).sum();
    if total <= width {
        return spans;
    }
    if width == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut used = 0;
    for span in spans {
        let span_width = span.width();
        if used + span_width <= width.saturating_sub(1) {
            used += span_width;
            out.push(span);
            continue;
        }
        // This span is the one that overflows: keep as much of it as fits and
        // leave a cell for the ellipsis.
        let budget = width.saturating_sub(used + 1);
        let mut kept = String::new();
        let mut kept_width = 0;
        for ch in span.content.chars() {
            let ch_width = ch.width().unwrap_or(0);
            if kept_width + ch_width > budget {
                break;
            }
            kept.push(ch);
            kept_width += ch_width;
        }
        kept.push('…');
        out.push(Span::styled(kept, span.style));
        break;
    }
    out
}
