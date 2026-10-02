// SPDX-License-Identifier: MIT
//! Bounded OS/queue samples and explicitly reported request latency.
use super::*;

#[derive(Clone, Default)]
struct Point {
    active: Option<u64>,
    waiting: Option<u64>,
    footprint: Option<u64>,
    process: Option<(u32, u64)>,
    provider: String,
}

#[derive(Clone, Default)]
pub(super) struct History {
    points: VecDeque<Point>,
    timings: VecDeque<(String, Option<u64>, SystemTime)>,
}

impl History {
    pub fn observe(&mut self, sample: &Sample, limit: usize) {
        let fresh = sample.llm_source == TelemetrySource::Live
            && sample.llm_status != "stale"
            && sample.llm_observed_at.is_some_and(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age <= Duration::from_secs(5))
            });
        self.points.push_back(Point {
            active: fresh.then_some(sample.llm_active_requests).flatten(),
            waiting: fresh.then_some(sample.llm_waiting_requests).flatten(),
            footprint: sample.process_memory.as_ref().map(|m| m.footprint),
            process: sample.process_memory.as_ref().map(|m| (m.pid, m.started)),
            provider: sample.llm_provider.clone(),
        });
        while self.points.len() > limit {
            self.points.pop_front();
        }
        for request in &sample.llm_requests {
            let key = format!("{}\0{}\0{}", request.provider, request.model, request.id);
            if let Some(old) = self.timings.iter_mut().find(|old| old.0 == key) {
                if let Some(ttft) = request.ttft_ms {
                    old.1 = Some(ttft);
                    old.2 = request.observed_at.unwrap_or(old.2);
                }
            } else {
                self.timings.push_back((
                    key,
                    request.ttft_ms,
                    request.observed_at.unwrap_or_else(SystemTime::now),
                ));
            }
        }
        while self.timings.len() > 240 {
            self.timings.pop_front();
        }
    }

    pub fn has_latency(&self) -> bool {
        self.timings.iter().any(|p| p.1.is_some())
    }
}

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

