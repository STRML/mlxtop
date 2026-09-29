// SPDX-License-Identifier: MIT
//! Separate measured problems from high resource use during normal work.
use super::*;

pub(super) struct Finding {
    pub title: String,
    pub evidence: String,
    pub context: String,
    pub next: &'static str,
    pub actionable: bool,
    pub tone: Tone,
}

pub(super) fn assess(sample: &Sample) -> Finding {
    let paging = if sample.swap_available && sample.vm_available && sample.rate_ready {
        rate(sample.swap_in.saturating_add(sample.swap_out))
    } else {
        "—".into()
    };
    let gpu = if sample.has_nvidia_gpus() {
        "GPU max"
    } else {
        "GPU"
    };
    let mut finding = Finding {
        title: "No bottleneck established".into(),
        evidence: format!("{gpu} {} · paging {paging}", percent_u8(sample.gpu_util)),
        context: format!(
            "Pressure {} · thermal {}",
            pressure_state_label(sample),
            sample.thermal
        ),
        next: "Waiting for a live throughput baseline.",
        actionable: false,
        tone: Tone::Muted,
    };

    // Host faults remain visible even without live serving telemetry.
    let impact = if sample.pressure == "RED" {
        "MEMORY BOTTLENECK"
    } else {
        sample.impact.as_str()
    };
    let action = match impact {
        "MEMORY BOTTLENECK" | "MEMORY STRESS" => Some("Free memory; stop unused models or apps."),
        "SWAP THRASHING" | "HEAVY PAGING" => Some("Reduce active models; recheck swap-out."),
        "PAGE-IN RECOVERY" => Some("Let page-ins settle before adding work."),
        "PAGING ACTIVE" | "WATCH PAGING" => Some("If paging persists, reduce concurrency."),
        "COMPRESSION ACTIVE" => Some("Check whether paging also rises."),
        "THERMAL LIMIT" => Some("Reduce concurrency; recheck thermals."),
        _ => None,
    };
    if let Some(action) = action {
        finding.title = impact.into();
        finding.next = action;
        finding.actionable = true;
        finding.tone = if sample.pressure == "RED" {
            Tone::Red
        } else {
            sample.impact_tone
        };
        if sample.impact == "COMPRESSION ACTIVE" {
            finding.evidence = format!(
                "Compression {} · paging {paging}",
                rate(sample.compress.saturating_add(sample.decompress))
            );
        }
        return finding;
    }
    if sample.impact == "SAMPLING" || !sample.rate_ready {
        finding.title = "Collecting system baseline".into();
        finding.next = "Wait for the next sample.";
        return finding;
    }
    if sample.impact == "DATA LIMITED" {
        finding.title = "System counters incomplete".into();
        finding.next = "Restore missing counters before tuning.";
        finding.actionable = true;
        return finding;
    }

    // Historical/reported rates must not become a current slowdown diagnosis.
    let fresh = sample.llm_source == TelemetrySource::Live
        && sample.llm_status != "stale"
        && sample.llm_observed_at.is_some_and(|at| {
            SystemTime::now()
                .duration_since(at)
                .is_ok_and(|age| age <= Duration::from_secs(5))
        });
    let live = fresh && sample.llm_generation_tps_live;
    if live && sample.correlation.is_material_drop() {
        finding.title = "Generation slowed".into();
        finding.evidence = sample
            .correlation
            .summary
            .split(" · ")
            .next()
            .unwrap_or("")
            .to_owned();
        let evidence = correlation_evidence_label(sample);
        finding.context =
            if sample.correlation.cause == CorrelationCause::Runtime || evidence.is_empty() {
                "No matching system signal".into()
            } else {
                format!(
                    "WITH {evidence} · {} confidence",
                    sample.correlation.confidence_label()
                )
            };
        finding.next = match sample.correlation.cause {
            CorrelationCause::Paging => "Reduce active models; recheck paging.",
            CorrelationCause::Compression => "Check whether paging also rises.",
            CorrelationCause::MemoryPressure
            | CorrelationCause::ModelMemory
            | CorrelationCause::MetalMemory => "Free memory; recheck generation speed.",
            CorrelationCause::Thermal => "Reduce concurrency; recheck thermals.",
            CorrelationCause::Queueing | CorrelationCause::GpuSaturation => {
                "Compare speed with one active request."
            }
            CorrelationCause::ContextGrowth => "Compare a request with a shorter prompt.",
            CorrelationCause::Runtime | CorrelationCause::None => {
                "Compare prompt size and runtime settings."
            }
        };
        finding.actionable = true;
        finding.tone = sample.correlation.tone();
        return finding;
    }
    if live
        && sample
            .llm_waiting_requests
            .is_some_and(|waiting| waiting > 0)
    {
        finding.title = format!("{} requests waiting", count(sample.llm_waiting_requests));
        finding.context = format!(
            "{} active · {}",
            count(sample.llm_active_requests),
            llm_generation_rate_label(sample)
        );
        finding.next = "Reduce concurrency; recheck queue length.";
        finding.actionable = true;
        finding.tone = Tone::Yellow;
        return finding;
    }
    if sample.gpu_util.is_some_and(|value| value >= 80) {
        finding.title = "GPU busy · bottleneck unconfirmed".into();
    }
    if live
        && matches!(
            sample.correlation.direction,
            ThroughputDirection::Flat | ThroughputDirection::Up
        )
    {
        finding.next = "No generation slowdown measured.";
    } else if !live {
        finding.next = "Live generation rate unavailable.";
    }
    finding
}

