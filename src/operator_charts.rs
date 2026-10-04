// SPDX-License-Identifier: MIT
use crate::chart_render::chart_window_label;
use crate::chart_scale;
use crate::domain::Tone;
use crate::formatting::{count, telemetry_age};
use crate::operator_history::History;
use crate::theme::{panel, BLUE, CYAN, DIM, MUTED, PANEL, YELLOW};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::time::Duration;
// At 1×, one column per sample; no connections across missing readings
// or changed process/provider identity. Callers fit the scale to visible samples.
fn trace(
    frame: &mut Frame,
    area: Rect,
    values: &[Option<u64>],
    breaks: &[bool],
    ceiling: u64,
    color: Color,
    zoom: u16,
) {
    if area.is_empty() || ceiling == 0 {
        return;
    }
    let mut previous: Option<u16> = None;
    let mut previous_index = None;
    for column in 0..area.width {
        let from_right = usize::from((area.width - 1 - column) / zoom.max(1));
        let Some(i) = values.len().checked_sub(from_right + 1) else {
            continue;
        };
        let value = &values[i];
        if previous_index != Some(i) && breaks.get(i).copied().unwrap_or(false) {
            previous = None;
        }
        previous_index = Some(i);
        let Some(value) = value else {
            previous = None;
            continue;
        };
        let x = area.x + column;
        let scaled = ((u128::from((*value).min(ceiling)) * u128::from(area.height - 1)
            + u128::from(ceiling) / 2)
            / u128::from(ceiling)) as u16;
        let y = area.bottom() - 1 - scaled;
        let mut glyph = "━";
        if let Some(old_y) = previous.filter(|old_y| *old_y != y) {
            for row in old_y.min(y) + 1..old_y.max(y) {
                frame.buffer_mut()[(x, row)].set_symbol("┃").set_fg(color);
            }
            frame.buffer_mut()[(x, old_y)]
                .set_symbol(if y > old_y { "┓" } else { "┛" })
                .set_fg(color);
            glyph = if y > old_y { "┗" } else { "┏" };
        }
        frame.buffer_mut()[(x, y)]
            .set_symbol(if *value > ceiling { "↑" } else { glyph })
            .set_fg(color);
        previous = Some(y);
    }
}

