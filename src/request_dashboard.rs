// SPDX-License-Identifier: MIT
//! Compact operator view of sampled request load; never a billing ledger.
use super::*;

const HISTORY_LIMIT: usize = 240;
const TREND_CEILING: u64 = 65_536;

#[derive(Clone)]
struct Entry {
    number: u64,
    usage: providers::RequestUsage,
    last_seen: SystemTime,
}

#[derive(Clone, Default)]
pub(super) struct History {
    entries: VecDeque<Entry>,
    next_number: u64,
}

fn same_request(a: &providers::RequestUsage, b: &providers::RequestUsage) -> bool {
    a.id == b.id && a.provider == b.provider && a.model == b.model
}

impl History {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn observe(&mut self, requests: &[providers::RequestUsage]) {
        for usage in requests {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| same_request(&entry.usage, usage))
            {
                // Retained provider responses and repeated file reads do not
                // refresh an observation's age.
                entry.last_seen = usage.observed_at.unwrap_or(entry.last_seen);
                entry.usage = usage.clone();
            } else {
                self.next_number = self.next_number.saturating_add(1);
                self.entries.push_back(Entry {
                    number: self.next_number,
                    usage: usage.clone(),
                    last_seen: usage.observed_at.unwrap_or_else(SystemTime::now),
                });
                while self.entries.len() > HISTORY_LIMIT {
                    self.entries.pop_front();
                }
            }
        }
    }
}

fn exact(n: u64) -> String {
    let digits = n.to_string();
    let mut output = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            output.push(',');
        }
        output.push(c);
    }
    output
}

fn clock_stamp(at: SystemTime) -> String {
    let Ok(elapsed) = at.duration_since(SystemTime::UNIX_EPOCH) else {
        return "--:--:--".into();
    };
    let seconds = elapsed.as_secs() % 86_400;
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    format!("{hours:02}:{minutes:02}:{seconds:02}")
}

fn compact_comparison(current: &providers::RequestUsage, previous: Option<&Entry>) -> String {
    let Some(previous) = previous else {
        return "PREVIOUS OBSERVED — · no earlier request".into();
    };
    if current.provider != previous.usage.provider || current.model != previous.usage.model {
        return "PREVIOUS OBSERVED — · different provider/model".into();
    }
    let sign = if current.prompt >= previous.usage.prompt {
        "+"
    } else {
        "−"
    };
    let amount = current.prompt.abs_diff(previous.usage.prompt);
    let percent = if previous.usage.prompt > 0 {
        format!(
            " ({sign}{:.1}%)",
            amount as f64 / previous.usage.prompt as f64 * 100.0
        )
    } else {
        String::new()
    };
    format!(
        "Δ {sign}{}{percent} · PREVIOUS OBSERVED {}",
        exact(amount),
        exact(previous.usage.prompt)
    )
}

fn chart_value(prompt: u64) -> u64 {
    prompt.min(TREND_CEILING)
}

fn comparable(a: &providers::RequestUsage, b: &providers::RequestUsage) -> bool {
    a.provider == b.provider && a.model == b.model
}

fn material_jump(current: &providers::RequestUsage, previous: Option<&Entry>) -> bool {
    previous.is_some_and(|old| {
        comparable(current, &old.usage)
            && current.prompt.saturating_sub(old.usage.prompt) >= 2048
            && old.usage.prompt > 0
            && current.prompt as u128 * 100 >= old.usage.prompt as u128 * 125
    })
}

struct Insight {
    title: String,
    detail: String,
    action: String,
    tone: Tone,
}

