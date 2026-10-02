// SPDX-License-Identifier: MIT
//! Human-readable axes derived from visible observations, never model limits.

pub(super) fn nice_step(value: u64) -> u64 {
    let value = value.max(1);
    let power = 10_u64.pow(value.ilog10());
    [1_u64, 2, 5, 10]
        .into_iter()
        .map(|factor| power.saturating_mul(factor))
        .find(|step| *step >= value)
        .unwrap_or(u64::MAX)
}

pub(super) fn ceiling(peak: u64, minimum: u64) -> u64 {
    let padded = peak.saturating_add(peak.div_ceil(10)).max(minimum);
    let step = nice_step(padded.div_ceil(5));
    padded.div_ceil(step).saturating_mul(step)
}

pub(super) fn range(values: impl Iterator<Item = u64>, minimum_span: u64) -> (u64, u64) {
    let Some((low, high)) = values
        .map(|value| (value, value))
        .reduce(|(low, high), (value, _)| (low.min(value), high.max(value)))
    else {
        return (0, minimum_span.max(1));
    };
    let spread = high - low;
    let span = spread.max(high / 5).max(minimum_span).max(1);
    let padding = ((span - spread) / 2).saturating_add(span.div_ceil(10));
    let step = nice_step(span.div_ceil(5));
    let lower = low.saturating_sub(padding) / step * step;
    let upper = high
        .saturating_add(padding)
        .div_ceil(step)
        .saturating_mul(step);
    (lower, upper.max(lower.saturating_add(1)))
}

#[cfg(test)]
#[path = "tests/chart_scale.rs"]
mod tests;
