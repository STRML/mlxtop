// SPDX-License-Identifier: MIT
//! Compact operator view of sampled request load; never a billing ledger.
use crate::domain;
use crate::domain::Sample;
use std::collections::VecDeque;
use std::time::SystemTime;

pub(crate) const HISTORY_LIMIT: usize = 240;

#[derive(Clone)]
pub(crate) struct Entry {
    pub(crate) number: u64,
    pub(crate) usage: domain::RequestUsage,
    pub(crate) last_seen: SystemTime,
    pub(crate) output_speed: Option<OutputSpeed>,
}

#[derive(Clone, Copy)]
pub(crate) struct OutputSpeed {
    pub(crate) tps: f64,
    pub(crate) observed_at: SystemTime,
    pub(crate) completed: bool,
}

pub(crate) fn observed_output_speed(
    usage: &domain::RequestUsage,
    observed_at: SystemTime,
) -> Option<OutputSpeed> {
    let tps = usage
        .output_tps
        .filter(|tps| tps.is_finite() && *tps >= 0.0)?;
    Some(OutputSpeed {
        tps,
        observed_at,
        completed: usage.completed,
    })
}

#[derive(Clone, Default)]
pub(super) struct History {
    pub(crate) entries: VecDeque<Entry>,
    pub(crate) next_number: u64,
}

pub(crate) fn same_request(a: &domain::RequestUsage, b: &domain::RequestUsage) -> bool {
    a.id == b.id && a.provider == b.provider && a.model == b.model
}

impl History {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn latest_for(&self, sample: &Sample) -> Option<(&domain::RequestUsage, SystemTime)> {
        self.entries
            .iter()
            .rev()
            .find(|entry| {
                entry.usage.provider == sample.llm_provider && entry.usage.model == sample.llm_model
            })
            .map(|entry| (&entry.usage, entry.last_seen))
    }

    pub fn observe(&mut self, requests: &[domain::RequestUsage]) {
        for usage in requests {
            if let Some(entry) = self
                .entries
                .iter_mut()
                .find(|entry| same_request(&entry.usage, usage))
            {
                // Retained provider responses and repeated file reads do not
                // refresh an observation's age.
                entry.last_seen = usage.observed_at.unwrap_or(entry.last_seen);
                if let Some(speed) = observed_output_speed(usage, entry.last_seen) {
                    entry.output_speed = Some(speed);
                }
                entry.usage = usage.clone();
            } else {
                self.next_number = self.next_number.saturating_add(1);
                let last_seen = usage.observed_at.unwrap_or_else(SystemTime::now);
                self.entries.push_back(Entry {
                    number: self.next_number,
                    usage: usage.clone(),
                    last_seen,
                    output_speed: observed_output_speed(usage, last_seen),
                });
                while self.entries.len() > HISTORY_LIMIT {
                    self.entries.pop_front();
                }
            }
        }
    }
}