fn insight(history: &History, index: usize) -> Insight {
    let current = &history.entries[index].usage;
    let mut baseline: Vec<_> = history
        .entries
        .iter()
        .take(index)
        .rev()
        .take_while(|old| comparable(current, &old.usage))
        .take(8)
        .map(|old| old.usage.prompt)
        .collect();
    baseline.sort_unstable();
    let typical = if baseline.len() >= 3 {
        Some(baseline[baseline.len() / 2])
    } else {
        None
    };
    let detail = typical
        .map(|n| {
            format!(
                "Recent median {} · {} prior requests",
                exact(n),
                baseline.len()
            )
        })
        .unwrap_or_else(|| "Building a same-model baseline".into());
    if material_jump(
        current,
        index.checked_sub(1).and_then(|i| history.entries.get(i)),
    ) {
        return Insight {
            title: "PROMPT JUMP · input increased".into(),
            detail,
            action: "Inspect context/tool results.".into(),
            tone: Tone::Yellow,
        };
    }
    if let Some(median) = typical.filter(|median| *median > 0) {
        if current.prompt as u128 * 2 >= median as u128 * 3
            && current.prompt.saturating_sub(median) >= 2048
        {
            return Insight {
                title: format!(
                    "LARGE VS RECENT · {:.1}× median",
                    current.prompt as f64 / median as f64
                ),
                detail,
                action: "Inspect context/tool results.".into(),
                tone: Tone::Yellow,
            };
        }
        if current.prompt as u128 * 4 <= median as u128 * 3 {
            return Insight {
                title: "SMALLER INPUT · below recent median".into(),
                detail,
                action: "Compare prefill time.".into(),
                tone: Tone::Cyan,
            };
        }
        return Insight {
            title: "TYPICAL INPUT · near recent median".into(),
            detail,
            action: "If slow, check prefill/queue/GPU.".into(),
            tone: Tone::Cyan,
        };
    }
    Insight {
        title: "BASELINE SAMPLING".into(),
        detail,
        action: "Collect more requests.".into(),
        tone: Tone::Muted,
    }
}

fn is_live(entry: &Entry, sample: &Sample, now: SystemTime) -> bool {
    sample.llm_source == TelemetrySource::Live
        && sample.llm_status != "stale"
        && !entry.usage.completed
        && now
            .duration_since(entry.last_seen)
            .is_ok_and(|age| age <= Duration::from_secs(5))
        && sample.llm_observed_at.is_some_and(|at| {
            now.duration_since(at)
                .is_ok_and(|age| age <= Duration::from_secs(5))
        })
        && sample
            .llm_requests
            .iter()
            .any(|request| same_request(&entry.usage, request))
}

fn append_detail(line: &mut Line<'static>, text: String, color: Color, width: u16) {
    let span = Span::styled(format!(" · {text}"), Style::default().fg(color));
    if line.width() + span.width() <= usize::from(width) {
        line.spans.push(span);
    }
}

fn assessment_line(
    mut assessment: Insight,
    low_cache: bool,
    live: bool,
    width: u16,
) -> Line<'static> {
    // One actionable note owns the footer; baseline detail is optional.
    if low_cache {
        assessment.action = "Repeating input? Check prefix reuse.".into();
    }
    let title = assessment.title.split(" · ").next().unwrap_or("");
    let mut line = Line::from(Span::styled(
        format!(
            " {}{}{}",
            if live { "" } else { "HISTORY · " },
            if assessment.tone == Tone::Yellow {
                "! "
            } else {
                ""
            },
            title
        ),
        Style::default().fg(if live { assessment.tone.color() } else { MUTED }),
    ));
    append_detail(
        &mut line,
        assessment.action,
        if live { Color::White } else { MUTED },
        width,
    );
    append_detail(&mut line, assessment.detail, MUTED, width);
    if line.width() < usize::from(width) {
        line.spans.push(Span::raw(" "));
    }
    line
}

