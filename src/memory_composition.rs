// SPDX-License-Identifier: MIT
//! Physical RAM by kind: what can be reclaimed, compressed or never moved.
//! Wired memory cannot be compressed or swapped; free and file cache are
//! headroom. A marker shows the GPU working-set limit when a runtime reports it;
//! it turns yellow, with a legend reason, once wired memory exceeds the limit.
use crate::domain::Sample;
use crate::formatting::bytes;
use crate::theme::{BLUE, CYAN, DIM, EDGE, MUTED, YELLOW};
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// One segment: legend name, glyph and color. Glyphs keep kinds distinct
/// without relying on color.
struct Kind {
    name: &'static str,
    glyph: &'static str,
    color: Color,
}

const WIRED: Kind = Kind {
    name: "wired",
    glyph: "█",
    color: BLUE,
};
const APP: Kind = Kind {
    name: "app",
    glyph: "█",
    color: CYAN,
};
const COMPRESSED: Kind = Kind {
    name: "comp",
    glyph: "▓",
    color: MUTED,
};
const CACHE: Kind = Kind {
    name: "cache",
    glyph: "░",
    color: MUTED,
};
const FREE: Kind = Kind {
    name: "free",
    glyph: " ",
    color: EDGE,
};

/// The GPU wired-memory cap: an explicit `iogpu.wired_limit_mb`, else the
/// runtime's reported Metal recommended working set.
pub(crate) fn gpu_limit(sample: &Sample) -> Option<u64> {
    sample
        .metal
        .resource_limit
        .or(sample.mlx.recommended_working_set)
        .filter(|limit| *limit > 0)
}

/// Segment sizes in bytes, clamped so they never exceed physical RAM.
pub(crate) fn segments(sample: &Sample) -> Option<[u64; 5]> {
    if !sample.vm_available || sample.total_memory == 0 {
        return None;
    }
    let mut remaining = sample.total_memory;
    let mut take = |value: u64| {
        let value = value.min(remaining);
        remaining -= value;
        value
    };
    let wired = take(sample.wired);
    let app = take(sample.anonymous);
    let compressed = take(sample.compressor);
    let cache = take(sample.file_backed);
    // Without any kind counters everything would read as free: say unknown.
    if wired + app + compressed + cache == 0 {
        return None;
    }
    Some([wired, app, compressed, cache, remaining])
}

fn kinds() -> [Kind; 5] {
    [WIRED, APP, COMPRESSED, CACHE, FREE]
}

/// A one-row bar of RAM composition. Returns false when counters are missing.
pub(crate) fn draw_bar(frame: &mut Frame, area: Rect, sample: &Sample) -> bool {
    let Some(sizes) = segments(sample) else {
        return false;
    };
    if area.is_empty() {
        return true;
    }
    let width = u64::from(area.width);
    let total = sample.total_memory;
    // Largest-remainder rounding keeps every column assigned exactly once.
    let mut columns = sizes.map(|size| u128::from(size) * u128::from(width) / u128::from(total));
    let assigned: u128 = columns.iter().sum();
    let mut order: Vec<usize> = (0..sizes.len()).collect();
    order.sort_by_key(|&i| {
        std::cmp::Reverse(u128::from(sizes[i]) * u128::from(width) % u128::from(total))
    });
    for &i in order.iter().take((u128::from(width) - assigned) as usize) {
        columns[i] += 1;
    }
    let mut x = area.x;
    for (kind, count) in kinds().iter().zip(columns) {
        for _ in 0..count {
            let cell = &mut frame.buffer_mut()[(x, area.y)];
            if kind.glyph == " " {
                cell.set_symbol(" ").set_bg(kind.color);
            } else {
                cell.set_symbol(kind.glyph).set_fg(kind.color).set_bg(EDGE);
            }
            x += 1;
        }
    }
    // GPU working-set limit: wired model memory beyond it slows generation.
    if let Some(limit) = gpu_limit(sample) {
        let column = (u128::from(limit.min(total)) * u128::from(width) / u128::from(total))
            .min(u128::from(width) - 1) as u16;
        let over = sample.wired > limit;
        frame.buffer_mut()[(area.x + column, area.y)]
            .set_symbol("┃")
            .set_fg(if over { YELLOW } else { Color::White });
    }
    true
}

/// Legend with exact sizes, flowing onto at most `max_lines` rows. Entries
/// never split; ones that do not fit are dropped whole.
pub(crate) fn legend(sample: &Sample, width: u16, max_lines: usize) -> Vec<Line<'static>> {
    let Some(sizes) = segments(sample) else {
        return vec![Line::from(Span::styled(
            "RAM composition unavailable",
            Style::default().fg(MUTED),
        ))];
    };
    let mut entries: Vec<(String, Color)> = kinds()
        .iter()
        .zip(sizes)
        .filter(|(kind, size)| *size > 0 || kind.name == "free")
        .map(|(kind, size)| {
            let swatch = if kind.glyph == " " { "□" } else { kind.glyph };
            (format!("{swatch}{} {}", kind.name, bytes(size)), kind.color)
        })
        .collect();
    if let Some(limit) = gpu_limit(sample) {
        // Over the limit is worth inspecting; the legend states the reason.
        entries.push(if sample.wired > limit {
            (format!("┃wired over GPU limit {}", bytes(limit)), YELLOW)
        } else {
            (format!("┃GPU limit {}", bytes(limit)), Color::White)
        });
    }
    if max_lines == 0 {
        return Vec::new();
    }
    let mut lines = vec![Line::default()];
    for (text, color) in entries {
        let style = Style::default().fg(if color == EDGE { DIM } else { color });
        let line = lines.last_mut().unwrap();
        let spaced = Span::styled(format!("  {text}"), style);
        if line.spans.is_empty() || line.width() + spaced.width() > usize::from(width) {
            let span = Span::styled(text, style);
            if span.width() > usize::from(width) {
                continue;
            }
            if !line.spans.is_empty() {
                if lines.len() == max_lines {
                    continue;
                }
                lines.push(Line::default());
            }
            lines.last_mut().unwrap().spans.push(span);
        } else {
            line.spans.push(spaced);
        }
    }
    lines
}

/// Draw the bar and, when rows allow, its legend beneath it. Returns rows used.
pub(crate) fn draw(frame: &mut Frame, area: Rect, sample: &Sample) -> u16 {
    if area.height == 0 {
        return 0;
    }
    if !draw_bar(frame, Rect::new(area.x, area.y, area.width, 1), sample) {
        frame.render_widget(
            Paragraph::new("RAM composition unavailable").style(Style::default().fg(MUTED)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        return 1;
    }
    if area.height < 2 {
        return 1;
    }
    let lines = legend(sample, area.width, usize::from(area.height - 1));
    let rows = lines.len() as u16;
    frame.render_widget(
        Paragraph::new(lines),
        Rect::new(area.x, area.y + 1, area.width, rows),
    );
    1 + rows
}

#[cfg(test)]
#[path = "tests/memory_composition.rs"]
mod tests;