fn panel_area(
    frame: &mut Frame,
    area: Rect,
    name: &str,
    caption: String,
    subtitle: String,
    footer: Option<String>,
) -> Rect {
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .style(Style::default().bg(PANEL))
        .title(Line::from(vec![
            Span::styled(
                format!(" {name} "),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
            Span::styled(caption, Style::default().fg(MUTED)),
        ]));
    if let Some(footer) = footer {
        block = block.title_bottom(Line::from(Span::styled(
            format!(" {footer} "),
            Style::default().fg(MUTED),
        )));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return inner;
    }
    frame.render_widget(
        Paragraph::new(subtitle).style(Style::default().fg(MUTED)),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    Rect::new(
        inner.x,
        inner.y + 1,
        inner.width,
        inner.height.saturating_sub(1),
    )
}

fn queue_tick(value: u64) -> String {
    if value < 10_000 {
        return value.to_string();
    }
    let (divisor, suffix) = [
        (1_000_000_000_000_000_000_u64, "E"),
        (1_000_000_000_000_000, "P"),
        (1_000_000_000_000, "T"),
        (1_000_000_000, "G"),
        (1_000_000, "M"),
        (1_000, "k"),
    ]
    .into_iter()
    .find(|(divisor, _)| value >= *divisor)
    .unwrap();
    format!("{:.1}{suffix}", value as f64 / divisor as f64)
}

fn queue_axis(
    history: &History,
    width: u16,
    zoom: u16,
    window: Option<usize>,
) -> (u16, u64, usize) {
    let mut label_width = 3.min(width.saturating_sub(1));
    loop {
        let visible = window
            .unwrap_or(usize::from(width.saturating_sub(label_width)))
            .div_ceil(usize::from(zoom.max(1)))
            .max(1);
        let ceiling = chart_scale::ceiling(
            history
                .points
                .iter()
                .rev()
                .take(visible)
                .flat_map(|point| [point.active, point.waiting])
                .flatten()
                .max()
                .unwrap_or(0),
            1,
        );
        let required = (queue_tick(ceiling).len() as u16 + 1).min(width.saturating_sub(1));
        if required <= label_width {
            return (label_width, ceiling, visible);
        }
        // Recalculate after reserving the tick gutter. Only grow the gutter:
        // an old spike leaving the visible window must not toggle its width.
        label_width = required;
    }
}

// Merge actual stroke directions instead of stamping a cross into every
// occupied cell. Coincident vertical connectors must remain vertical lines.
fn queue_junction(active: char, waiting: char) -> char {
    if active == '↑' || waiting == '↑' {
        return '↑';
    }
    if active == '━' && waiting == '━' {
        return '≈';
    }
    let directions = |glyph| match glyph {
        '━' => 0b1010, // left + right
        '┃' => 0b0101, // up + down
        '┏' => 0b0110,
        '┓' => 0b1100,
        '┗' => 0b0011,
        '┛' => 0b1001,
        _ => 0,
    };
    match directions(active) | directions(waiting) {
        0b0101 => '┃',
        0b0110 => '┏',
        0b1100 => '┓',
        0b0011 => '┗',
        0b1001 => '┛',
        0b0111 => '┣',
        0b1101 => '┫',
        0b1110 => '┳',
        0b1011 => '┻',
        _ => '╋',
    }
}

pub(super) fn queue_plot_width(history: &History, width: u16) -> u16 {
    let inner = width.saturating_sub(2);
    let (gutter, _, _) = queue_axis(history, inner, 1, None);
    inner.saturating_sub(gutter)
}

pub(super) fn queue(
    frame: &mut Frame,
    area: Rect,
    history: &History,
    interval: Duration,
    zoom: u16,
    window: Option<usize>,
) {
    let last = history.points.back();
    let active = last.and_then(|p| p.active);
    let waiting = last.and_then(|p| p.waiting);
    // A short terminal still gets a separate, selectable queue panel with
    // both exact counts. Expanding it reveals the full history.
    if area.height <= 5 {
        let active = format!("active {}", count(active));
        let waiting = format!("waiting {}", count(waiting));
        let mut block = panel(
            &if area.height <= 2 {
                format!("queue {active}")
            } else {
                "queue".into()
            },
            Tone::Cyan,
        );
        let mut lines = Vec::new();
        if area.height >= 3 {
            lines.push(Line::from(Span::styled(active, Style::default().fg(CYAN))));
        }
        if area.height >= 4 {
            lines.push(Line::from(Span::styled(
                waiting,
                Style::default().fg(YELLOW),
            )));
        } else {
            block = block.title_bottom(Line::from(Span::styled(
                format!(" {waiting} "),
                Style::default().fg(YELLOW),
            )));
        }
        if area.height >= 5 {
            lines.push(Line::from(Span::styled(
                "Enter: history",
                Style::default().fg(MUTED),
            )));
        }
        frame.render_widget(Paragraph::new(lines).block(block), area);
        return;
    }
    let narrow = area.width < 38;
    let (label_width, ceiling, visible) =
        queue_axis(history, area.width.saturating_sub(2), zoom, window);
    let ceiling_label = queue_tick(ceiling);
    let counts = format!("active {} waiting {}", count(active), count(waiting));
    let split_counts = narrow && counts.len() > usize::from(area.width.saturating_sub(2));
    let window = chart_window_label(
        if window.is_some() {
            visible
        } else {
            history.points.len().min(visible)
        },
        interval,
    );
    let subtitle = if split_counts {
        format!("active {}", count(active))
    } else {
        [
            format!("0–{ceiling_label} req · auto · {window}"),
            format!("0–{ceiling_label} req · {window}"),
            format!("req · {window}"),
            window,
        ]
        .into_iter()
        .find(|text| Line::from(text.as_str()).width() <= usize::from(area.width.saturating_sub(2)))
        .unwrap_or_default()
    };
    let plot = panel_area(
        frame,
        area,
        "queue",
        if narrow {
            String::new()
        } else {
            format!("active {} · waiting {} ", count(active), count(waiting))
        },
        subtitle,
        None,
    );
    if plot.height < 2 || plot.width <= label_width {
        return;
    }
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                if split_counts {
                    String::new()
                } else if narrow {
                    format!("active {} ", count(active))
                } else {
                    "active ".into()
                },
                Style::default().fg(CYAN),
            ),
            Span::styled(
                if narrow {
                    format!("waiting {}", count(waiting))
                } else {
                    "waiting ".into()
                },
                Style::default().fg(YELLOW),
            ),
            Span::styled(
                if narrow { "" } else { "═ equal" },
                Style::default().fg(Color::White),
            ),
        ])),
        // Keep the legend in the bottom border so a six-row panel still has
        // three plot rows: the 0, 1 and 2 levels must remain distinguishable.
        Rect::new(
            area.x + if narrow { 1 } else { 2 },
            area.bottom() - 1,
            area.width.saturating_sub(if narrow { 2 } else { 4 }),
            1,
        ),
    );
    let graph = Rect::new(
        plot.x + label_width,
        plot.y,
        plot.width - label_width,
        plot.height,
    );
    for (y, label) in [(graph.y, ceiling_label), (graph.bottom() - 1, "0".into())] {
        frame.render_widget(
            Paragraph::new(label).style(Style::default().fg(MUTED)),
            Rect::new(plot.x, y, label_width, 1),
        );
    }
    for x in graph.x..graph.right() {
        frame.buffer_mut()[(x, graph.bottom() - 1)]
            .set_symbol("─")
            .set_fg(DIM);
    }
    // Expand the shared ordinal sample window across this graph, retaining gaps.
    let indices: Vec<_> = (0..usize::from(graph.width))
        .map(|column| {
            let offset = column * visible / usize::from(graph.width);
            history.points.len().checked_sub(visible - offset)
        })
        .collect();
    let active_values: Vec<_> = indices
        .iter()
        .map(|i| i.and_then(|i| history.points[i].active))
        .collect();
    let waiting_values: Vec<_> = indices
        .iter()
        .map(|i| i.and_then(|i| history.points[i].waiting))
        .collect();
    let breaks: Vec<_> = indices
        .iter()
        .enumerate()
        .map(|(column, i)| {
            i.is_some_and(|i| i > 0 && history.points[i].provider != history.points[i - 1].provider)
                && (column == 0 || indices[column - 1] != *i)
        })
        .collect();
    trace(frame, graph, &active_values, &breaks, ceiling, CYAN, 1);
    let active_cells: Vec<_> = (graph.y..graph.bottom())
        .flat_map(|y| (graph.x..graph.right()).map(move |x| (x, y)))
        .filter_map(|(x, y)| {
            let cell = &frame.buffer_mut()[(x, y)];
            (cell.fg == CYAN).then_some((x, y, cell.symbol().chars().next().unwrap_or(' ')))
        })
        .collect();
    trace(frame, graph, &waiting_values, &breaks, ceiling, YELLOW, 1);
    // Equality is a fact about the samples, not an intersection of rasterized
    // connectors. Preserve stroke geometry and mark rounded collisions separately.
    for (x, y, active_glyph) in active_cells {
        if frame.buffer_mut()[(x, y)].fg == YELLOW {
            let column = usize::from(x - graph.x);
            let equal = active_values[column]
                .zip(waiting_values[column])
                .filter(|(active, waiting)| active == waiting)
                .is_some_and(|(value, _)| {
                    let scaled = ((u128::from(value.min(ceiling)) * u128::from(graph.height - 1)
                        + u128::from(ceiling) / 2)
                        / u128::from(ceiling)) as u16;
                    y == graph.bottom() - 1 - scaled
                });
            let waiting_glyph = frame.buffer_mut()[(x, y)]
                .symbol()
                .chars()
                .next()
                .unwrap_or(' ');
            let glyph = if equal && active_glyph != '↑' && waiting_glyph != '↑' {
                '═'
            } else {
                queue_junction(active_glyph, waiting_glyph)
            };
            frame.buffer_mut()[(x, y)]
                .set_symbol(&glyph.to_string())
                .set_fg(if equal { Color::White } else { MUTED });
        }
    }
    if active_values
        .iter()
        .chain(&waiting_values)
        .all(Option::is_none)
    {
        frame.render_widget(
            Paragraph::new("No queue samples").style(Style::default().fg(MUTED)),
            graph,
        );
    }
}