fn draw_bar(
    frame: &mut Frame,
    area: Rect,
    usage: &providers::RequestUsage,
    color: Color,
    jump: bool,
) {
    if area.is_empty() {
        return;
    }
    // Cache metadata must not change a bar's height. Only a full cell can
    // contain two segment colors without painting above the measured value.
    let total = (u128::from(chart_value(usage.prompt)) * u128::from(area.height) * 8)
        .div_ceil(u128::from(TREND_CEILING)) as u64;
    let cached = usage
        .cached
        .filter(|n| *n <= usage.prompt)
        .filter(|_| usage.prompt > 0)
        .map(|n| (u128::from(n) * u128::from(total) / u128::from(usage.prompt)) as u64)
        .unwrap_or(0);
    let blocks = [" ", "▁", "▂", "▃", "▄", "▅", "▆", "▇", "█"];
    if total == 0 {
        frame.buffer_mut()[(area.x, area.bottom() - 1)]
            .set_symbol("·")
            .set_fg(DIM);
    }
    for row in 0..area.height {
        let part = total.saturating_sub(u64::from(row) * 8).min(8) as usize;
        if part == 0 {
            continue;
        }
        let cached_part = cached.saturating_sub(u64::from(row) * 8).min(8) as usize;
        let (glyph, fg, bg) = if cached_part == part {
            (blocks[part], GREEN, PANEL)
        } else if cached_part > 0 && part == 8 {
            (blocks[cached_part], GREEN, color)
        } else {
            // A partial cell has an empty area too: keep its measured height
            // and use the dominant segment. The readout retains exact reuse.
            (
                blocks[part],
                if cached_part * 2 >= part {
                    GREEN
                } else {
                    color
                },
                PANEL,
            )
        };
        frame.buffer_mut()[(area.x, area.bottom() - 1 - row)]
            .set_symbol(glyph)
            .set_fg(fg)
            .set_bg(bg);
    }
    if usage.prompt > TREND_CEILING {
        frame.buffer_mut()[(area.x, area.y)]
            .set_symbol("↑")
            .set_fg(color);
    } else if jump {
        let top = area.bottom().saturating_sub(total.div_ceil(8) as u16);
        frame.buffer_mut()[(area.x, top.saturating_sub(1).max(area.y))]
            .set_symbol("!")
            .set_fg(YELLOW);
    }
}

pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    history: &History,
    sample: &Sample,
    scroll: usize,
) {
    let selected = history
        .len()
        .checked_sub(1)
        .map(|last| last - scroll.min(last));
    let now = SystemTime::now();
    let live = selected.is_some_and(|i| is_live(&history.entries[i], sample, now));
    let cached = selected.and_then(|i| {
        let usage = &history.entries[i].usage;
        usage.cached.filter(|n| *n <= usage.prompt)
    });
    let low_cache = selected.is_some_and(|i| {
        let prompt = history.entries[i].usage.prompt;
        prompt >= 4096 && cached.is_some_and(|n| u128::from(n) * 5 < u128::from(prompt))
    });
    let mut title = Line::from(vec![
        Span::styled(
            " prompt load ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        ),
        Span::styled("· 0–65,536 tokens", Style::default().fg(MUTED)),
    ]);
    append_detail(
        &mut title,
        "1 bar/request".into(),
        MUTED,
        area.width.saturating_sub(2),
    );
    append_detail(
        &mut title,
        "↑↓ history / Home latest".into(),
        MUTED,
        area.width.saturating_sub(2),
    );
    title.spans.push(Span::raw(" "));
    let mut block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(DIM))
        .style(Style::default().bg(PANEL));
    if let Some(index) = selected {
        block = block.title_bottom(assessment_line(
            insight(history, index),
            low_cache,
            live,
            area.width.saturating_sub(2),
        ));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }
    let Some(index) = selected else {
        let text = if matches!(sample.llm_provider.as_str(), "oMLX" | "KoboldCpp") {
            "Waiting for per-request prompt counts."
        } else {
            "No per-request counts received. Connect client usage to see prompt load."
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: true })
                .style(Style::default().fg(MUTED)),
            inner,
        );
        return;
    };
    let entry = &history.entries[index];
    let previous = index.checked_sub(1).and_then(|i| history.entries.get(i));
    let color = if live { CYAN } else { BLUE };
    let state = if live {
        "LIVE"
    } else if entry.usage.completed {
        "REPORTED"
    } else {
        "LAST SEEN"
    };
    let mut headline = Line::from(vec![
        Span::styled(
            format!("{} tokens", exact(entry.usage.prompt)),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {state}"), Style::default().fg(color)),
    ]);
    append_detail(
        &mut headline,
        format!("{} UTC", clock_stamp(entry.last_seen)),
        MUTED,
        inner.width,
    );
    append_detail(
        &mut headline,
        telemetry_age(Some(entry.last_seen)),
        MUTED,
        inner.width,
    );
    let cache_label = cached
        .map(|n| {
            let ratio = if entry.usage.prompt == 0 {
                0.0
            } else {
                n as f64 / entry.usage.prompt as f64
            };
            format!("CACHE {:.0}%", ratio * 100.0)
        })
        .unwrap_or_else(|| "CACHE —".into());
    let cache_color = if cached.is_none() {
        MUTED
    } else if low_cache {
        YELLOW
    } else if entry.usage.prompt > 0
        && cached.is_some_and(|n| u128::from(n) * 5 >= u128::from(entry.usage.prompt) * 4)
    {
        GREEN
    } else {
        CYAN
    };
    let cache_span = Span::styled(cache_label, Style::default().fg(cache_color));
    let cache_in_header = headline.width() + cache_span.width() + 2 <= usize::from(inner.width);
    if cache_in_header {
        headline.spans.push(Span::raw(
            " ".repeat(usize::from(inner.width) - headline.width() - cache_span.width()),
        ));
        headline.spans.push(cache_span.clone());
    }
    frame.render_widget(
        Paragraph::new(headline),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
    if inner.height < 2 {
        return;
    }
    let mut details = Line::from(Span::styled(
        compact_comparison(&entry.usage, previous),
        Style::default().fg(if live && material_jump(&entry.usage, previous) {
            YELLOW
        } else {
            MUTED
        }),
    ));
    if !cache_in_header {
        append_detail(
            &mut details,
            cache_span.content.into_owned(),
            cache_color,
            inner.width,
        );
    }
    if let Some(cached) = cached {
        append_detail(
            &mut details,
            format!("{} uncached", exact(entry.usage.prompt - cached)),
            color,
            inner.width,
        );
    }
    append_detail(
        &mut details,
        format!("#{} / {}", entry.number, history.next_number),
        MUTED,
        inner.width,
    );
    frame.render_widget(
        Paragraph::new(details),
        Rect::new(inner.x, inner.y + 1, inner.width, 1),
    );
    if inner.height < 4 {
        return;
    }

    // Use every remaining column for history; new requests enter on the right.
    // The axis is ordinal (one bar/request), with timestamps anchored to bars.
    let plot = Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 3);
    let visible = (index + 1).min(usize::from(plot.width));
    let start = index + 1 - visible;
    let first_x = plot.right() - visible as u16;
    for (offset, old) in history.entries.iter().skip(start).take(visible).enumerate() {
        let i = start + offset;
        draw_bar(
            frame,
            Rect::new(first_x + offset as u16, plot.y, 1, plot.height),
            &old.usage,
            if is_live(old, sample, now) {
                CYAN
            } else {
                BLUE
            },
            material_jump(
                &old.usage,
                i.checked_sub(1).and_then(|n| history.entries.get(n)),
            ),
        );
    }

    let axis_y = plot.bottom();
    let selected_x = plot.right() - 1;
    frame.buffer_mut()[(selected_x, axis_y)]
        .set_symbol("▲")
        .set_style(Style::default().fg(color).add_modifier(Modifier::BOLD));
    if plot.width >= 9 {
        let last_label_x = selected_x - 8;
        frame.render_widget(
            Paragraph::new(clock_stamp(entry.last_seen)).style(Style::default().fg(color)),
            Rect::new(last_label_x, axis_y, 8, 1),
        );
        let step = visible.div_ceil(4).max(12);
        for offset in (0..visible).step_by(step) {
            let x = first_x + offset as u16;
            if x + 9 > last_label_x {
                break;
            }
            frame.render_widget(
                Paragraph::new(clock_stamp(history.entries[start + offset].last_seen))
                    .style(Style::default().fg(MUTED)),
                Rect::new(x, axis_y, 8, 1),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn usage(id: &str, prompt: u64) -> providers::RequestUsage {
        providers::RequestUsage {
            provider: "oMLX".into(),
            model: "model".into(),
            id: id.into(),
            prompt,
            cached: None,
            output: None,
            completed: false,
            ttft_ms: None,
            observed_at: None,
        }
    }

    #[test]
    fn polls_update_one_bar_and_absent_requests_remain() {
        let mut history = History::default();
        history.observe(&[usage("a", 12000), usage("b", 20000)]);
        let mut updated = usage("a", 12000);
        updated.output = Some(80);
        history.observe(&[updated]);
        history.observe(&[]);
        assert_eq!(history.len(), 2);
        assert_eq!(history.entries[0].usage.output, Some(80));
        assert_eq!(history.entries[1].usage.prompt, 20000);
        assert_eq!(history.entries[0].number, 1);
    }

    #[test]
    fn history_is_bounded_and_ids_are_scoped_to_model_and_provider() {
        let mut history = History::default();
        let first = usage("a", 10);
        let mut second = first.clone();
        second.provider = "Ollama".into();
        let mut third = first.clone();
        third.model = "another".into();
        history.observe(&[first, second, third]);
        assert_eq!(history.len(), 3);
        for n in 0..300 {
            history.observe(&[usage(&n.to_string(), n)]);
        }
        assert_eq!(history.len(), HISTORY_LIMIT);
        assert_eq!(history.entries.back().unwrap().number, 303);
    }
    #[test]
    fn comparison_is_explicit_and_handles_zero_and_provider_changes() {
        let mut history = History::default();
        history.observe(&[usage("previous", 22710)]);
        assert_eq!(
            compact_comparison(&usage("latest", 20055), history.entries.back()),
            "Δ −2,655 (−11.7%) · PREVIOUS OBSERVED 22,710"
        );
        let mut other = usage("other", 20055);
        other.model = "different".into();
        assert!(
            compact_comparison(&other, history.entries.back()).contains("different provider/model")
        );
        history.observe(&[usage("zero", 0)]);
        assert_eq!(
            compact_comparison(&usage("new", 10), history.entries.back()),
            "Δ +10 · PREVIOUS OBSERVED 0"
        );
    }

    #[test]
    fn fixed_trend_scale_does_not_change_with_outliers() {
        let before = chart_value(20055);
        assert_eq!(chart_value(u64::MAX), TREND_CEILING);
        assert_eq!(chart_value(TREND_CEILING), TREND_CEILING);
        assert_eq!(chart_value(0), 0);
        assert_eq!(chart_value(20055), before);
    }

    #[test]
    fn live_requires_fresh_membership_and_file_history_keeps_its_age() {
        let now = SystemTime::now();
        let mut request = usage("active", 20055);
        request.observed_at = Some(now);
        let mut history = History::default();
        history.observe(&[request.clone()]);
        let mut sample = Sample {
            llm_source: TelemetrySource::Live,
            llm_status: "generating".into(),
            llm_observed_at: Some(now),
            llm_requests: vec![request.clone()],
            ..Sample::default()
        };
        let entry = history.entries.back().unwrap();
        assert!(is_live(entry, &sample, now));
        assert!(!is_live(entry, &sample, now + Duration::from_secs(6)));
        sample.llm_requests.clear();
        assert!(!is_live(entry, &sample, now));
        request.completed = true;
        request.observed_at = Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000));
        history.observe(&[request.clone()]);
        history.observe(&[request]);
        assert_eq!(
            history.entries.back().unwrap().last_seen,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1700000000)
        );
    }
    #[test]
    fn operator_insights_need_evidence_and_keep_model_boundaries() {
        let mut history = History::default();
        for (i, prompt) in [20000, 21000, 20500, 20055].into_iter().enumerate() {
            history.observe(&[usage(&i.to_string(), prompt)]);
        }
        assert!(insight(&history, 3).title.starts_with("TYPICAL INPUT"));
        history.observe(&[usage("jump", 40000)]);
        assert!(insight(&history, 4).title.starts_with("PROMPT JUMP"));
        assert_eq!(insight(&history, 4).tone, Tone::Yellow);
        let mut other = usage("other", 100000);
        other.model = "different".into();
        history.observe(&[other]);
        assert_eq!(insight(&history, 5).title, "BASELINE SAMPLING");
        assert!(!material_jump(
            &usage("small", 1000),
            Some(&history.entries[0])
        ));
    }

    #[test]
    fn cache_metadata_does_not_inflate_short_bars() {
        let mut history = History::default();
        let unknown = usage("unknown", 12000);
        let mut cached = usage("cached", 12000);
        cached.cached = Some(9000);
        history.observe(&[unknown, cached]);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(90, 8)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &history, &Sample::default(), 0))
            .unwrap();
        let buffer = terminal.backend().buffer();
        // Two equal prompts must have identical geometry even if one reports
        // cache reuse. In a partial cell, the background must stay empty.
        for y in 3..6 {
            assert_eq!(buffer[(87, y)].symbol(), buffer[(88, y)].symbol());
            assert_eq!(buffer[(88, y)].bg, PANEL);
            assert_ne!(buffer[(87, y)].fg, GREEN);
        }
        assert_eq!(buffer[(88, 5)].fg, GREEN);
    }

    #[test]
    fn full_width_history_keeps_timestamps_and_selection_at_supported_sizes() {
        let mut history = History::default();
        for i in 0..120 {
            let mut request = usage(&i.to_string(), 16_384 + i * 128);
            request.observed_at =
                Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + i * 5));
            request.completed = true;
            history.observe(&[request]);
        }
        for (width, height) in [(80, 8), (100, 8), (90, 8), (140, 7)] {
            let mut terminal =
                Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|f| draw(f, f.area(), &history, &Sample::default(), 0))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let visible_bars = (1..width - 1)
                .filter(|x| {
                    (3..height - 2)
                        .any(|y| buffer[(*x, y)].fg == BLUE && buffer[(*x, y)].symbol() != " ")
                })
                .count();
            assert_eq!(visible_bars, usize::from(width - 2).min(120));
            assert_eq!(buffer[(width - 2, height - 2)].symbol(), "▲");
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            for label in [
                "31,616 tokens",
                "REPORTED",
                "CACHE —",
                "22:23:15",
                "UTC",
                "PREVIOUS OBSERVED",
            ] {
                assert!(text.contains(label), "missing {label} at {width}x{height}");
            }
            terminal
                .draw(|f| draw(f, f.area(), &history, &Sample::default(), usize::MAX))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|c| c.symbol())
                .collect();
            assert!(text.contains("16,384 tokens"));
            assert!(text.contains("22:13:20"));
            assert!(!text.contains("31,616"));
        }
    }

    #[test]
    fn selection_survives_a_jump_and_overflow_and_zero_stays_zero() {
        let mut history = History::default();
        history.observe(&[
            usage("zero", 0),
            usage("small", 12_000),
            usage("overflow", 100_000),
        ]);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|f| draw(f, f.area(), &history, &Sample::default(), 0))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(78, 3)].symbol(), "↑");
        assert_eq!(buffer[(78, 6)].symbol(), "▲");
        assert_eq!(buffer[(76, 5)].symbol(), "·");
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        assert!(text.contains("100,000 tokens"));
        assert!(text.contains("HISTORY · ! PROMPT JUMP"));
        assert!(text.contains("Inspect context/tool results."));
    }

    #[test]
    fn full_cell_stacks_preserve_both_segments_and_timestamp_rollover_is_explicit() {
        let mut request = usage("cached", TREND_CEILING);
        request.cached = Some(TREND_CEILING * 3 / 4);
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(1, 3)).unwrap();
        terminal
            .draw(|f| draw_bar(f, f.area(), &request, BLUE, false))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].fg, GREEN);
        assert_eq!(buffer[(0, 0)].bg, BLUE);
        assert_eq!(buffer[(0, 2)].fg, GREEN);
        assert_eq!(
            clock_stamp(SystemTime::UNIX_EPOCH + Duration::from_secs(86_399)),
            "23:59:59"
        );
        assert_eq!(
            clock_stamp(SystemTime::UNIX_EPOCH + Duration::from_secs(86_400)),
            "00:00:00"
        );
        assert_eq!(
            clock_stamp(SystemTime::UNIX_EPOCH - Duration::from_secs(1)),
            "--:--:--"
        );
    }

    #[test]
    fn colored_chart_and_operator_insights_render_together() {
        let now = SystemTime::now();
        let mut history = History::default();
        for (i, prompt) in [10000, 12000, 20000, 21000].into_iter().enumerate() {
            history.observe(&[usage(&i.to_string(), prompt)]);
        }
        let mut current = usage("live", 40000);
        current.cached = Some(36000);
        current.observed_at = Some(now);
        history.observe(&[current.clone()]);
        let sample = Sample {
            llm_source: TelemetrySource::Live,
            llm_status: "generating".into(),
            llm_observed_at: Some(now),
            llm_requests: vec![current],
            ..Sample::default()
        };
        let backend = ratatui::backend::TestBackend::new(180, 7);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &history, &sample, 0))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for color in [BLUE, CYAN, YELLOW, GREEN] {
            assert!(
                buffer.content.iter().any(|cell| cell.fg == color),
                "missing semantic color {color:?}"
            );
        }
        let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
        for label in [
            "LIVE",
            "OBSERVED",
            "PROMPT JUMP",
            "CACHE 90%",
            "Recent median",
            "Inspect context/tool results.",
        ] {
            assert!(text.contains(label), "missing operator insight: {label}");
        }
    }
}
