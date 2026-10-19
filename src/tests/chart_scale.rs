// SPDX-License-Identifier: MIT
use super::*;

#[test]
fn ceilings_round_up_to_a_readable_step_and_never_reach_zero() {
    assert_eq!(ceiling(837, 1), 1000);
    assert_eq!(ceiling(0, 1), 1);
    // Rate charts stand on zero: the visible peak sets the top.
    assert_eq!(ceiling(360, 10), 400);
    assert!(ceiling(u64::MAX / 2, 1) >= u64::MAX / 2);
}
