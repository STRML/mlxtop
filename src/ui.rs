// SPDX-License-Identifier: MIT
//! Rendering for Overview, Top, Journal and their overlays.
use crate::app::App;
use crate::chart_navigation::Chart;
use crate::chart_render::{
    chart_axis_label, chart_columns_for_plot, chart_display_value, chart_inactive_rate_label,
    chart_scale, chart_stat_label, chart_stats_for_width, chart_window_label,
    stretch_chart_columns, trace_connector, trace_point, TraceCell, TraceStyle,
};
use crate::cli::VERSION;
use crate::domain::{ChartMetric, ChartPoint, Tone};
use crate::formatting::{
    bytes, compact_label, gpu_load_label, llm_generation_rate_label, llm_prefill_rate_label,
    percent, pressure_state_label, rate, telemetry_age, telemetry_source,
};
use crate::processes::process_provider;
use crate::theme::{
    centered_rect, panel, CYAN, DIM, EDGE, MUTED, PANEL, PANEL_RAISED, RED, YELLOW,
};
use crate::{
    gpu_dashboard, model_dashboard, operator_charts, process_memory, request_dashboard, swap_usage,
};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block, Borders, Cell, Clear, Gauge, Paragraph, Row, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Table, TableState, Tabs, Wrap,
};
use ratatui::Frame;
use std::collections::VecDeque;
impl App {
    pub(crate) fn draw(&self, frame: &mut Frame) {
        let area = frame.area();
        self.charts.regions.borrow_mut().clear();
        self.charts.overview_samples.set(None);
        if area.width < 72 || area.height < 24 {
            self.draw_compact_warning(frame, area);
            return;
        }
        frame.render_widget(
            Block::default().style(Style::default().bg(Color::Rgb(10, 14, 21))),
            area,
        );
        let outer = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(1),
                Constraint::Min(5),
                Constraint::Length(1),
            ])
            .split(area);
        self.draw_header(frame, outer[0]);
        self.draw_controls(frame, outer[2]);
        match self.tab {
            0 => self.draw_overview(frame, outer[1]),
            1 => self.draw_llm_top(frame, outer[1]),
            _ => self.draw_journal(frame, outer[1]),
        }
        if self.tab == 0 {
            if self.charts.expanded {
                frame.render_widget(Clear, outer[1]);
                self.charts.overview_samples.set(None);
                self.draw_selected_chart(frame, outer[1]);
            }
            self.charts.decorate(frame);
        }
        if self.alert.is_some() {
            self.draw_alert_banner(frame, outer[1]);
        }
        if self.diagnostics_open {
            self.draw_diagnostics(frame, outer[1]);
        }
        if self.help {
            self.draw_help(frame, area);
        }
    }

    /// Overlay strip at the top of the tab content: a critical condition demands
    /// attention without stealing a permanent layout row from the panels.
    pub(crate) fn draw_alert_banner(&self, frame: &mut Frame, area: Rect) {
        let Some(alert) = &self.alert else {
            return;
        };
        let height = area.height.min(3);
        if height == 0 || area.width < 20 {
            return;
        }
        let banner = Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height,
        };
        frame.render_widget(Clear, banner);
        let text = Line::from(vec![
            Span::styled(
                format!(" ⚠ {} ", alert.state),
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                alert.summary.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  ·  raised {}  ·  a acknowledge", alert.time),
                Style::default().fg(MUTED),
            ),
        ]);
        frame.render_widget(
            Paragraph::new(text).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(RED))
                    .style(Style::default().bg(PANEL)),
            ),
            banner,
        );
    }

    pub(crate) fn draw_header(&self, frame: &mut Frame, area: Rect) {
        let compact_tabs = area.width < 96;
        let tab_labels = if compact_tabs {
            vec![
                Line::from("1 OVR"),
                Line::from("2 TOP"),
                Line::from("3 JRN"),
            ]
        } else {
            vec![
                Line::from("1 Overview"),
                Line::from("2 MLX Top"),
                Line::from("3 Journal"),
            ]
        };
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Length(24),
                Constraint::Length(if compact_tabs { 30 } else { 42 }),
                Constraint::Fill(1),
            ])
            .split(area);
        let title = Paragraph::new(Line::from(vec![
            Span::styled(
                " mlxtop ",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("v{VERSION}"),
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            ),
        ]));
        frame.render_widget(title, chunks[0]);

        let tabs = Tabs::new(tab_labels)
            .select(self.tab)
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(CYAN)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().fg(MUTED))
            .divider(Span::styled(" · ", Style::default().fg(DIM)));
        frame.render_widget(tabs, chunks[1]);
        frame.render_widget(
            Paragraph::new(format!(
                "{} · {}s ",
                if self.paused { "PAUSED" } else { "SAMPLING" },
                self.interval.as_secs()
            ))
            .alignment(Alignment::Right)
            .style(Style::default().fg(if self.paused { YELLOW } else { MUTED })),
            chunks[2],
        );
    }

    pub(crate) fn draw_request_chart(&self, frame: &mut Frame, area: Rect) {
        self.charts.register(Chart::Prompt, area);
        request_dashboard::draw(
            frame,
            area,
            &self.collector.request_history,
            &self.collector.current,
            self.request_scroll,
            self.charts.zoom(Chart::Prompt),
        );
    }

    pub(crate) fn draw_operator_chart(&self, frame: &mut Frame, area: Rect, chart: Chart) {
        self.charts.register(chart, area);
        let zoom = self.charts.zoom(chart);
        match chart {
            Chart::Queue => operator_charts::queue(
                frame,
                area,
                &self.collector.operator_history,
                self.interval,
                zoom,
                self.charts.overview_samples.get(),
            ),
            Chart::Latency => {
                operator_charts::latency(frame, area, &self.collector.operator_history, zoom)
            }
            _ => unreachable!("operator chart expected"),
        }
    }

    pub(crate) fn draw_selected_chart(&self, frame: &mut Frame, area: Rect) {
        match self.charts.focused {
            Chart::Prompt => self.draw_request_chart(frame, area),
            chart @ (Chart::Queue | Chart::Latency) => self.draw_operator_chart(frame, area, chart),
            chart => {
                let (name, history, metric) = match chart {
                    Chart::Generation => (
                        "generation",
                        &self.collector.generation_history,
                        ChartMetric::Generation,
                    ),
                    Chart::Prefill => (
                        "prefill",
                        &self.collector.prefill_history,
                        ChartMetric::Prefill,
                    ),
                    Chart::Cache => ("cache", &self.collector.cache_history, ChartMetric::Cache),
                    Chart::Gpu => (
                        self.gpu_chart_title(),
                        &self.collector.gpu_history,
                        ChartMetric::Gpu,
                    ),
                    Chart::Memory => ("memory", &self.collector.load_history, ChartMetric::Memory),
                    Chart::Paging => ("paging", &self.collector.swap_history, ChartMetric::Swap),
                    _ => unreachable!("indicator chart expected"),
                };
                self.render_indicator_chart(frame, area, name, history, metric);
            }
        }
    }

    pub(crate) fn draw_overview(&self, frame: &mut Frame, area: Rect) {
        if self.collector.current.has_nvidia_gpus() {
            self.draw_gpu_overview(frame, area);
        } else {
            self.draw_overview_layout(frame, area, false);
        }
    }

    pub(crate) fn draw_gpu_overview(&self, frame: &mut Frame, area: Rect) {
        self.draw_overview_layout(frame, area, true);
    }

    pub(crate) fn draw_overview_layout(&self, frame: &mut Frame, area: Rect, devices: bool) {
        let compact = area.height < 30;
        let info_height = 3;
        let device_height = if devices {
            gpu_dashboard::height(
                self.collector.current.gpus.len(),
                if compact { 4 } else { 6 },
            )
        } else {
            0
        };
        // Keep idle geometry stable, but give throughput and request bars enough
        // vertical resolution. Resources remain visible in a compact first row.
        let journal_height = if compact { 0 } else { 5 };
        let summary_height = 2;
        let request_height = if compact {
            if devices {
                6
            } else {
                7
            }
        } else {
            (area.height / 4).clamp(9, 13)
        };
        let remaining = area.height.saturating_sub(
            info_height + summary_height + device_height + journal_height + request_height,
        );
        let resource_height = (remaining / 2).clamp(4, 8).min(remaining);
        let rates_height = remaining.saturating_sub(resource_height);
        // Fit every captured observation into even the narrowest plot, then
        // widen those same observations across larger panels. No decimation.
        let resources =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
                .split(area);
        let rates = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ])
        .split(area);
        let supporting = Layout::horizontal([
            Constraint::Percentage(50),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
        let queue_width = operator_charts::queue_plot_width(
            &self.collector.operator_history,
            supporting[2].width,
        );
        let samples = [
            resources[0].width.saturating_sub(7),
            resources[1].width.saturating_sub(14),
            rates[0].width.saturating_sub(8),
            rates[1].width.saturating_sub(8),
            rates[2].width.saturating_sub(7),
            supporting[1].width.saturating_sub(7),
            queue_width,
        ]
        .into_iter()
        .min()
        .unwrap_or(1)
        .max(1);
        self.charts.overview_samples.set(Some(usize::from(samples)));
        let rows = Layout::vertical([
            Constraint::Length(info_height),
            Constraint::Length(summary_height),
            Constraint::Length(device_height),
            Constraint::Length(resource_height),
            Constraint::Length(rates_height),
            Constraint::Length(request_height),
            Constraint::Length(journal_height),
        ])
        .split(area);
        model_dashboard::draw(
            frame,
            rows[0],
            &self.collector.current,
            &self.collector.request_history,
        );
        crate::diagnostics_view::summary(frame, rows[1], &self.collector.current, self.paused);
        if devices {
            gpu_dashboard::draw(
                frame,
                rows[2],
                &self.collector.current.gpus,
                self.gpu_selected,
                self.thresholds,
            );
        }
        self.draw_resource_charts(frame, rows[3]);
        self.draw_workload_charts(frame, rows[4]);
        self.draw_supporting_charts(frame, rows[5]);
        if rows[6].height > 0 {
            if self.collector.operator_history.has_latency() && area.width >= 120 {
                let columns =
                    Layout::horizontal([Constraint::Percentage(70), Constraint::Percentage(30)])
                        .split(rows[6]);
                self.draw_signal_log(frame, columns[0]);
                self.draw_operator_chart(frame, columns[1], Chart::Latency);
            } else {
                self.draw_signal_log(frame, rows[6]);
            }
        }
    }

    pub(crate) fn draw_resource_charts(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(area);
        self.render_indicator_chart(
            frame,
            columns[0],
            "memory",
            &self.collector.load_history,
            ChartMetric::Memory,
        );
        self.render_indicator_chart(
            frame,
            columns[1],
            "paging / I/O",
            &self.collector.swap_history,
            ChartMetric::Swap,
        );
    }

    pub(crate) fn draw_supporting_charts(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([
            Constraint::Percentage(50),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(area);
        self.draw_request_chart(frame, columns[0]);
        self.render_indicator_chart(
            frame,
            columns[1],
            "cache",
            &self.collector.cache_history,
            ChartMetric::Cache,
        );
        self.draw_operator_chart(frame, columns[2], Chart::Queue);
    }

    pub(crate) fn draw_workload_charts(&self, frame: &mut Frame, area: Rect) {
        let columns = Layout::horizontal([
            Constraint::Percentage(40),
            Constraint::Percentage(35),
            Constraint::Percentage(25),
        ])
        .split(area);
        self.render_indicator_chart(
            frame,
            columns[0],
            "generation",
            &self.collector.generation_history,
            ChartMetric::Generation,
        );
        self.render_indicator_chart(
            frame,
            columns[1],
            "prefill",
            &self.collector.prefill_history,
            ChartMetric::Prefill,
        );
        self.render_indicator_chart(
            frame,
            columns[2],
            self.gpu_chart_title(),
            &self.collector.gpu_history,
            ChartMetric::Gpu,
        );
    }

    pub(crate) fn gpu_chart_title(&self) -> &'static str {
        if self.collector.current.has_nvidia_gpus() && self.collector.current.gpus.len() > 1 {
            "GPU max"
        } else {
            "GPU"
        }
    }

    pub(crate) fn render_indicator_chart(
        &self,
        frame: &mut Frame,
        area: Rect,
        name: &'static str,
        history: &VecDeque<ChartPoint>,
        metric: ChartMetric,
    ) {
        self.charts.register(Chart::from(metric), area);
        let zoom = usize::from(self.charts.zoom(Chart::from(metric)));
        let current = history.back().and_then(|point| point.value);
        let current_label = match metric {
            ChartMetric::Generation | ChartMetric::Prefill => current
                .map(|value| {
                    if area.width < 38 {
                        format!("{:.1}", value as f64 / 10.0)
                    } else {
                        format!("LIVE {:.1} tok/s", value as f64 / 10.0)
                    }
                })
                .unwrap_or_else(|| {
                    let label = chart_inactive_rate_label(metric, &self.collector.current);
                    if area.width < 38 {
                        label.replace(" active", "")
                    } else {
                        label
                    }
                }),
            ChartMetric::Cache => current
                .map(|value| {
                    if area.width < 28 {
                        format!("int {value}%")
                    } else {
                        format!("interval {value}%")
                    }
                })
                .unwrap_or_else(|| {
                    if area.width < 28 {
                        "int —".into()
                    } else {
                        "interval —".into()
                    }
                }),
            ChartMetric::Gpu => current
                .map(|value| format!("{value}%"))
                .unwrap_or_else(|| "—".into()),
            ChartMetric::Memory => current
                .map(|value| {
                    if area.width >= 24 {
                        format!("{value}% resident")
                    } else {
                        format!("{value}%")
                    }
                })
                .unwrap_or_else(|| "—".into()),
            ChartMetric::Swap => current.map(rate).unwrap_or_else(|| "—".into()),
        };
        let chart_tone = metric.chart_tone();
        let current_tone = history
            .back()
            .map(|point| point.tone)
            .unwrap_or(Tone::Muted);
        let current_tone = if metric == ChartMetric::Memory && current.is_some() {
            Tone::Cyan
        } else {
            current_tone
        };
        let rate_chart = matches!(metric, ChartMetric::Generation | ChartMetric::Prefill);
        let label_width = if metric == ChartMetric::Swap {
            12
        } else if rate_chart {
            6
        } else {
            5
        }
        .min(area.width.saturating_sub(2) as usize);
        let plot_width = (area.width.saturating_sub(2) as usize).saturating_sub(label_width);
        let visible_count = self.charts.visible_samples(Chart::from(metric), plot_width);
        let visible_missing = history
            .iter()
            .rev()
            .take(visible_count)
            .all(|point| point.value.is_none());
        let scale = chart_scale(history, metric, visible_count);
        let top_axis = chart_axis_label(metric, scale.1);
        let mut axis_label = if rate_chart || metric == ChartMetric::Swap {
            format!("{}–{} auto", chart_axis_label(metric, scale.0), top_axis)
        } else {
            "0–100%".into()
        };
        let (average, peak) = chart_stats_for_width(history, metric, visible_count);
        if metric == ChartMetric::Swap && peak == Some(0) {
            axis_label = "zero traffic".into();
        }
        let window = format!(
            "{} · {zoom}×",
            chart_window_label(history.len().min(visible_count), self.interval)
        );
        let name = if metric == ChartMetric::Memory && area.width < 30 {
            "RAM"
        } else if metric == ChartMetric::Swap && area.width < 34 {
            "paging"
        } else {
            name
        };
        let mut title = Line::from(vec![Span::styled(
            format!(" {name} "),
            Style::default()
                .fg(chart_tone.color())
                .add_modifier(Modifier::BOLD),
        )]);
        let reading = Line::from(Span::styled(
            format!(" {current_label} "),
            Style::default()
                .fg(current_tone.color())
                .add_modifier(Modifier::BOLD),
        ))
        .alignment(Alignment::Right);
        // Whole metadata fields fit or disappear. Never clip a token rate,
        // unit, time window or PID to squeeze in lower-priority statistics.
        let mut details = vec![window.clone()];
        if average.is_some() {
            details.extend([
                format!("window avg {}", chart_stat_label(metric, average)),
                format!("peak {}", chart_stat_label(metric, peak)),
                axis_label,
                "older → now".into(),
            ]);
        }
        if !self.charts.expanded {
            let span = Span::styled(
                format!("· {} ", chart_window_label(visible_count, self.interval)),
                Style::default().fg(MUTED),
            );
            if title.width() + span.width() + reading.width() + 2
                <= usize::from(area.width.saturating_sub(2))
            {
                title.spans.push(span);
            }
        }
        if self.charts.expanded && area.width >= 38 {
            for detail in details {
                let span = Span::styled(format!(" · {detail}"), Style::default().fg(MUTED));
                if title.width() + span.width() + reading.width() + 2
                    <= usize::from(area.width.saturating_sub(2))
                {
                    title.spans.push(span);
                }
            }
        }
        let mut block = Block::default()
            .title(title)
            .title(reading)
            .borders(Borders::ALL)
            .border_style(Style::default().fg(DIM))
            .style(Style::default().bg(PANEL));
        let sample = &self.collector.current;
        let summary = match metric {
            ChartMetric::Generation
                if self.charts.expanded
                    && sample.llm_generation_tps.is_some()
                    && !sample.llm_generation_tps_live =>
            {
                Some(llm_generation_rate_label(sample))
            }
            ChartMetric::Prefill
                if self.charts.expanded
                    && sample.llm_prefill_tps.is_some()
                    && !sample.llm_prefill_tps_live =>
            {
                Some(llm_prefill_rate_label(sample))
            }
            ChartMetric::Generation | ChartMetric::Prefill => Some("tok/s · auto".into()),
            ChartMetric::Memory => Some(if area.width < 24 {
                pressure_state_label(sample).into()
            } else {
                format!("PRESSURE {}", pressure_state_label(sample))
            }),
            ChartMetric::Gpu => Some(if current == Some(0) {
                "idle".into()
            } else {
                gpu_load_label(sample.gpu_util, self.thresholds).into()
            }),
            ChartMetric::Swap => Some(format!(
                "IN {} OUT {}",
                if sample.rate_ready {
                    rate(sample.swap_in).replace(' ', "")
                } else {
                    "—".into()
                },
                if sample.rate_ready {
                    rate(sample.swap_out).replace(' ', "")
                } else {
                    "—".into()
                }
            )),
            ChartMetric::Cache => Some(if area.width < 38 {
                format!("TOTAL {}", percent(sample.llm_cache_efficiency))
            } else {
                format!(
                    "TOTAL {} · PREFIX HIT {}",
                    percent(sample.llm_cache_efficiency),
                    percent(sample.llm_prefix_hit_rate)
                )
            }),
        };
        if let Some(summary) = summary {
            let summary = if rate_chart {
                if area.width < 38 {
                    summary.replace(" GEN", "").replace(" PREFILL", "")
                } else {
                    summary.replacen("AVG ", "SERVER AVG ", 1)
                }
            } else if metric == ChartMetric::Memory
                && Line::from(summary.as_str()).width() + 2
                    > usize::from(area.width.saturating_sub(2))
            {
                summary.replace("process ", "OS ")
            } else {
                summary
            };
            block = block.title_bottom(Line::from(Span::styled(
                format!(" {summary} "),
                if metric == ChartMetric::Memory {
                    Style::default()
                        .fg(sample.pressure_tone.color())
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(chart_tone.color())
                },
            )));
        }
        let mut inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        if metric == ChartMetric::Memory && inner.height >= 5 {
            let usage = sample
                .resident_memory
                .map(|used| format!("{} / {}", bytes(used), bytes(sample.total_memory)))
                .unwrap_or_else(|| "RAM reading unavailable".into());
            let combined = format!("{usage} · Includes file cache");
            let lines = if Line::from(combined.as_str()).width() <= usize::from(inner.width) {
                vec![combined]
            } else {
                vec![usage, "Includes file cache".into()]
            };
            for text in lines {
                frame.render_widget(
                    Paragraph::new(text).style(Style::default().fg(MUTED)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                inner.y += 1;
                inner.height -= 1;
            }
        }
        if metric == ChartMetric::Swap {
            swap_usage::draw(
                frame,
                Rect::new(inner.x, inner.bottom() - 1, inner.width, 1),
                sample,
            );
            inner.height = inner.height.saturating_sub(1);
            if inner.is_empty() {
                return;
            }
            if inner.height < 3 {
                frame.render_widget(
                    Paragraph::new(match current {
                        Some(0) => "No paging traffic",
                        Some(_) => "Paging active",
                        None => "Paging unavailable",
                    })
                    .style(Style::default().fg(current_tone.color())),
                    inner,
                );
                return;
            }
        }
        if metric == ChartMetric::Cache && area.width < 38 && inner.height >= 4 {
            frame.render_widget(
                Paragraph::new(format!(
                    "PREFIX HIT {}",
                    percent(sample.llm_prefix_hit_rate)
                ))
                .style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.y, inner.width, 1),
            );
            inner.y += 1;
            inner.height -= 1;
        }
        let compact_percent = inner.height < 3
            && matches!(
                metric,
                ChartMetric::Memory | ChartMetric::Gpu | ChartMetric::Cache
            );
        if compact_percent {
            let measured = current.map(|value| (value as f64, format!("{value}%"), current_tone));
            if let Some((value, label, tone)) = measured {
                frame.render_widget(
                    Gauge::default()
                        .ratio((value / 100.0).clamp(0.0, 1.0))
                        .label(label)
                        .use_unicode(true)
                        .gauge_style(Style::default().fg(tone.color()).bg(EDGE)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                if inner.height > 1 {
                    frame.render_widget(
                        Paragraph::new("Enter: history").style(Style::default().fg(MUTED)),
                        Rect::new(inner.x, inner.y + 1, inner.width, 1),
                    );
                }
            } else {
                frame.render_widget(
                    Paragraph::new(if inner.width < 19 {
                        "Unavailable"
                    } else {
                        "Reading unavailable"
                    })
                    .style(Style::default().fg(MUTED)),
                    inner,
                );
            }
            return;
        }
        if visible_missing {
            let message = match metric {
                _ if history.iter().any(|point| point.value.is_some()) => "No samples in view",
                ChartMetric::Cache if inner.width < 22 => "No cache samples",
                ChartMetric::Cache => "No interval cache data",
                ChartMetric::Generation | ChartMetric::Prefill
                    if sample.llm_status == "idle" && inner.width >= 28 =>
                {
                    "Idle · no live rate samples"
                }
                ChartMetric::Generation | ChartMetric::Prefill => "No live rate samples",
                _ => "No samples available",
            };
            frame.render_widget(
                Paragraph::new(message)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(MUTED)),
                Rect::new(
                    inner.x,
                    inner.y + inner.height.saturating_sub(1) / 2,
                    inner.width,
                    1,
                ),
            );
            return;
        }
        // Zero is a measurement, not an absent sample. Keep the zero trace
        // and its gaps, but do not invent a 1 B/s ceiling for an idle window.
        let paging_idle = metric == ChartMetric::Swap && peak == Some(0);
        if rate_chart && inner.height < 3 {
            frame.render_widget(
                Paragraph::new("Enter: history").style(Style::default().fg(MUTED)),
                inner,
            );
            return;
        }

        // Historical observations end where sampling stopped. Explain the
        // trailing gap without drawing a zero or holding the old rate live.
        if current.is_none() && inner.height >= 4 {
            if let Some((observed_at, value)) = history
                .iter()
                .rev()
                .take(visible_count)
                .find_map(|point| point.value.map(|value| (point.observed_at, value)))
            {
                let value_label = chart_stat_label(metric, Some(value));
                let mut label = if rate_chart {
                    format!("LAST SAMPLE {value_label} tok/s")
                } else if metric == ChartMetric::Cache {
                    format!("LAST INTERVAL {value_label}")
                } else {
                    format!("LAST {value_label}")
                };
                if Line::from(label.as_str()).width() > usize::from(inner.width) {
                    label = if rate_chart {
                        format!("LAST {value_label} tok/s")
                    } else {
                        format!("LAST INT {value_label}")
                    };
                }
                let with_age = format!("{label} · {}", telemetry_age(Some(observed_at)));
                if Line::from(with_age.as_str()).width() <= usize::from(inner.width) {
                    label = with_age;
                }
                frame.render_widget(
                    Paragraph::new(label).style(Style::default().fg(MUTED)),
                    Rect::new(inner.x, inner.y, inner.width, 1),
                );
                inner.y += 1;
                inner.height -= 1;
            }
        }

        let plot_height = inner.height as usize;
        // Render the newest sample at the right edge and let older samples
        // leave from the left. Every displayed column maps to one captured
        // sample: smoothing uses only the causal prefix and the current chart
        // resolution. Auto-scaling changes coordinates, never captured values,
        // ordering or colors; there is no future-sample smoothing.
        let points = chart_columns_for_plot(history, visible_count, metric, plot_height, scale);
        let mut visible_points = stretch_chart_columns(&points, plot_width);
        if metric == ChartMetric::Memory {
            for point in &mut visible_points {
                if point.value.is_some() {
                    point.tone = Tone::Cyan;
                }
            }
        }
        let mut cells = vec![
            vec![
                TraceCell {
                    glyph: ' ',
                    tone: Tone::Muted,
                };
                plot_width
            ];
            plot_height
        ];
        for (row, row_cells) in cells.iter_mut().enumerate() {
            let guide = if row + 1 == plot_height {
                Some('─')
            } else if row == plot_height.saturating_sub(1) / 2 && !paging_idle {
                Some('┄')
            } else {
                None
            };
            if let Some(glyph) = guide {
                row_cells.fill(TraceCell {
                    glyph,
                    tone: Tone::Muted,
                });
            }
        }

        let mut point_rows = vec![None; plot_width];
        let mut connect_before = vec![false; plot_width];
        let mut previous_point = None;
        for (column, point) in visible_points.iter().enumerate() {
            let Some(value) = point.value else {
                previous_point = None;
                continue;
            };
            if point.break_before {
                previous_point = None;
            }
            let display_value = chart_display_value(metric, value, scale);
            let Some((row, glyph)) = trace_point(display_value, plot_height) else {
                previous_point = None;
                continue;
            };
            if previous_point.is_some() {
                connect_before[column] = true;
            }
            point_rows[column] = Some((row, point.tone));
            cells[row][column] = TraceCell {
                glyph: if previous_point.is_none()
                    && visible_points
                        .get(column + 1)
                        .is_none_or(|next| next.value.is_none() || next.break_before)
                {
                    '●'
                } else {
                    glyph
                },
                tone: point.tone,
            };
            previous_point = Some((row, point.tone));
        }

        for column in 1..plot_width {
            if !connect_before[column] {
                continue;
            }
            let (Some((previous_row, previous_tone)), Some((row, tone))) =
                (point_rows[column - 1], point_rows[column])
            else {
                continue;
            };
            trace_connector(
                &mut cells,
                column,
                previous_row,
                row,
                TraceStyle {
                    metric,
                    previous_tone,
                    tone,
                    thresholds: self.thresholds,
                    scale,
                },
            );
        }

        let mut lines = Vec::with_capacity(plot_height);
        for (row, row_cells) in cells.iter().enumerate() {
            let label = if row == 0 && !paging_idle {
                format!("{top_axis} ")
            } else if row + 1 == plot_height {
                format!("{} ", chart_axis_label(metric, scale.0))
            } else if row == plot_height.saturating_sub(1) / 2
                && !paging_idle
                && scale.1.saturating_sub(scale.0) > 1
            {
                format!(
                    "{} ",
                    chart_axis_label(metric, scale.0 + (scale.1 - scale.0) / 2)
                )
            } else {
                String::new()
            };
            let mut spans = vec![Span::styled(
                format!("{label:>label_width$}"),
                Style::default().fg(DIM),
            )];
            let mut run = String::new();
            let mut run_tone = None;
            for cell in row_cells.iter().take(plot_width).copied() {
                let (cell, tone) = (cell.glyph, cell.tone);
                if run_tone != Some(tone) {
                    if let Some(tone) = run_tone {
                        spans.push(Span::styled(
                            std::mem::take(&mut run),
                            Style::default().fg(tone.color()),
                        ));
                    }
                    run_tone = Some(tone);
                }
                run.push(cell);
            }
            if let Some(tone) = run_tone {
                spans.push(Span::styled(run, Style::default().fg(tone.color())));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(Text::from(lines)), inner);
        if paging_idle && inner.height >= 4 {
            frame.render_widget(
                Paragraph::new(if inner.width >= 32 {
                    "No paging traffic in this window"
                } else {
                    "No paging traffic"
                })
                .alignment(Alignment::Center)
                .style(Style::default().fg(MUTED)),
                Rect::new(inner.x, inner.y + inner.height / 2 - 1, inner.width, 1),
            );
        }
    }

    pub(crate) fn draw_signal_log(&self, frame: &mut Frame, area: Rect) {
        let block = panel("RECENT JOURNAL", Tone::Muted)
            .title(
                Line::from(Span::styled(" 3 open ", Style::default().fg(CYAN)))
                    .alignment(Alignment::Right),
            )
            .title_bottom(Line::from(Span::styled(
                format!(" latest first · {} events ", self.collector.signals.len()),
                Style::default().fg(MUTED),
            )));
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        if self.collector.signals.is_empty() {
            frame.render_widget(
                Paragraph::new("No events yet. Requests and resource changes appear here.")
                    .style(Style::default().fg(MUTED))
                    .wrap(Wrap { trim: true }),
                inner,
            );
            return;
        }
        let message_x = inner.x + 24.min(inner.width);
        let message_width = inner.right().saturating_sub(message_x);
        let mut y = inner.y;
        for event in self.collector.signals.iter().rev() {
            if y >= inner.bottom() || message_width == 0 {
                break;
            }
            let mut lines = Vec::new();
            let mut line = String::new();
            for word in event.summary.split_whitespace() {
                let next = if line.is_empty() {
                    word.to_owned()
                } else {
                    format!("{line} {word}")
                };
                if Line::from(next.as_str()).width() > usize::from(message_width)
                    && !line.is_empty()
                {
                    lines.push(std::mem::take(&mut line));
                    line = compact_label(word, usize::from(message_width));
                } else {
                    line = compact_label(&next, usize::from(message_width));
                }
            }
            if !line.is_empty() {
                lines.push(line);
            }
            if lines.is_empty() {
                lines.push(String::new());
            }
            let height = lines.len().min(2).min(usize::from(inner.bottom() - y));
            if lines.len() > height {
                lines[height - 1] = format!(
                    "{}…",
                    compact_label(
                        &lines[height - 1],
                        usize::from(message_width).saturating_sub(1)
                    )
                );
            }
            frame.render_widget(
                Paragraph::new(event.time.as_str()).style(Style::default().fg(MUTED)),
                Rect::new(inner.x, y, 9, 1),
            );
            frame.render_widget(
                Paragraph::new(compact_label(&event.state, 13)).style(
                    Style::default()
                        .fg(event.tone.color())
                        .add_modifier(Modifier::BOLD),
                ),
                Rect::new(inner.x + 10, y, 13, 1),
            );
            frame.render_widget(
                Paragraph::new(
                    lines
                        .into_iter()
                        .take(height)
                        .map(Line::from)
                        .collect::<Vec<_>>(),
                )
                .style(Style::default().fg(Color::White)),
                Rect::new(message_x, y, message_width, height as u16),
            );
            y += height as u16;
        }
    }

    pub(crate) fn draw_journal(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(10)])
            .split(area);
        self.draw_journal_header(frame, rows[0]);
        self.draw_journal_events(frame, rows[1]);
    }

    pub(crate) fn draw_llm_top(&self, frame: &mut Frame, area: Rect) {
        let details = self.collector.current.llm_details.as_deref();
        let rows = Layout::vertical([
            Constraint::Length(if details.is_some() { 5 } else { 4 }),
            Constraint::Min(6),
            Constraint::Length(7),
        ])
        .split(area);
        let sample = &self.collector.current;
        let filtered = self.filtered_llm_processes();
        let total_rss = filtered.iter().map(|p| p.rss).sum::<u64>();
        let total_cpu = filtered.iter().map(|p| p.cpu).sum::<f64>();
        let filter_label = if self.top_filter.is_empty() {
            "all local LLM processes".to_owned()
        } else {
            format!("filter /{}", self.top_filter)
        };
        frame.render_widget(
            Paragraph::new(
                [
                    Line::from(Span::styled(
                        format!(
                            " {} of {} processes · RSS {} · CPU {:.1}% · sort {}",
                            filtered.len(),
                            sample.llm_processes.len(),
                            bytes(total_rss),
                            total_cpu,
                            self.top_sort.label()
                        ),
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    )),
                    Line::from(Span::styled(
                        format!(
                            " {}{}",
                            filter_label,
                            if self.top_filtering {
                                " · typing · Enter/Esc finish"
                            } else {
                                " · ↑↓ select · s sort · / filter · c clear"
                            }
                        ),
                        Style::default().fg(if self.top_filtering { YELLOW } else { MUTED }),
                    )),
                ]
                .into_iter()
                .chain(details.map(|text| {
                    Line::from(Span::styled(
                        format!(" API · {text}"),
                        Style::default().fg(CYAN),
                    ))
                }))
                .collect::<Vec<_>>(),
            )
            .block(panel("PROCESS MONITOR · LOCAL OS READINGS", Tone::Cyan)),
            rows[0],
        );
        let selected = self.top_selected.min(filtered.len().saturating_sub(1));
        let visible = rows[1].height.saturating_sub(3).max(1) as usize;
        let start = selected
            .saturating_sub(visible.saturating_sub(1))
            .min(filtered.len().saturating_sub(visible));
        let table_mode = if rows[1].width >= 150 {
            2
        } else if rows[1].width >= 95 {
            1
        } else {
            0
        };
        let (headers, widths): (Vec<&str>, Vec<Constraint>) = match table_mode {
            2 => (
                vec![
                    "PID", "PROCESS", "CPU", "MEM%", "RSS", "PAGEIN/s", "OS STATE", "PROVIDER",
                    "COMMAND",
                ],
                vec![
                    Constraint::Length(8),
                    Constraint::Length(20),
                    Constraint::Length(8),
                    Constraint::Length(8),
                    Constraint::Length(13),
                    Constraint::Length(11),
                    Constraint::Length(9),
                    Constraint::Length(12),
                    Constraint::Min(20),
                ],
            ),
            1 => (
                vec![
                    "PID", "PROCESS", "CPU", "RSS", "PAGEIN/s", "OS STATE", "PROVIDER",
                ],
                vec![
                    Constraint::Length(7),
                    Constraint::Min(20),
                    Constraint::Length(8),
                    Constraint::Length(12),
                    Constraint::Length(10),
                    Constraint::Length(9),
                    Constraint::Length(12),
                ],
            ),
            _ => (
                vec!["PID", "PROCESS", "CPU", "RSS", "OS STATE"],
                vec![
                    Constraint::Length(7),
                    Constraint::Min(22),
                    Constraint::Length(8),
                    Constraint::Length(12),
                    Constraint::Length(9),
                ],
            ),
        };
        let table_rows = filtered
            .iter()
            .skip(start)
            .take(visible)
            .map(|process| {
                let pagein = process
                    .pagein_rate
                    .map(|value| format!("{value:.1}"))
                    .unwrap_or_else(|| "—".into());
                let state = if process.state == "?" {
                    "—"
                } else {
                    &process.state
                };
                let provider =
                    process_provider(&process.name, &process.command).unwrap_or_else(|| "—".into());
                let mut cells = vec![
                    Cell::from(process.pid.to_string()),
                    Cell::from(process.name.clone()),
                    Cell::from(format!("{:.1}%", process.cpu)),
                ];
                if table_mode == 2 {
                    cells.push(Cell::from(
                        process
                            .memory_percent
                            .map(|value| format!("{value:.1}%"))
                            .unwrap_or_else(|| "—".into()),
                    ));
                }
                cells.push(Cell::from(bytes(process.rss)));
                if table_mode > 0 {
                    cells.push(Cell::from(pagein));
                }
                cells.push(Cell::from(state.to_owned()));
                if table_mode > 0 {
                    cells.push(Cell::from(provider));
                }
                if table_mode == 2 {
                    cells.push(Cell::from(process.command.clone()));
                }
                Row::new(cells)
            })
            .collect::<Vec<_>>();
        let title = format!(
            "LLM PROCESSES · {}–{} of {}",
            if filtered.is_empty() { 0 } else { start + 1 },
            (start + visible).min(filtered.len()),
            filtered.len()
        );
        let table = Table::new(table_rows, widths)
            .header(
                Row::new(headers).style(Style::default().fg(MUTED).add_modifier(Modifier::BOLD)),
            )
            .row_highlight_style(
                Style::default()
                    .bg(Color::Rgb(35, 48, 67))
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
            .highlight_symbol("▸ ")
            .block(panel(&title, Tone::Blue));
        let mut table_state = TableState::default();
        if !filtered.is_empty() {
            table_state.select(Some(selected.saturating_sub(start)));
        }
        frame.render_stateful_widget(table, rows[1], &mut table_state);

        let Some(process) = filtered.get(selected) else {
            let message = if self.top_filter.is_empty() {
                "No local LLM processes detected. Provider telemetry can still be available in Overview."
            } else {
                "No processes match this filter. Press c to clear it or / to edit."
            };
            frame.render_widget(
                Paragraph::new(message)
                    .wrap(Wrap { trim: true })
                    .block(panel("SELECTED PROCESS", Tone::Muted)),
                rows[2],
            );
            return;
        };
        let provider = process_provider(&process.name, &process.command);
        let mut detail = vec![Line::from(format!(
            "CPU {:.1}% · RSS {} · RAM {} · OS {} · PAGEIN {} pages/s",
            process.cpu,
            bytes(process.rss),
            process
                .memory_percent
                .map(|v| format!("{v:.1}%"))
                .unwrap_or_else(|| "—".into()),
            process.state,
            process
                .pagein_rate
                .map(|v| format!("{v:.1}"))
                .unwrap_or_else(|| "—".into())
        ))];
        if sample
            .process_memory
            .as_ref()
            .is_some_and(|reading| reading.pid == process.pid)
        {
            detail.push(Line::from(format!(
                "OS footprint {} · {}",
                bytes(sample.process_memory.as_ref().unwrap().footprint),
                process_memory::detail(sample)
            )));
        }
        // Provider telemetry is runtime-wide; never attribute it to every PID.
        if provider.as_deref() == Some(sample.llm_provider.as_str()) {
            detail.push(Line::from(Span::styled(
                format!(
                    "RUNTIME {} · {} · {} · {}",
                    sample.llm_provider,
                    sample.llm_model,
                    sample.llm_status.to_uppercase(),
                    telemetry_source(sample)
                ),
                Style::default().fg(CYAN),
            )));
        } else {
            detail.push(Line::from(Span::styled(
                format!(
                    "RUNTIME {} · model/state unavailable",
                    provider.as_deref().unwrap_or("unknown")
                ),
                Style::default().fg(MUTED),
            )));
        }
        detail.push(Line::from(format!("COMMAND {}", process.command)));
        frame.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: true })
                .block(panel(
                    &format!("SELECTED PROCESS · PID {} · {}", process.pid, process.name),
                    Tone::Cyan,
                )),
            rows[2],
        );
    }

    pub(crate) fn draw_journal_header(&self, frame: &mut Frame, area: Rect) {
        let filtered_events = self.filtered_journal_events();
        let event_count = filtered_events.len();
        let total_count = self.collector.signals.len();
        let latest = filtered_events
            .last()
            .map(|event| event.summary.as_str())
            .unwrap_or("waiting for the first recorded event");
        let latest_tone = filtered_events
            .last()
            .map(|event| event.tone)
            .unwrap_or(Tone::Muted);
        let latest_age = filtered_events
            .last()
            .map(|event| telemetry_age(Some(event.recorded_at)))
            .unwrap_or_else(|| "age —".into());
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(
                        " JOURNAL  ",
                        Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(
                            "{event_count} {} events · {total_count} total · f/[/] filter",
                            self.journal_filter.label(),
                        ),
                        Style::default().fg(Color::White),
                    ),
                    Span::styled("  ·  ", Style::default().fg(DIM)),
                    Span::styled("meaningful changes only", Style::default().fg(MUTED)),
                ]),
                Line::from(vec![
                    Span::styled("  LATEST  ", Style::default().fg(MUTED)),
                    Span::styled(
                        format!("{latest} · {latest_age}"),
                        Style::default().fg(latest_tone.color()),
                    ),
                ]),
                Line::from(vec![
                    Span::styled("  SCOPE   ", Style::default().fg(MUTED)),
                    Span::styled(
                        "what changed · why it matters · what recovered",
                        Style::default().fg(CYAN),
                    ),
                    Span::styled("  ·  ↑ newer · ↓ older", Style::default().fg(DIM)),
                ]),
            ])
            .block(panel("EVENT JOURNAL · IMPACT TIMELINE", Tone::Cyan))
            .wrap(Wrap { trim: true }),
            area,
        );
    }

    pub(crate) fn draw_journal_events(&self, frame: &mut Frame, area: Rect) {
        let capacity = area.height.saturating_sub(2) as usize;
        let filtered_events = self.filtered_journal_events();
        let max_scroll = filtered_events.len().saturating_sub(capacity);
        let scroll = self.journal_scroll.min(max_scroll);
        let lines = filtered_events
            .iter()
            .rev()
            .skip(scroll)
            .take(capacity)
            .map(|event| {
                Line::from(vec![
                    Span::styled(format!(" {} ", event.time), Style::default().fg(DIM)),
                    Span::styled(
                        format!("{:^16}", compact_label(&event.state, 16)),
                        Style::default()
                            .fg(event.tone.color())
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled("  │  ", Style::default().fg(DIM)),
                    Span::styled(&event.summary, Style::default().fg(Color::White)),
                ])
            })
            .collect::<Vec<_>>();
        let text = if lines.is_empty() {
            Text::from(Line::from(Span::styled(
                " waiting for the first recorded event",
                Style::default().fg(MUTED),
            )))
        } else {
            Text::from(lines)
        };
        let title = format!(
            "EVENTS · {} · {}–{} of {}",
            self.journal_filter.label(),
            if filtered_events.is_empty() {
                0
            } else {
                scroll + 1
            },
            (scroll + capacity).min(filtered_events.len()),
            filtered_events.len()
        );
        frame.render_widget(
            Paragraph::new(text)
                .block(panel(&title, Tone::Blue))
                .wrap(Wrap { trim: true }),
            area,
        );
        if filtered_events.len() > capacity {
            let mut scrollbar_state = ScrollbarState::new(filtered_events.len()).position(scroll);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .thumb_style(Style::default().fg(CYAN))
                    .track_style(Style::default().fg(DIM)),
                area,
                &mut scrollbar_state,
            );
        }
    }

    pub(crate) fn draw_controls(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(
            Block::default().style(Style::default().bg(PANEL_RAISED)),
            area,
        );
        let help = Line::from(vec![
            Span::styled(
                " d",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                if self.diagnostics_open {
                    " close "
                } else {
                    " diagnostics "
                },
                Style::default().fg(MUTED),
            ),
            Span::styled(
                " ?",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" help ", Style::default().fg(MUTED)),
            Span::styled(
                "q",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" quit ", Style::default().fg(MUTED)),
        ]);
        let available = usize::from(area.width).saturating_sub(help.width());
        let mut line = Line::from(Span::styled(
            if self.diagnostics_open {
                " Diagnostics ".into()
            } else if self.tab == 0 {
                format!(
                    " {} · {}× ",
                    self.charts.focused.label(),
                    self.charts.zoom(self.charts.focused)
                )
            } else if self.tab == 1 {
                " MLX Top ".into()
            } else {
                " Journal ".into()
            },
            Style::default().fg(CYAN),
        ));
        let hints = if self.diagnostics_open {
            vec![
                ("↑↓", "scroll"),
                ("PgUp/Dn", "page"),
                ("Tab", "view"),
                ("p", if self.paused { "resume" } else { "pause" }),
            ]
        } else {
            match self.tab {
                0 => vec![
                    (
                        "Enter",
                        if self.charts.expanded {
                            "restore"
                        } else {
                            "expand"
                        },
                    ),
                    ("↑↓←→", "chart"),
                    ("+/−", "zoom"),
                    ("Tab", "view"),
                    ("p", if self.paused { "resume" } else { "pause" }),
                    ("Shift+↑↓", "requests"),
                    ("{ / }", "interval"),
                ],
                1 => vec![
                    ("↑↓", "select"),
                    ("s", "sort"),
                    ("/", "filter"),
                    ("Tab", "view"),
                    ("p", "pause"),
                ],
                _ => vec![
                    ("↑↓", "scroll"),
                    ("f", "filter"),
                    ("Tab", "view"),
                    ("r", "reset"),
                    ("p", "pause"),
                ],
            }
        };
        for (key, label) in hints {
            let key = Span::styled(
                format!(" {key}"),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            );
            let label = Span::styled(format!(" {label} "), Style::default().fg(MUTED));
            if line.width() + key.width() + label.width() <= available {
                line.spans.extend([key, label]);
            }
        }
        frame.render_widget(
            Paragraph::new(line),
            Rect::new(area.x, area.y, available as u16, 1),
        );
        frame.render_widget(
            Paragraph::new(help),
            Rect::new(
                area.x + available as u16,
                area.y,
                area.width - available as u16,
                1,
            ),
        );
    }

    pub(crate) fn draw_help(&self, frame: &mut Frame, area: Rect) {
        let popup = if area.width < 120 || area.height < 36 {
            centered_rect(94, 88, area)
        } else {
            centered_rect(60, 58, area)
        };
        frame.render_widget(Clear, popup);
        let mut text = vec![
            Line::from(Span::styled(
                "CONTROLS",
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )),
            Line::from("1 / 2 / 3       Overview / MLX Top / Journal"),
            Line::from("d               diagnostics and runtime setup"),
            Line::from("Tab / Shift-Tab next / previous view"),
            Line::from("p / Space       pause or resume; r resets history"),
            Line::from("{ / }           change refresh interval (1–60s)"),
            Line::from("a               acknowledge critical system alarm"),
            Line::from("q / Ctrl-C      quit; ? / h closes help"),
            Line::from(""),
        ];
        text.extend(match self.tab {
            0 => vec![
                Line::from("Arrow keys      select a neighboring chart"),
                Line::from("+ / - / wheel   zoom (time series linked); 0 reset"),
                Line::from("Enter / Esc     enlarge / restore chart"),
                Line::from("Mouse click     select chart; right-click enlarge"),
                Line::from("Shift-↑↓        newer / older prompt"),
                Line::from("Home / End      newest / oldest prompt"),
                Line::from("PgUp / PgDn     move by ten requests"),
                Line::from(if self.collector.current.has_nvidia_gpus() {
                    "[ / ] GPUs      select previous / next NVIDIA card"
                } else {
                    ""
                }),
            ],
            1 => vec![
                Line::from("↑↓ / PgUp/PgDn  select process / move ten rows"),
                Line::from("Home / End      first / last process"),
                Line::from("s               cycle RSS / CPU / PID / name sort"),
                Line::from("f or /          filter processes; c clears filter"),
            ],
            _ => vec![
                Line::from("↑↓ / PgUp/PgDn  newer / older events"),
                Line::from("Home / End      newest / oldest event"),
                Line::from("f / [ / ]       cycle event filters"),
            ],
        });
        frame.render_widget(
            Paragraph::new(text).wrap(Wrap { trim: true }).block(
                Block::default()
                    .title(" HELP ")
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(CYAN))
                    .style(Style::default().bg(PANEL)),
            ),
            popup,
        );
    }

    pub(crate) fn draw_compact_warning(&self, frame: &mut Frame, area: Rect) {
        let text = vec![
            Line::from(Span::styled(
                "mlxtop",
                Style::default().fg(CYAN).add_modifier(Modifier::BOLD),
            )),
            Line::from(""),
            Line::from("This dashboard needs at least 72×24 terminal cells."),
            Line::from(format!("Current size: {}×{}", area.width, area.height)),
            Line::from("Resize the terminal, or use --once for a static report."),
            Line::from("q quit"),
        ];
        frame.render_widget(
            Paragraph::new(text).alignment(Alignment::Center).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(YELLOW)),
            ),
            area,
        );
    }
}
