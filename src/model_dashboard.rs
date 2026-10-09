// SPDX-License-Identifier: MIT
//! A dense system strip: runtime identity, workload and host facts share one border.
use crate::domain::Sample;
use crate::formatting::{
    bytes, compact_label, compact_tokens, compressed_memory_label, llm_context_label,
    optional_tokens, process_count_label, signed_rate, telemetry_age, telemetry_source,
};
use crate::theme::{card_block, llm_status_tone, tone_badge, BLUE, MUTED};
use crate::{gpu, request_history};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use std::env;

fn fitted_parts(parts: impl IntoIterator<Item = String>, width: usize) -> String {
    let mut text = String::new();
    for part in parts {
        if part.is_empty() {
            continue;
        }
        let next = if text.is_empty() {
            part.clone()
        } else {
            format!("{text} · {part}")
        };
        if Line::from(next.as_str()).width() <= width {
            text = next;
        } else if text.is_empty() {
            text = compact_label(&part, width);
        }
    }
    text
}

fn work_parts(sample: &Sample, history: &request_history::History) -> Vec<String> {
    if sample.llm_prompt_tokens.is_none() && sample.llm_active_requests == Some(0) {
        if let Some((request, observed)) = history.latest_for(sample) {
            return vec![
                format!("LAST PROMPT {}", compact_tokens(request.prompt)),
                telemetry_age(Some(observed)),
                format!("OUT {}", optional_tokens(request.output)),
            ];
        }
    }
    vec![
        format!("PROMPT {}", optional_tokens(sample.llm_prompt_tokens)),
        format!("OUT {}", optional_tokens(sample.llm_output_tokens)),
        format!("CONTEXT {}", llm_context_label(sample)),
    ]
}

/// Overview's prompt load panel already shows the latest request's prompt and
/// output. Keep runtime counters here only when they add a different reading.
fn prompt_panel_covers(sample: &Sample, history: &request_history::History) -> bool {
    history.latest_for(sample).is_some_and(|(request, _)| {
        sample
            .llm_prompt_tokens
            .is_none_or(|prompt| prompt == request.prompt)
    })
}

fn hardware_parts(sample: &Sample) -> Vec<String> {
    if sample.has_nvidia_gpus() {
        return vec![if sample.gpus.len() == 1 {
            sample.gpus[0].name.clone()
        } else {
            format!("{} NVIDIA GPUs", sample.gpus.len())
        }];
    }
    let mut parts = vec![sample
        .metal
        .device_name
        .clone()
        .unwrap_or_else(|| env::consts::OS.into())];
    if let Some(cores) = sample.metal.gpu_cores {
        parts.push(format!("{cores} GPU cores"));
    }
    parts
}

fn gpu_memory(sample: &Sample) -> Option<String> {
    if sample.has_nvidia_gpus() {
        return gpu::memory_totals(&sample.gpus)
            .map(|(used, total)| format!("VRAM sum {} / {}", bytes(used), bytes(total)));
    }
    match (sample.gpu_in_use, sample.gpu_alloc) {
        (Some(used), Some(total)) => Some(format!("GPU mem {} / {}", bytes(used), bytes(total))),
        (Some(used), None) => Some(format!("GPU mem {}", bytes(used))),
        (None, Some(total)) => Some(format!("GPU mem — / {}", bytes(total))),
        (None, None) => None,
    }
}

fn ram(sample: &Sample) -> String {
    format!(
        "RAM {}",
        if sample.total_memory > 0 {
            bytes(sample.total_memory)
        } else {
            "—".into()
        }
    )
}

fn cpu(sample: &Sample) -> String {
    if sample.llm_count > 0 {
        format!("CPU {:.1}%", sample.llm_cpu)
    } else {
        "CPU —".into()
    }
}

fn rss(sample: &Sample) -> String {
    format!(
        "LLM RSS {}",
        if sample.llm_count > 0 {
            bytes(sample.llm_rss)
        } else {
            "—".into()
        }
    )
}

fn thermal_parts(sample: &Sample) -> Vec<String> {
    let mut parts = vec![format!("THERMAL {}", sample.thermal)];
    if let Some(renderer) = sample.metal.renderer_util {
        parts.push(format!("R {renderer}%"));
    }
    if let Some(tiler) = sample.metal.tiler_util {
        parts.push(format!("T {tiler}%"));
    }
    parts
}

fn plain(value: String, color: Color) -> Line<'static> {
    Line::from(Span::styled(value, Style::default().fg(color)))
}

fn identity(sample: &Sample, width: usize) -> Line<'static> {
    Line::from(Span::styled(
        compact_label(
            &format!("{} · {}", sample.llm_provider, sample.llm_model),
            width,
        ),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    ))
}

fn state(sample: &Sample) -> Line<'static> {
    Line::from(vec![
        tone_badge(
            llm_status_tone(&sample.llm_status),
            &sample.llm_status.to_ascii_uppercase(),
        ),
        Span::styled(
            format!("  {}", telemetry_source(sample)),
            Style::default().fg(MUTED),
        ),
    ])
}

