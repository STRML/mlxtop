// SPDX-License-Identifier: MIT
use super::*;

fn live() -> Sample {
    Sample {
        llm_source: TelemetrySource::Live,
        llm_status: "idle".into(),
        llm_provider: "oMLX".into(),
        llm_observed_at: Some(SystemTime::now()),
        llm_active_requests: Some(0),
        llm_waiting_requests: Some(0),
        ..Sample::default()
    }
}

#[test]
fn queue_keeps_idle_zero_but_gaps_stale_and_reported_values() {
    let mut history = History::default();
    let mut sample = live();
    history.observe(&sample, 10);
    assert_eq!(history.points.back().unwrap().active, Some(0));
    sample.llm_observed_at = Some(SystemTime::now() - Duration::from_secs(20));
    history.observe(&sample, 10);
    assert_eq!(history.points.back().unwrap().active, None);
    sample.llm_observed_at = Some(SystemTime::now());
    sample.llm_source = TelemetrySource::Report;
    history.observe(&sample, 10);
    assert_eq!(history.points.back().unwrap().waiting, None);
    sample.llm_source = TelemetrySource::Live;
    sample.llm_waiting_requests = None;
    history.observe(&sample, 2);
    assert_eq!(history.points.len(), 2);
    assert_eq!(history.points.back().unwrap().waiting, None);
}

#[test]
fn latency_requires_explicit_measurement_and_deduplicates_requests() {
    let mut sample = live();
    sample.llm_requests.push(providers::RequestUsage {
        provider: "oMLX".into(),
        model: "test".into(),
        id: "one".into(),
        prompt: 100,
        cached: None,
        output: None,
        completed: true,
        observed_at: Some(SystemTime::now()),
        ttft_ms: None,
        output_tps: None,
    });
    let mut history = History::default();
    history.observe(&sample, 10);
    assert!(!history.has_latency());
    sample.llm_requests[0].ttft_ms = Some(0);
    history.observe(&sample, 10);
    history.observe(&sample, 10);
    assert!(history.has_latency());
    assert_eq!(history.timings.len(), 1);
    sample.llm_requests[0].model = "other".into();
    history.observe(&sample, 10);
    assert_eq!(history.timings.len(), 2);
}

#[test]
fn stepped_trace_uses_connected_corners_for_rises_and_falls() {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(3, 5)).unwrap();
    terminal
        .draw(|frame| {
            trace(
                frame,
                frame.area(),
                &[Some(0), Some(16), Some(0)],
                &[false; 3],
                16,
                CYAN,
                1,
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(1, 4)].symbol(), "┛");
    assert_eq!(buffer[(1, 0)].symbol(), "┏");
    assert_eq!(buffer[(2, 0)].symbol(), "┓");
    assert_eq!(buffer[(2, 4)].symbol(), "┗");
}

#[test]
fn zero_queue_series_share_labeled_baseline_and_keep_border_intact() {
    let mut history = History::default();
    for _ in 0..40 {
        history.observe(&live(), 80);
    }
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(44, 10)).unwrap();
    terminal
        .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), 1))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(40, 8)].symbol(), "═");
    assert_eq!(buffer[(40, 8)].fg, Color::White);
    assert_eq!(buffer[(1, 8)].symbol(), "0");
    assert!((2..8).all(|y| buffer[(40, y)].symbol() == " "));
    assert_eq!(buffer[(1, 9)].symbol(), "─");
}

#[test]
fn unequal_queue_counts_use_one_scale_and_missing_series_is_not_overlap() {
    let mut history = History::default();
    let mut sample = live();
    sample.llm_active_requests = Some(8);
    for _ in 0..40 {
        history.observe(&sample, 80);
    }
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(44, 10)).unwrap();
    terminal
        .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), 1))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(40, 3)].fg, CYAN);
    assert_eq!(buffer[(40, 8)].fg, YELLOW);
    history = History::default();
    sample.llm_active_requests = None;
    for _ in 0..40 {
        history.observe(&sample, 80);
    }
    terminal
        .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), 1))
        .unwrap();
    assert_eq!(terminal.backend().buffer()[(40, 8)].symbol(), "━");
    assert_eq!(terminal.backend().buffer()[(40, 8)].fg, YELLOW);
}

