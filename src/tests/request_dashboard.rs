// SPDX-License-Identifier: MIT
use super::*;

const TREND_CEILING: u64 = 65_536;
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
        output_tps: None,
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
fn selected_prompt_keeps_its_own_output_speed_live_historical_and_completed() {
    let now = SystemTime::now();
    let mut first = usage("a", 12000);
    first.output = Some(160);
    first.output_tps = Some(24.5);
    first.observed_at = Some(now);
    let mut second = usage("b", 20000);
    second.output = Some(300);
    second.output_tps = Some(38.0);
    second.observed_at = Some(now);
    let mut history = History::default();
    history.observe(&[first.clone(), second.clone()]);
    let mut sample = Sample {
        llm_provider: "oMLX".into(),
        llm_model: "model".into(),
        llm_source: TelemetrySource::Live,
        llm_status: "generating".into(),
        llm_observed_at: Some(now),
        llm_requests: vec![first.clone(), second],
        llm_generation_tps: Some(99.0),
        ..Sample::default()
    };
    let render = |history: &History, sample: &Sample, scroll, width, height| {
        crate::tests::render_view(width, height, |frame| {
            draw(frame, frame.area(), history, sample, scroll, 1)
        })
    };
    for (width, height) in [(39, 6), (40, 7), (50, 8), (85, 8), (170, 30)] {
        let newest = render(&history, &sample, 0, width, height);
        assert!(newest.contains("OUT 300 · LIVE 38.0 tok/s"), "{newest}");
        let older = render(&history, &sample, 1, width, height);
        assert!(older.contains("OUT 160 · LIVE 24.5 tok/s"), "{older}");
        assert!(!older.contains("38.0 tok/s"));
        assert!(!older.contains("99.0 tok/s"));
    }
    sample.llm_requests.clear();
    sample.llm_status = "idle".into();
    assert!(render(&history, &sample, 1, 85, 8).contains("LAST 24.5 tok/s"));
    // A completion without final timing cannot promote a sampled rate to AVG.
    first.completed = true;
    first.output_tps = None;
    first.output = Some(200);
    first.observed_at = Some(now + Duration::from_secs(2));
    history.observe(&[first.clone()]);
    assert!(render(&history, &sample, 1, 85, 8).contains("OUT 200 · LAST 24.5 tok/s"));
    assert_eq!(history.entries[0].output_speed.unwrap().observed_at, now);
    first.output_tps = Some(25.0);
    history.observe(&[first]);
    assert!(render(&history, &sample, 1, 85, 8).contains("OUT 200 · AVG 25.0 tok/s"));
    // Missing request-specific throughput never borrows the server average.
    history.observe(&[usage("unknown", 100)]);
    assert!(render(&history, &sample, 0, 40, 7).contains("OUT — · SPEED —"));
}

#[test]
fn missing_speed_updates_preserve_last_measured_age_and_model_scope() {
    let now = SystemTime::now();
    let mut request = usage("same-id", 100);
    request.output_tps = Some(0.0);
    request.observed_at = Some(now - Duration::from_secs(20));
    let mut history = History::default();
    history.observe(&[request.clone()]);
    request.observed_at = Some(now);
    for invalid in [None, Some(f64::NAN), Some(-1.0), Some(f64::INFINITY)] {
        request.output_tps = invalid;
        history.observe(&[request.clone()]);
        let line = output_summary(&history.entries[0], true, 90).to_string();
        assert!(line.contains("LAST 0.0 tok/s · 20s old"), "{line}");
    }
    request.model = "another-model".into();
    request.output_tps = None;
    history.observe(&[request]);
    assert!(output_summary(&history.entries[1], true, 90)
        .to_string()
        .contains("SPEED —"));
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
    assert!(compact_comparison(&other, history.entries.back()).contains("different provider/model"));
    history.observe(&[usage("zero", 0)]);
    assert_eq!(
        compact_comparison(&usage("new", 10), history.entries.back()),
        "Δ +10 · PREVIOUS OBSERVED 0"
    );
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
fn selected_prompt_identifies_previous_model_without_losing_measurements() {
    let mut request = usage("old-request", 12_000);
    request.model = "previous-model".into();
    request.cached = Some(9_000);
    request.completed = true;
    request.observed_at = Some(SystemTime::now() - Duration::from_secs(120));
    let mut history = History::default();
    history.observe(&[request.clone()]);
    let mut sample = Sample {
        llm_provider: "oMLX".into(),
        llm_model: "current-model".into(),
        ..Sample::default()
    };
    let render = |history: &History, sample: &Sample, scroll, width, height| {
        let mut terminal =
            Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), history, sample, scroll, 1))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    };
    // Prompt context remains substantial at classic and wide terminals.
    for (width, height) in [(53, 10), (120, 20)] {
        let screen = render(&history, &sample, 0, width, height);
        for label in [
            "REQUEST oMLX · previous-model",
            "12,000 tokens",
            "REPORTED",
            "CACHE 75%",
            "2m old",
            "UTC",
            "12.0k▲",
        ] {
            assert!(
                screen.contains(label),
                "missing {label} at {width}x{height}"
            );
        }
        assert!(!screen.contains("current-model"));
    }

    // Matching SYSINFO identity needs no repeated model metadata.
    sample.llm_model = request.model.clone();
    assert!(!render(&history, &sample, 0, 80, 6).contains("REQUEST oMLX"));
    // The provider is part of the identity, even if model names match.
    sample.llm_provider = "Ollama".into();
    assert!(render(&history, &sample, 0, 80, 6).contains("REQUEST oMLX · previous-model"));

    let mut newest = usage("current-request", 20_000);
    newest.model = "current-model".into();
    history.observe(&[newest]);
    sample.llm_provider = "oMLX".into();
    sample.llm_model = "current-model".into();
    assert!(!render(&history, &sample, 0, 80, 6).contains("REQUEST oMLX"));
    let older = render(&history, &sample, 1, 80, 6);
    assert!(older.contains("REQUEST oMLX · previous-model"));
    assert!(older.contains("12,000 tokens"));
    assert!(older.contains("12.0k▲"));
}

