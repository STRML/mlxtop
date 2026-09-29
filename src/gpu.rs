// SPDX-License-Identifier: MIT
//! Per-device NVIDIA counters. Device memory is separate from Apple Metal accounting.
use std::collections::HashSet;

use crate::{command_text, MIB};

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
pub(crate) fn collect(previous: &[Device]) -> Vec<Device> {
    if !cfg!(target_os = "linux") {
        return Vec::new();
    }
    let output = command_text(
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
mod tests {
    use super::*;

    const TWO: &str = "1, GPU-b, NVIDIA RTX 4090, 97, 22000, 24564, 78\n0, GPU-a, NVIDIA RTX 4090, 0, 1024, 24564, 35\n";

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn nvidia_collection_is_disabled_outside_linux() {
        assert!(collect(&parse(TWO)).is_empty());
    }

    #[test]
    fn keeps_every_card_sorted_without_merging_identical_models() {
        let cards = parse(TWO);
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].index, 0);
        assert_eq!(cards[0].uuid, "GPU-a");
        assert_eq!(cards[1].uuid, "GPU-b");
        assert_eq!(cards[1].temperature, Some(78));
        assert_eq!(cards[1].used, Some(22000 * MIB));
        assert_eq!(peak_utilization(&cards), Some(97));
        assert_eq!(memory_totals(&cards), Some((23024 * MIB, 49128 * MIB)));
    }

    #[test]
    fn unsupported_fields_and_bad_rows_do_not_become_zero_or_hide_other_cards() {
        let cards = parse(&format!(
            "garbage\n{TWO}2, GPU-c, \"NVIDIA, test\", [N/A], N/A, 8192, [Not Supported]\n"
        ));
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].utilization, Some(0));
        assert_eq!(cards[2].name, "NVIDIA, test");
        assert_eq!(cards[2].utilization, None);
        assert_eq!(cards[2].used, None);
        assert_eq!(cards[2].temperature, None);
        assert_eq!(peak_utilization(&cards), None);
        assert_eq!(memory_totals(&cards), None);
        let bad = parse("0, GPU-a, Test, 101, -1, 18446744073709551615, N/A");
        assert_eq!(bad[0].utilization, None);
        assert_eq!(bad[0].used, None);
        assert_eq!(bad[0].total, None);
        assert_eq!(bad[0].memory_percent(), None);
    }

    #[test]
    fn failed_poll_clears_counters_and_recovers_by_uuid() {
        let original = parse(TWO);
        for output in [None, Some(""), Some("Failed to initialize NVML")] {
            let missing = readings(output, &original);
            assert_eq!(missing.len(), 2);
            assert_eq!(missing[0].uuid, original[0].uuid);
            assert_eq!(missing[1].utilization, None);
            assert_eq!(missing[1].used, None);
            assert_eq!(missing[1].total, None);
            assert_eq!(missing[1].temperature, None);
            assert_eq!(readings(Some(TWO), &missing), original);
        }
        let removed = readings(
            Some("3, GPU-b, NVIDIA RTX 4090, 50, 2000, 24564, 45"),
            &original,
        );
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].uuid, "GPU-b");
        assert_eq!(removed[0].index, 3);
        assert_eq!(parse(&format!("{TWO}{TWO}")).len(), 2);
        assert_eq!(peak_utilization(&[]), None);
    }
}