#[test]
fn queue_large_ticks_fit_their_gutter_without_truncating_or_covering_the_trace() {
    for value in [2_000, 20_000, u64::MAX] {
        let mut history = History::default();
        let mut sample = live();
        sample.llm_active_requests = Some(value);
        for _ in 0..44 {
            history.observe(&sample, 80);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(44, 10)).unwrap();
        terminal
            .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), 1))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (gutter, ceiling, _) = queue_axis(&history, 42, 1);
        let label = queue_tick(ceiling);
        let rendered: String = (1..=gutter).map(|x| buffer[(x, 2)].symbol()).collect();
        assert_eq!(rendered.trim_end(), label);
        assert!(
            gutter as usize > label.len(),
            "leave a gap before the trace"
        );
        assert!((2..8).any(|y| buffer[(42, y)].fg == CYAN));
        assert_eq!(buffer[(42, 8)].fg, YELLOW);
    }
}

#[test]
fn queue_range_and_time_window_use_the_actual_graph_width_after_reserving_ticks() {
    for (zoom, recent) in [(1, 24), (2, 12)] {
        let mut history = History::default();
        let mut sample = live();
        sample.llm_active_requests = Some(10_000);
        history.observe(&sample, 80);
        sample.llm_active_requests = Some(1);
        for _ in 0..recent {
            history.observe(&sample, 80);
        }
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(30, 10)).unwrap();
        terminal
            .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), zoom))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let (gutter, ceiling, visible) = queue_axis(&history, 28, zoom);
        assert_eq!(visible, (28 - gutter).div_ceil(zoom) as usize);
        assert_eq!(ceiling, 2, "off-screen spike must not flatten the trace");
        let subtitle: String = (1..29).map(|x| buffer[(x, 1)].symbol()).collect();
        assert!(subtitle.contains("0–2 req · auto"), "{subtitle}");
        assert!(subtitle.contains(&format!("{visible}s")), "{subtitle}");
        assert_eq!(buffer[(28, 5)].fg, CYAN);
    }
}

#[test]
fn compact_queue_keeps_exact_counts_when_there_is_no_room_for_a_scale() {
    let mut history = History::default();
    let mut sample = live();
    sample.llm_active_requests = Some(1234);
    sample.llm_waiting_requests = Some(5678);
    history.observe(&sample, 80);
    for height in [2, 3, 4, 5] {
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(20, height)).unwrap();
        terminal
            .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), 1))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text = (0..height)
            .map(|y| (0..20).map(|x| buffer[(x, y)].symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("active 1234"), "height {height}: {text}");
        assert!(text.contains("waiting 5678"), "height {height}: {text}");
        assert!(text.contains("queue"), "height {height}: {text}");
        assert!(!text.contains("0–"), "a compact summary has no fake axis");
        if height >= 5 {
            assert!(text.contains("Enter: history"));
        }
    }
}

#[test]
fn six_row_queue_distinguishes_one_request_from_the_two_request_tick() {
    let mut history = History::default();
    let mut sample = live();
    sample.llm_active_requests = Some(1);
    sample.llm_waiting_requests = Some(0);
    history.observe(&sample, 80);
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(50, 6)).unwrap();
    terminal
        .draw(|frame| queue(frame, frame.area(), &history, Duration::from_secs(1), 1))
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(1, 2)].symbol(), "2");
    assert_ne!(buffer[(48, 2)].fg, CYAN);
    assert_eq!(buffer[(48, 3)].fg, CYAN);
    assert_eq!(buffer[(48, 4)].fg, YELLOW);
}