#[test]
fn recent_sizes_keep_model_boundaries_and_ignore_future_requests() {
    let mut history = History::default();
    for (i, prompt) in [20000, 21000, 20500, 20055, 40000].into_iter().enumerate() {
        history.observe(&[usage(&i.to_string(), prompt)]);
    }
    let summary = size_summary(&history, 3, 100).to_string();
    assert!(summary.contains("RECENT 4"));
    assert!(summary.contains("MEDIAN 20,277.5 tokens"));
    assert!(summary.contains("RANGE 20,000–21,000"));
    assert!(!summary.contains("40,000"));
    let mut other = usage("other", 100000);
    other.model = "different".into();
    history.observe(&[other]);
    assert_eq!(
        size_summary(&history, 5, 100).to_string(),
        " RECENT 1 · newest at right"
    );
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
        .draw(|frame| {
            frame.render_widget(
                Block::default().style(Style::default().bg(PANEL)),
                frame.area(),
            );
            draw_bar(
                frame,
                Rect::new(75, 3, 1, 3),
                &history.entries[0].usage,
                BLUE,
                TREND_CEILING,
            );
            draw_bar(
                frame,
                Rect::new(82, 3, 1, 3),
                &history.entries[1].usage,
                BLUE,
                TREND_CEILING,
            );
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    // Two equal prompts must have identical geometry even if one reports
    // cache reuse. In a partial cell, the background must stay empty.
    for y in 3..6 {
        assert_eq!(buffer[(75, y)].symbol(), buffer[(82, y)].symbol());
        assert_eq!(buffer[(82, y)].bg, PANEL);
        assert_ne!(buffer[(75, y)].fg, GREEN);
    }
    assert_eq!(buffer[(82, 5)].fg, GREEN);
}

#[test]
fn every_visible_bar_has_a_size_at_supported_sizes() {
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
            .draw(|f| draw(f, f.area(), &history, &Sample::default(), 0, 1))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let visible_columns = (1..width - 1)
            .filter(|x| {
                (3..height - 2)
                    .any(|y| buffer[(*x, y)].fg == BLUE && buffer[(*x, y)].symbol() != " ")
            })
            .count();
        let visible = usize::from((width - 2) / 7);
        assert_eq!(visible_columns, visible * 5);
        let labels: String = (1..width - 1)
            .map(|x| buffer[(x, height - 2)].symbol())
            .collect();
        let expected: Vec<_> = history
            .entries
            .iter()
            .skip(120 - visible)
            .map(|entry| compact_tokens(entry.usage.prompt))
            .collect();
        assert_eq!(
            labels
                .replace('▲', " ")
                .split_whitespace()
                .collect::<Vec<_>>(),
            expected,
            "each bar needs its own readable size at {width}x{height}"
        );
        assert_eq!(buffer[(width - 3, height - 2)].symbol(), "▲");
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
            .draw(|f| draw(f, f.area(), &history, &Sample::default(), usize::MAX, 1))
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
        .draw(|f| draw(f, f.area(), &history, &Sample::default(), 0, 1))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_ne!(buffer[(71, 3)].symbol(), "↑");
    assert!((55..63).any(|x| buffer[(x, 5)].symbol() == "·"));
    assert!((55..63).all(|x| buffer[(x, 3)].symbol() == " "));
    let labels: String = (1..79).map(|x| buffer[(x, 6)].symbol()).collect();
    assert_eq!(
        labels.split_whitespace().collect::<Vec<_>>(),
        ["0", "12.0k", "100.0k▲"]
    );
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    assert!(text.contains("100,000 tokens"));
    assert!(text.contains("0–150,000 tokens · auto"));
    assert!(text.contains("MEDIAN 12,000 tokens"));
    assert!(!text.contains('!'));
}

#[test]
fn full_cell_stacks_preserve_both_segments_and_timestamp_rollover_is_explicit() {
    let mut request = usage("cached", TREND_CEILING);
    request.cached = Some(TREND_CEILING * 3 / 4);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(1, 3)).unwrap();
    terminal
        .draw(|f| draw_bar(f, f.area(), &request, BLUE, TREND_CEILING))
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
fn prompt_sizes_and_cache_render_without_growth_warnings() {
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
        .draw(|frame| draw(frame, frame.area(), &history, &sample, 0, 1))
        .unwrap();
    let buffer = terminal.backend().buffer();
    for color in [BLUE, CYAN, GREEN] {
        assert!(
            buffer.content.iter().any(|cell| cell.fg == color),
            "missing semantic color {color:?}"
        );
    }
    let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
    for label in [
        "LIVE",
        "OBSERVED",
        "40,000 tokens",
        "CACHE 90%",
        "4,000 uncached",
        "MEDIAN 20,000 tokens",
        "RANGE 10,000–40,000",
        "10.0k",
        "12.0k",
        "20.0k",
        "21.0k",
        "40.0k▲",
    ] {
        assert!(text.contains(label), "missing prompt measurement: {label}");
    }
    assert!(!text.contains('!'));
    assert!(!text.contains("PROMPT JUMP"));
    assert!(!buffer.content.iter().any(|cell| cell.fg == YELLOW));
}

#[test]
fn sparse_requests_stay_at_the_right_edge_with_fixed_spacing() {
    let mut history = History::default();
    for index in 0..3 {
        history.observe(&[usage(&index.to_string(), 2947)]);
        for width in [53, 80, 120, 180] {
            for zoom in [1, 2, 4, 8] {
                let mut terminal =
                    Terminal::new(ratatui::backend::TestBackend::new(width, 12)).unwrap();
                terminal
                    .draw(|frame| draw(frame, frame.area(), &history, &Sample::default(), 0, zoom))
                    .unwrap();
                let buffer = terminal.backend().buffer();
                let labels: String = (1..width - 1).map(|x| buffer[(x, 10)].symbol()).collect();
                assert!(labels.trim_end().ends_with("2.9k▲"));
                // The newest body and label stay in the last slot even
                // with a single request, at every supported zoom/width.
                let last_bar = (1..width - 1)
                    .rev()
                    .find(|x| {
                        (4..10).any(|y| {
                            buffer[(*x, y)].fg == BLUE
                                && matches!(
                                    buffer[(*x, y)].symbol(),
                                    "█" | "▁" | "▂" | "▃" | "▄" | "▅" | "▆" | "▇"
                                )
                        })
                    })
                    .unwrap();
                assert!(last_bar >= width - 3, "bar floats away from latest edge");
                if zoom == 1 {
                    let occupied_start = width - 1 - (index + 1) * 6;
                    assert!((1..occupied_start).all(|x| buffer[(x, 9)].symbol() == " "));
                }
            }
        }
    }
}

#[test]
fn zoom_changes_history_range_without_changing_selected_prompt_size() {
    let mut history = History::default();
    for index in 0..120 {
        let mut request = usage(&index.to_string(), 12_000 + index * 128);
        request.observed_at =
            Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000 + index));
        history.observe(&[request]);
    }
    for (zoom, first) in [(1, 109), (2, 115), (4, 118), (8, 119)] {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(80, 8)).unwrap();
        terminal
            .draw(|frame| draw(frame, frame.area(), &history, &Sample::default(), 0, zoom))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("27,232 tokens"));
        let labels: String = (1..79)
            .map(|x| terminal.backend().buffer()[(x, 6)].symbol())
            .collect();
        let expected: Vec<_> = history
            .entries
            .iter()
            .skip(first)
            .map(|entry| compact_tokens(entry.usage.prompt))
            .collect();
        assert_eq!(
            labels
                .replace('▲', " ")
                .split_whitespace()
                .collect::<Vec<_>>(),
            expected,
            "every bar must retain its size at {zoom}×"
        );
        assert!(labels.contains("27.2k▲"));
    }
}
