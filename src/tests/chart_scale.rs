// SPDX-License-Identifier: MIT
use super::*;

#[test]
fn axes_fit_small_large_zero_and_constant_workloads_without_clipping() {
    for values in [
        vec![350, 355, 360],
        vec![1, 2, 3],
        vec![0],
        vec![20000],
        vec![u64::MAX],
    ] {
        let (low, high) = range(values.iter().copied(), 10);
        assert!(high > low);
        assert!(values.iter().all(|value| *value >= low && *value <= high));
    }
    let (low, high) = range([350, 355, 360].into_iter(), 10);
    assert!(low >= 250 && high <= 450, "35 tok/s needs a useful range");
    assert_eq!(range([].into_iter(), 10), (0, 10));
    assert_eq!(ceiling(837, 1), 1000);
    assert_eq!(ceiling(0, 1), 1);
}