#[test]
fn footprint_labels_bytes_current_process_and_peak_without_clipping() {
    let mut history = History::default();
    let mut sample = live();
    sample.process_memory = Some(process_memory::Reading {
        pid: 41441,
        started: 1,
        resident: 16 * 1024 * MIB,
        footprint: 19 * 1024 * MIB,
        peak: 30 * 1024 * MIB,
        at: Instant::now(),
    });
    sample.process_memory_growth = Some(-(MIB as i64));
    for _ in 0..80 {
        history.observe(&sample, 120);
    }
    for (width, height) in [(24, 4), (24, 6), (30, 9), (54, 12)] {
        let text = super::super::tests::render_view(width, height, |frame| {
            footprint(frame, frame.area(), &history, &sample, 1);
        });
        for label in ["19.0 GiB", "PID 41441", "peak 30.0 GiB"] {
            assert!(
                text.contains(label),
                "missing {label} at {width}x{height}\n{text}"
            );
        }
        assert_eq!(text.matches("PID 41441").count(), 1);
        if height >= 6 {
            let (_, low, high) = footprint_axis(&history, width - 2, 1);
            assert!(low > 0 && low < 19 * 1024 * MIB && high > 19 * 1024 * MIB);
            assert!(text.contains(&bytes(low)), "low tick missing\n{text}");
            assert!(text.contains(&bytes(high)), "high tick missing\n{text}");
        }
        assert_eq!(text.contains("growth -1.0 MiB/s"), width >= 54);
    }
}

#[test]
fn footprint_range_ignores_offscreen_peaks_and_keeps_missing_samples_empty() {
    let mut history = History::default();
    history.points.push_back(Point {
        footprint: Some(100 * 1024 * MIB),
        ..Point::default()
    });
    for _ in 0..20 {
        history.points.push_back(Point {
            footprint: Some(19 * 1024 * MIB),
            ..Point::default()
        });
    }
    let (_, _, wide_high) = footprint_axis(&history, 50, 1);
    let (_, _, recent_high) = footprint_axis(&history, 50, 4);
    assert!(wide_high > 100 * 1024 * MIB);
    assert!(recent_high < 25 * 1024 * MIB);
    let text = super::super::tests::render_view(54, 10, |frame| {
        footprint(frame, frame.area(), &History::default(), &live(), 1);
    });
    assert!(text.contains("No footprint samples"));
    assert!(text.contains("OS unavailable"));
    assert!(!text.contains("0.0 GiB"));
}

#[test]
fn narrow_queue_preserves_both_counts_and_complete_window_labels() {
    for (active, waiting) in [(1, 0), (1234, 5678)] {
        let mut sample = live();
        sample.llm_active_requests = Some(active);
        sample.llm_waiting_requests = Some(waiting);
        let mut history = History::default();
        for _ in 0..40 {
            history.observe(&sample, 80);
        }
        let text = super::super::tests::render_view(20, 8, |frame| {
            queue(frame, frame.area(), &history, Duration::from_secs(1), 1);
        });
        assert!(text.contains(&format!("active {active}")), "{text}");
        assert!(text.contains(&format!("waiting {waiting}")), "{text}");
        if active == 1 {
            assert!(text.contains("0–2 req · 15s"), "{text}");
        }
    }
}

#[test]
fn trace_preserves_gaps_boundaries_and_overflow() {
    let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(5, 5)).unwrap();
    terminal
        .draw(|frame| {
            trace(
                frame,
                frame.area(),
                &[Some(0), None, Some(16), Some(0), Some(u64::MAX)],
                &[false, false, false, true, false],
                16,
                CYAN,
                1,
            )
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer[(1, 2)].symbol(), " ");
    assert_eq!(buffer[(3, 2)].symbol(), " ");
    assert_eq!(buffer[(4, 0)].symbol(), "↑");
}