pub(super) fn draw(frame: &mut Frame, area: Rect, sample: &Sample) {
    let finding = assess(sample);
    render_card(
        frame,
        area,
        Line::from(Span::styled(
            " DIAGNOSIS ",
            Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
        )),
        vec![
            Line::from(Span::styled(
                finding.title,
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from(Span::styled(
                finding.evidence,
                Style::default().fg(finding.tone.color()),
            )),
            Line::from(Span::styled(finding.context, Style::default().fg(MUTED))),
            Line::from(vec![
                Span::styled(
                    if finding.actionable {
                        "CHECK  "
                    } else {
                        "NOTE  "
                    },
                    Style::default().fg(MUTED),
                ),
                Span::styled(
                    finding.next,
                    Style::default().fg(if finding.actionable {
                        finding.tone.color()
                    } else {
                        MUTED
                    }),
                ),
            ]),
        ],
        finding.tone,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy_sample() -> Sample {
        Sample {
            impact: "GPU BUSY".into(),
            gpu_util: Some(99),
            pressure: "GREEN".into(),
            thermal: "no warning".into(),
            rate_ready: true,
            vm_available: true,
            swap_available: true,
            llm_source: TelemetrySource::Live,
            llm_generation_tps: Some(24.0),
            llm_generation_tps_live: true,
            llm_observed_at: Some(SystemTime::now()),
            ..Sample::default()
        }
    }

    #[test]
    fn high_gpu_usage_is_evidence_not_a_slowdown_or_alarm() {
        let mut sample = busy_sample();
        sample.correlation.direction = ThroughputDirection::Flat;
        let finding = assess(&sample);
        assert!(!finding.actionable);
        assert!(finding.title.contains("unconfirmed"));
        assert!(finding.evidence.contains("99%"));
        assert_eq!(finding.next, "No generation slowdown measured.");
        assert_eq!(
            ChartMetric::Gpu.tone(100, Thresholds::default()),
            Tone::Blue
        );
    }

    #[test]
    fn measured_slowdown_keeps_rate_change_and_conditional_guidance() {
        let mut sample = busy_sample();
        sample.correlation = CorrelationInsight {
            direction: ThroughputDirection::Down,
            cause: CorrelationCause::GpuSaturation,
            confidence: 72,
            summary: "GEN ↓20.0% (30.0→24.0 tok/s) · correlated: GPU 99% busy".into(),
            ..CorrelationInsight::default()
        };
        for width in [46, 60, 80] {
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(width, 7)).unwrap();
            terminal
                .draw(|frame| draw(frame, frame.area(), &sample))
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for label in [
                "Generation slowed",
                "30.0→24.0 tok/s",
                "medium confidence",
                "one active",
                "request.",
            ] {
                assert!(text.contains(label), "missing {label} at {width}: {text}");
            }
            assert!(!text.contains('…'));
        }
        sample.llm_status = "stale".into();
        assert!(!assess(&sample).actionable);
        assert_eq!(assess(&sample).next, "Live generation rate unavailable.");
    }

    #[test]
    fn missing_live_rates_do_not_hide_critical_memory_pressure() {
        let mut sample = Sample {
            pressure: "RED".into(),
            impact: "DATA LIMITED".into(),
            ..Sample::default()
        };
        let finding = assess(&sample);
        assert_eq!(finding.title, "MEMORY BOTTLENECK");
        assert_eq!(finding.tone, Tone::Red);
        assert!(finding.actionable);
        sample.pressure = "UNKNOWN".into();
        sample.rate_ready = true;
        assert_eq!(assess(&sample).title, "System counters incomplete");
    }
}