pub(super) fn latency(frame: &mut Frame, area: Rect, history: &History, zoom: u16) {
    let latest = history.timings.iter().rev().find(|p| p.1.is_some());
    let caption = latest
        .map(|p| format!("{} ms · REPORTED ", p.1.unwrap()))
        .unwrap_or_else(|| "— ".into());
    let visible = area.width.saturating_sub(2).div_ceil(zoom.max(1)) as usize;
    let ceiling = chart_scale::ceiling(
        history
            .timings
            .iter()
            .rev()
            .take(visible)
            .filter_map(|point| point.1)
            .max()
            .unwrap_or(0),
        1,
    );
    let subtitle = latest
        .map(|p| {
            format!(
                "0–{ceiling} ms · auto · {zoom}× · {}",
                telemetry_age(Some(p.2))
            )
        })
        .unwrap_or_default();
    let plot = panel_area(frame, area, "first token", caption, subtitle, None);
    let values: Vec<_> = history.timings.iter().map(|p| p.1).collect();
    // Each bar is one observed request; zoom only widens it.
    for column in 0..plot.width {
        let i = usize::from(column / zoom.max(1));
        if let Some(Some(value)) = values
            .len()
            .checked_sub(i + 1)
            .and_then(|index| values.get(index))
        {
            let x = plot.right() - 1 - column;
            let h = if plot.height == 0 {
                0
            } else {
                ((*value).min(ceiling) as u128 * plot.height as u128).div_ceil(ceiling as u128)
                    as u16
            };
            for y in plot.bottom().saturating_sub(h)..plot.bottom() {
                frame.buffer_mut()[(x, y)]
                    .set_symbol(if *value > ceiling { "↑" } else { "▇" })
                    .set_fg(BLUE);
            }
        }
    }
}

#[cfg(test)]
#[path = "tests/operator_charts.rs"]
mod tests;