fn queue_axis(history: &History, width: u16, zoom: u16) -> (u16, u64, usize) {
    let mut label_width = 3.min(width.saturating_sub(1));
    loop {
        let visible = width.saturating_sub(label_width).div_ceil(zoom.max(1)) as usize;
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

pub(super) fn queue(
    frame: &mut Frame,
    area: Rect,
    history: &History,
    interval: Duration,
    zoom: u16,
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
    let (label_width, ceiling, visible) = queue_axis(history, area.width.saturating_sub(2), zoom);
    let ceiling_label = queue_tick(ceiling);
    let counts = format!("active {} waiting {}", count(active), count(waiting));
    let split_counts = narrow && counts.len() > usize::from(area.width.saturating_sub(2));
    let window = chart_window_label(history.points.len().min(visible), interval);
    let subtitle = if split_counts {
        format!("active {}", count(active))
    } else if area.width < 26 {
        format!("0–{ceiling_label} req · {window}")
    } else {
        format!("0–{ceiling_label} req · auto · {window}")
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
    let active_values: Vec<_> = history.points.iter().map(|p| p.active).collect();
    let waiting_values: Vec<_> = history.points.iter().map(|p| p.waiting).collect();
    let breaks: Vec<_> = history
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| i > 0 && p.provider != history.points[i - 1].provider)
        .collect();
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
                if narrow { "" } else { "═ overlap" },
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
    trace(frame, graph, &active_values, &breaks, ceiling, CYAN, zoom);
    let active_cells: Vec<_> = (graph.y..graph.bottom())
        .flat_map(|y| (graph.x..graph.right()).map(move |x| (x, y)))
        .filter_map(|(x, y)| {
            let cell = &frame.buffer_mut()[(x, y)];
            (cell.fg == CYAN).then_some((x, y, cell.symbol() == "↑"))
        })
        .collect();
    trace(
        frame,
        graph,
        &waiting_values,
        &breaks,
        ceiling,
        YELLOW,
        zoom,
    );
    // Preserve both series wherever their rasterized traces share a cell.
    // This includes equal values and values indistinguishable at terminal resolution.
    for (x, y, active_overflow) in active_cells {
        if frame.buffer_mut()[(x, y)].fg == YELLOW {
            let overflow = active_overflow || frame.buffer_mut()[(x, y)].symbol() == "↑";
            frame.buffer_mut()[(x, y)]
                .set_symbol(if overflow { "↑" } else { "═" })
                .set_fg(Color::White);
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

fn footprint_axis(history: &History, width: u16, zoom: u16) -> (u16, u64, u64) {
    let mut gutter = 9.min(width.saturating_sub(1));
    loop {
        let visible = width.saturating_sub(gutter).div_ceil(zoom.max(1)) as usize;
        let (low, high) = chart_scale::range(
            history
                .points
                .iter()
                .rev()
                .take(visible)
                .filter_map(|p| p.footprint),
            MIB,
        );
        let required =
            (bytes(low).len().max(bytes(high).len()) as u16 + 1).min(width.saturating_sub(1));
        if required <= gutter {
            return (gutter, low, high);
        }
        gutter = required;
    }
}

pub(super) fn footprint(
    frame: &mut Frame,
    area: Rect,
    history: &History,
    sample: &Sample,
    zoom: u16,
) {
    let last = sample.process_memory.as_ref();
    let mut block = panel("process memory", Tone::Cyan);
    if area.width >= 28 && area.height >= 6 {
        block = block.title(
            Line::from(Span::styled(" auto ", Style::default().fg(MUTED)))
                .alignment(Alignment::Right),
        );
    }
    let mut footer = last
        .map(|m| format!("peak {}", bytes(m.peak)))
        .unwrap_or_else(|| "OS footprint".into());
    if let Some(growth) = last.and(sample.process_memory_growth) {
        let detailed = format!("{footer} · growth {}", signed_rate(growth));
        if Line::from(detailed.as_str()).width() + 2 <= usize::from(area.width.saturating_sub(2)) {
            footer = detailed;
        }
    }
    block = block.title_bottom(Line::from(Span::styled(
        format!(" {footer} "),
        Style::default().fg(MUTED),
    )));
    let mut inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let reading = last
        .map(|m| bytes(m.footprint))
        .unwrap_or_else(|| "—".into());
    let mut headline = Line::from(Span::styled(
        reading,
        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
    ));
    let source = last
        .map(|m| {
            [
                format!("OS footprint · PID {}", m.pid),
                format!("OS · PID {}", m.pid),
                format!("OS PID {}", m.pid),
                format!("PID {}", m.pid),
            ]
            .into_iter()
            .find(|label| headline.width() + label.len() + 2 <= usize::from(inner.width))
            .unwrap_or_else(|| format!("PID {}", m.pid))
        })
        .unwrap_or_else(|| "OS unavailable".into());
    let source_in_headline = headline.width() + source.len() + 2 <= usize::from(inner.width);
    if source_in_headline {
        headline.spans.push(Span::raw(
            " ".repeat(usize::from(inner.width) - headline.width() - source.len()),
        ));
        headline
            .spans
            .push(Span::styled(source.clone(), Style::default().fg(MUTED)));
    }
    frame.render_widget(
        Paragraph::new(headline),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    inner.y += 1;
    inner.height = inner.height.saturating_sub(1);
    if inner.height < 3 {
        frame.render_widget(
            Paragraph::new(if source_in_headline {
                "Enter: history".into()
            } else {
                source
            })
            .style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    }
    let (gutter, low, high) = footprint_axis(history, inner.width, zoom);
    let plot = Rect::new(
        inner.x + gutter,
        inner.y,
        inner.width.saturating_sub(gutter),
        inner.height,
    );
    let visible = plot.width.div_ceil(zoom.max(1)) as usize;
    if history
        .points
        .iter()
        .rev()
        .take(visible)
        .all(|p| p.footprint.is_none())
    {
        frame.render_widget(
            Paragraph::new("No footprint samples").style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    }
    for (row, value) in [
        (0, high),
        ((plot.height - 1) / 2, low + (high - low) / 2),
        (plot.height - 1, low),
    ] {
        frame.render_widget(
            Paragraph::new(bytes(value))
                .alignment(Alignment::Right)
                .style(Style::default().fg(MUTED)),
            Rect::new(inner.x, plot.y + row, gutter.saturating_sub(1), 1),
        );
        for x in plot.x..plot.right() {
            frame.buffer_mut()[(x, plot.y + row)]
                .set_symbol(if row + 1 == plot.height { "─" } else { "┄" })
                .set_fg(DIM);
        }
    }
    let values: Vec<_> = history
        .points
        .iter()
        .map(|p| p.footprint.map(|v| v.saturating_sub(low)))
        .collect();
    let breaks: Vec<_> = history
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| i > 0 && p.process != history.points[i - 1].process)
        .collect();
    trace(frame, plot, &values, &breaks, high - low, CYAN, zoom);
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
