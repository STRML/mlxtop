// SPDX-License-Identifier: MIT
use super::*;
use ratatui::backend::TestBackend;
use ratatui::Terminal;

const GIB: u64 = 1024 * 1024 * 1024;

fn sample() -> Sample {
    Sample {
        vm_available: true,
        total_memory: 32 * GIB,
        wired: 4 * GIB,
        anonymous: 16 * GIB,
        compressor: 4 * GIB,
        file_backed: 4 * GIB,
        ..Sample::default()
    }
}

fn row(sample: &Sample, width: u16) -> (String, Vec<Color>, String) {
    let mut terminal = Terminal::new(TestBackend::new(width, 2)).unwrap();
    terminal
        .draw(|frame| {
            draw(frame, frame.area(), sample);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    let text = (0..width).map(|x| buffer[(x, 0)].symbol()).collect();
    let legend: String = (0..width).map(|x| buffer[(x, 1)].symbol()).collect();
    (
        text,
        (0..width).map(|x| buffer[(x, 0)].fg).collect(),
        legend,
    )
}

#[test]
fn composition_partitions_ram_and_never_exceeds_it() {
    assert_eq!(
        segments(&sample()),
        Some([4 * GIB, 16 * GIB, 4 * GIB, 4 * GIB, 4 * GIB])
    );
    // Overlapping counters are clamped instead of overflowing physical RAM.
    let mut oversized = sample();
    oversized.anonymous = 40 * GIB;
    assert_eq!(segments(&oversized), Some([4 * GIB, 28 * GIB, 0, 0, 0]));
    assert_eq!(segments(&Sample::default()), None);
    let counters_missing = Sample {
        vm_available: true,
        total_memory: 32 * GIB,
        ..Sample::default()
    };
    assert_eq!(
        segments(&counters_missing),
        None,
        "never report all RAM as free"
    );

    let (bar, colors, legend) = row(&sample(), 32);
    assert!(legend.contains("wired 4.0 GiB"), "{legend}");
    assert_eq!(bar.chars().count(), 32, "every column is assigned once");
    assert_eq!(&bar[..12], "████");
    assert_eq!(colors[0], BLUE);
    assert_eq!(colors[4], CYAN);
    assert_eq!(bar.chars().filter(|c| *c == '▓').count(), 4);
    assert_eq!(bar.chars().filter(|c| *c == '░').count(), 4);
}

#[test]
fn gpu_limit_marker_turns_yellow_with_a_reason_when_wired_memory_exceeds_it() {
    let mut sample = sample();
    sample.mlx.recommended_working_set = Some(24 * GIB);
    let (bar, colors, _) = row(&sample, 32);
    assert_eq!(bar.chars().nth(24), Some('┃'));
    assert_eq!(colors[24], Color::White);
    sample.wired = 26 * GIB;
    let (_, colors, _) = row(&sample, 32);
    assert_eq!(colors[24], YELLOW, "worth inspecting, not critical");
    let reason: String = legend(&sample, 120, 2)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert!(reason.contains("wired over GPU limit"), "{reason}");
    let wide: Vec<String> = legend(&sample, 120, 1)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(wide.len(), 1);
    assert!(wide[0].contains("GPU limit 24.0 GiB"), "{wide:?}");
    // An explicit iogpu.wired_limit_mb wins over the runtime's estimate.
    sample.metal.resource_limit = Some(28 * GIB);
    assert_eq!(gpu_limit(&sample), Some(28 * GIB));
    // Narrow legends flow onto more rows, then drop whole entries.
    let flowing: Vec<String> = legend(&sample, 32, 3)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(flowing.len(), 2, "{flowing:?}");
    assert!(flowing[1].contains("GPU limit 28.0 GiB"), "{flowing:?}");
    assert!(flowing[0].starts_with("█wired 26.0 GiB"), "{flowing:?}");
    assert!(flowing.iter().all(|line| line.chars().count() <= 32));
    let single: Vec<String> = legend(&sample, 32, 1)
        .iter()
        .map(|l| l.to_string())
        .collect();
    assert_eq!(single.len(), 1);
    assert!(!single[0].contains("free"), "{single:?}");
}

#[test]
fn missing_counters_say_so() {
    let mut terminal = Terminal::new(TestBackend::new(40, 2)).unwrap();
    terminal
        .draw(|frame| {
            assert_eq!(draw(frame, frame.area(), &Sample::default()), 1);
        })
        .unwrap();
    let text: String = (0..40)
        .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
        .collect();
    assert!(text.contains("RAM composition unavailable"));
    assert!(legend(&Sample::default(), 40, 2)[0]
        .to_string()
        .contains("unavailable"));
}
