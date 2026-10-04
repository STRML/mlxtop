// SPDX-License-Identifier: MIT
//! Bounded OS/queue samples and explicitly reported request latency.
use crate::domain::{Sample, TelemetrySource};
use std::collections::VecDeque;
use std::time::{Duration, SystemTime};

#[derive(Clone, Default)]
pub(crate) struct Point {
    pub(crate) active: Option<u64>,
    pub(crate) waiting: Option<u64>,
    pub(crate) provider: String,
}

#[derive(Clone, Default)]
pub(super) struct History {
    pub(crate) points: VecDeque<Point>,
    pub(crate) timings: VecDeque<(String, Option<u64>, SystemTime)>,
}

impl History {
    pub fn observe(&mut self, sample: &Sample, limit: usize) {
        let fresh = sample.llm_source == TelemetrySource::Live
            && sample.llm_status != "stale"
            && sample.llm_observed_at.is_some_and(|at| {
                SystemTime::now()
                    .duration_since(at)
                    .is_ok_and(|age| age <= Duration::from_secs(5))
            });
        self.points.push_back(Point {
            active: fresh.then_some(sample.llm_active_requests).flatten(),
            waiting: fresh.then_some(sample.llm_waiting_requests).flatten(),
            provider: sample.llm_provider.clone(),
        });
        while self.points.len() > limit {
            self.points.pop_front();
        }
        for request in &sample.llm_requests {
            let key = format!("{}\0{}\0{}", request.provider, request.model, request.id);
            if let Some(old) = self.timings.iter_mut().find(|old| old.0 == key) {
                if let Some(ttft) = request.ttft_ms {
                    old.1 = Some(ttft);
                    old.2 = request.observed_at.unwrap_or(old.2);
                }
            } else {
                self.timings.push_back((
                    key,
                    request.ttft_ms,
                    request.observed_at.unwrap_or_else(SystemTime::now),
                ));
            }
        }
        while self.timings.len() > 240 {
            self.timings.pop_front();
        }
    }

    pub fn has_latency(&self) -> bool {
        self.timings.iter().any(|p| p.1.is_some())
    }
}
