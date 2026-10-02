// SPDX-License-Identifier: MIT
use super::*;

const TWO: &str = "1, GPU-b, NVIDIA RTX 4090, 97, 22000, 24564, 78\n0, GPU-a, NVIDIA RTX 4090, 0, 1024, 24564, 35\n";

#[cfg(not(target_os = "linux"))]
#[test]
fn nvidia_collection_is_disabled_outside_linux() {
    let host = crate::test_support::FakeHost::default();
    assert!(collect(&host, &parse(TWO)).is_empty());
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
