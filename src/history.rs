// SPDX-License-Identifier: MIT
//! Counter rates and bounded time-series history updates.
use crate::config::Thresholds;
use crate::domain::{ChartMetric, ChartPoint, Tone};
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};
pub(crate) fn delta(current: u64, previous: u64) -> u64 {
    current.saturating_sub(previous)
}

pub(crate) fn rate_bytes(delta_units: u64, unit_bytes: u64, elapsed: Duration) -> u64 {
    let seconds = elapsed.as_secs_f64().max(0.001);
    let value = (delta_units as f64 * unit_bytes as f64 / seconds).round();
    if value.is_finite() && value > 0.0 {
        value.min(u64::MAX as f64) as u64
    } else {
        0
    }
}

pub(crate) fn signed_rate_bytes(current: u64, previous: u64, elapsed: Duration) -> i64 {
    let seconds = elapsed.as_secs_f64().max(0.001);
    let delta = current as f64 - previous as f64;
    let value = (delta / seconds).round();
    if !value.is_finite() {
        0
    } else if value > i64::MAX as f64 {
        i64::MAX
    } else if value < i64::MIN as f64 {
        i64::MIN
    } else {
        value as i64
    }
}

pub(crate) fn push_history(
    history: &mut VecDeque<ChartPoint>,
    value: Option<u64>,
    metric: ChartMetric,
    limit: usize,
    thresholds: Thresholds,
) {
    push_history_with_tone(
        history,
        value,
        value
            .map(|value| metric.tone(value, thresholds))
            .unwrap_or(Tone::Muted),
        limit,
    );
}

pub(crate) fn push_history_with_tone(
    history: &mut VecDeque<ChartPoint>,
    value: Option<u64>,
    tone: Tone,
    limit: usize,
) {
    history.push_back(ChartPoint {
        tone: if value.is_some() { tone } else { Tone::Muted },
        value,
        observed_at: SystemTime::now(),
    });
    while history.len() > limit {
        history.pop_front();
    }
}
