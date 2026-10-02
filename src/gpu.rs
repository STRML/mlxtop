// SPDX-License-Identifier: MIT
//! Per-device NVIDIA counters. Device memory is separate from Apple Metal accounting.
use std::collections::HashSet;

use crate::host::Host;
use crate::MIB;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device {
    pub index: u32,
    pub uuid: String,
    pub name: String,
    pub utilization: Option<u8>,
    pub used: Option<u64>,
    pub total: Option<u64>,
    pub temperature: Option<u64>,
}

impl Device {
    pub fn memory_percent(&self) -> Option<u16> {
        let (used, total) = self.used.zip(self.total)?;
        (total > 0).then(|| ((u128::from(used) * 100 / u128::from(total)).min(100)) as u16)
    }

    fn unavailable(&self) -> Self {
        Self {
            utilization: None,
            used: None,
            total: None,
            temperature: None,
            ..self.clone()
        }
    }
}

/// Query all physical devices in one invocation. UUIDs distinguish identical
/// models and survive driver enumeration changes; indices remain familiar labels.
pub(crate) fn collect(host: &dyn Host, previous: &[Device]) -> Vec<Device> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let output = host.command(
        "nvidia-smi",
        &[
            "--query-gpu=index,uuid,name,utilization.gpu,memory.used,memory.total,temperature.gpu",
            "--format=csv,noheader,nounits",
        ],
    );
    readings(output.as_deref(), previous)
}

fn readings(output: Option<&str>, previous: &[Device]) -> Vec<Device> {
    let devices = output.map(parse).unwrap_or_default();
    if devices.is_empty() {
        // A failed/invalid read is not a healthy zero, and must not erase the
        // panel. Retain identity only, never the last successful counters.
        previous.iter().map(Device::unavailable).collect()
    } else {
        devices
    }
}

pub(crate) fn parse(text: &str) -> Vec<Device> {
    let mut seen = HashSet::new();
    let mut devices: Vec<_> = text
        .lines()
        .filter_map(|line| {
            let (index, rest) = line.split_once(',')?;
            let index = index.trim().parse().ok()?;
            let (uuid, rest) = rest.split_once(',')?;
            let uuid = uuid.trim();
            if !uuid.starts_with("GPU-") || uuid.chars().any(char::is_control) {
                return None;
            }
            // The last four fields are numeric. Splitting from the right also
            // preserves a quoted device name containing commas.
            let mut fields = rest.rsplitn(5, ',');
            let temperature = number(fields.next()?);
            let total = number(fields.next()?).and_then(|n| n.checked_mul(MIB));
            let used = number(fields.next()?).and_then(|n| n.checked_mul(MIB));
            let utilization = number(fields.next()?)
                .filter(|n| *n <= 100)
                .map(|n| n as u8);
            let name: String = fields
                .next()?
                .trim()
                .trim_matches('"')
                .chars()
                .filter(|c| !c.is_control())
                .take(120)
                .collect();
            if name.is_empty() || !seen.insert(uuid.to_owned()) {
                return None;
            }
            Some(Device {
                index,
                uuid: uuid.into(),
                name,
                utilization,
                used,
                total,
                temperature,
            })
        })
        .collect();
    devices.sort_by(|a, b| a.index.cmp(&b.index).then(a.uuid.cmp(&b.uuid)));
    devices
}

fn number(field: &str) -> Option<u64> {
    field.trim().parse().ok()
}

/// A summary must cover all devices. One missing reading makes the summary
/// unavailable; the remaining measured cards are still visible individually.
pub(crate) fn peak_utilization(devices: &[Device]) -> Option<u8> {
    if devices.is_empty() {
        return None;
    }
    devices
        .iter()
        .try_fold(0, |peak, gpu| Some(peak.max(gpu.utilization?)))
}

pub(crate) fn memory_totals(devices: &[Device]) -> Option<(u64, u64)> {
    if devices.is_empty() {
        return None;
    }
    devices
        .iter()
        .try_fold((0_u64, 0_u64), |(used, total), gpu| {
            Some((used.checked_add(gpu.used?)?, total.checked_add(gpu.total?)?))
        })
}

#[cfg(test)]
#[path = "tests/gpu.rs"]
mod tests;
