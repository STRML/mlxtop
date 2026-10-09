// SPDX-License-Identifier: MIT
//! Swap occupancy is capacity, independent of paging traffic or pressure.
use crate::domain::Sample;
use crate::theme::{CYAN, EDGE, MUTED, PANEL};
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Paragraph;
use ratatui::Frame;

/// Render a single row inside the paging panel, without an additional border.
pub(super) fn draw(frame: &mut Frame, area: Rect, sample: &Sample) {
    if area.is_empty() {
        return;
    }
    let row = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(Paragraph::default().style(Style::default().bg(PANEL)), row);
    if !sample.swap_available || sample.swap_total == 0 {
        let state = if sample.swap_available {
            "SWAP 0 B · not allocated"
        } else {
            "SWAP — unavailable"
        };
        frame.render_widget(Paragraph::new(state).style(Style::default().fg(MUTED)), row);
        return;
    }

    capacity_bar(frame, row, "SWAP", sample.swap_used, sample.swap_total);
}

/// Compressor occupancy as a share of physical RAM, in the same form as SWAP.
pub(super) fn draw_compressor(frame: &mut Frame, area: Rect, sample: &Sample) {
    if area.is_empty() {
        return;
    }
    let row = Rect::new(area.x, area.y, area.width, 1);
    frame.render_widget(Paragraph::default().style(Style::default().bg(PANEL)), row);
    if !sample.vm_available || sample.total_memory == 0 {
        frame.render_widget(
            Paragraph::new("COMP — unavailable").style(Style::default().fg(MUTED)),
            row,
        );
        return;
    }
    capacity_bar(frame, row, "COMP", sample.compressor, sample.total_memory);
}

fn capacity_bar(frame: &mut Frame, row: Rect, prefix: &str, used: u64, total: u64) {
    let percent = (u128::from(used) * 100 + u128::from(total) / 2) / u128::from(total);
    let label = format!("{} {percent}%", capacity(used, total));
    // Shared units preserve both exact capacity readings and a useful bar at 30 columns.
    let label_width = u16::try_from(label.len()).unwrap_or(u16::MAX);
    let bar_width = row.width.saturating_sub(label_width.saturating_add(6));
    if bar_width < 3 {
        frame.render_widget(
            Paragraph::new(format!("{prefix} {label}")).style(Style::default().fg(CYAN)),
            row,
        );
        return;
    }
    frame.render_widget(
        Paragraph::new(prefix.to_owned()).style(Style::default().fg(MUTED)),
        Rect::new(row.x, row.y, 4, 1),
    );
    let filled_eighths =
        u128::from(used.min(total)) * u128::from(bar_width) * 8 / u128::from(total);
    const PARTIAL: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];
    for column in 0..bar_width {
        let cell = &mut frame.buffer_mut()[(row.x + 5 + column, row.y)];
        let eighths = filled_eighths.saturating_sub(u128::from(column) * 8).min(8);
        if eighths == 8 {
            cell.set_symbol(" ").set_bg(CYAN);
        } else {
            cell.set_symbol(PARTIAL[eighths as usize])
                .set_fg(CYAN)
                .set_bg(EDGE);
        }
    }
    frame.render_widget(
        Paragraph::new(label).style(Style::default().fg(CYAN)),
        Rect::new(row.x + 6 + bar_width, row.y, label_width, 1),
    );
}

fn capacity(used: u64, total: u64) -> String {
    let largest = used.max(total);
    for (divisor, unit) in [
        (1024_u64.pow(3), "GiB"),
        (1024_u64.pow(2), "MiB"),
        (1024, "KiB"),
    ] {
        if largest >= divisor {
            return format!(
                "{:.1}/{:.1} {unit}",
                used as f64 / divisor as f64,
                total as f64 / divisor as f64
            );
        }
    }
    format!("{used}/{total} B")
}

#[cfg(test)]
#[path = "tests/swap_usage.rs"]
mod tests;