pub(super) fn draw(
    frame: &mut Frame,
    area: Rect,
    sample: &Sample,
    history: &request_history::History,
) {
    let width = usize::from(area.width.saturating_sub(2));
    let compact = area.height < 5;
    let wide = width >= 130;
    let tone = llm_status_tone(&sample.llm_status);
    let title = fitted_parts(
        std::iter::once("SYSINFO".into()).chain(
            if compact && !prompt_panel_covers(sample, history) {
                work_parts(sample, history)
            } else {
                Vec::new()
            },
        ),
        width.saturating_sub(2),
    );
    let footer = if compact {
        fitted_parts(
            hardware_parts(sample)
                .into_iter()
                .chain([
                    ram(sample),
                    format!("THERMAL {}", sample.thermal),
                    cpu(sample),
                    rss(sample),
                ])
                .chain(gpu_memory(sample)),
            width.saturating_sub(2),
        )
    } else if wide {
        let mut parts = Vec::new();
        if let Some(memory) = &sample.process_memory {
            parts.push(format!("OS peak {}", bytes(memory.peak)));
            if let Some(growth) = sample.process_memory_growth {
                parts.push(format!("growth {}", signed_rate(growth)));
            }
        }
        if let Some(version) = &sample.mlx.version {
            parts.push(format!("MLX {version}"));
        }
        fitted_parts(parts, width.saturating_sub(2))
    } else {
        fitted_parts(
            gpu_memory(sample)
                .into_iter()
                .chain([format!("COMP {}", compressed_memory_label(sample))])
                .chain(
                    (sample.llm_count > 0)
                        .then(|| format!("LOCAL {}", process_count_label(sample))),
                ),
            width.saturating_sub(2),
        )
    };
    let mut block = card_block(
        Line::from(Span::styled(
            format!(" {title} "),
            Style::default()
                .fg(tone.color())
                .add_modifier(Modifier::BOLD),
        )),
        tone,
    );
    if !footer.is_empty() {
        block = block.title_bottom(plain(format!(" {footer} "), MUTED));
    }
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return;
    }

    if compact {
        // Reserve state and provenance before fitting even an unusually long
        // model name; a clipped identity must never conceal stale/offline state.
        let status = state(sample);
        let status_width = status.width().min(usize::from(inner.width));
        let identity_width = usize::from(inner.width).saturating_sub(status_width + 2);
        frame.render_widget(
            Paragraph::new(identity(sample, identity_width)),
            Rect::new(inner.x, inner.y, identity_width as u16, 1),
        );
        frame.render_widget(
            Paragraph::new(status),
            Rect::new(
                inner.right() - status_width as u16,
                inner.y,
                status_width as u16,
                1,
            ),
        );
        return;
    }

    let gap = 2;
    let widths = if wide {
        let available = inner.width.saturating_sub(gap * 2);
        let first = available / 3;
        vec![first, first, available - first * 2]
    } else {
        let available = inner.width.saturating_sub(gap);
        let first = available * 55 / 100;
        vec![first, available - first]
    };
    let mut columns = Vec::new();
    let mut x = inner.x;
    for width in widths {
        columns.push(Rect::new(x, inner.y, width, inner.height));
        x += width + gap;
    }
    let runtime_width = usize::from(columns[0].width);
    frame.render_widget(
        Paragraph::new(vec![
            identity(sample, runtime_width),
            state(sample),
            plain(
                fitted_parts(work_parts(sample, history), runtime_width),
                Color::White,
            ),
        ]),
        columns[0],
    );
    let hardware_width = usize::from(columns[1].width);
    let hardware_lines = if wide {
        vec![
            plain(fitted_parts(hardware_parts(sample), hardware_width), BLUE),
            plain(
                fitted_parts(
                    [
                        ram(sample),
                        format!("COMP {}", compressed_memory_label(sample)),
                    ],
                    hardware_width,
                ),
                Color::White,
            ),
            plain(
                gpu_memory(sample).unwrap_or_else(|| "GPU mem —".into()),
                MUTED,
            ),
        ]
    } else {
        vec![
            plain(fitted_parts(hardware_parts(sample), hardware_width), BLUE),
            plain(
                fitted_parts([ram(sample), rss(sample)], hardware_width),
                Color::White,
            ),
            plain(
                fitted_parts(
                    [format!("THERMAL {}", sample.thermal), cpu(sample)],
                    hardware_width,
                ),
                MUTED,
            ),
        ]
    };
    frame.render_widget(Paragraph::new(hardware_lines), columns[1]);
    if wide {
        let process_width = usize::from(columns[2].width);
        let footprint = sample
            .process_memory
            .as_ref()
            .map(|memory| {
                format!(
                    "PID {} · OS footprint {}",
                    memory.pid,
                    bytes(memory.footprint)
                )
            })
            .unwrap_or_else(|| "OS footprint —".into());
        frame.render_widget(
            Paragraph::new(vec![
                plain(
                    fitted_parts(
                        [
                            format!("LOCAL {}", process_count_label(sample)),
                            cpu(sample),
                            rss(sample),
                        ],
                        process_width,
                    ),
                    Color::White,
                ),
                plain(compact_label(&footprint, process_width), MUTED),
                plain(fitted_parts(thermal_parts(sample), process_width), MUTED),
            ]),
            columns[2],
        );
    }
}

#[cfg(test)]
#[path = "tests/model_dashboard.rs"]
mod tests;
