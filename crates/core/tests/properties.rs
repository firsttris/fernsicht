//! Property tests: the allocation-free structures behave like naive models.

use fernsicht_core::Slot;
use fernsicht_core::latency::{FrameTimings, Stage, Window};
use proptest::prelude::*;

#[derive(Clone, Debug)]
enum Op {
    Put(u32),
    Take,
}

proptest! {
    /// The ring-buffer summary equals a sort over the last N samples.
    #[test]
    fn window_matches_naive(samples in proptest::collection::vec(0u32..1_000_000, 1..400)) {
        let mut w = Window::<64>::default();
        for &v in &samples {
            w.push(v);
        }
        let mut last: Vec<u32> = samples.iter().rev().take(64).copied().collect();
        last.sort_unstable();
        let s = w.summary();
        let sum: u64 = last.iter().map(|&v| u64::from(v)).sum();
        prop_assert_eq!(s.samples, last.len());
        prop_assert_eq!(s.min, last[0]);
        prop_assert_eq!(s.max, *last.last().unwrap());
        prop_assert_eq!(u64::from(s.avg), sum / last.len() as u64);
        let p95 = last[(last.len() * 95).div_ceil(100) - 1];
        prop_assert_eq!(s.p95, p95);
        prop_assert!(s.min <= s.avg && s.avg <= s.max && s.p95 <= s.max);
    }

    /// Single-threaded, the slot is an `Option` with overwrite counting.
    #[test]
    fn slot_matches_option_model(ops in proptest::collection::vec(
        prop_oneof![any::<u32>().prop_map(Op::Put), Just(Op::Take)], 0..200)) {
        let slot = Slot::new();
        let mut model: Option<u32> = None;
        let mut overwritten = 0u64;
        for op in ops {
            match op {
                Op::Put(v) => {
                    let old = model.replace(v);
                    overwritten += u64::from(old.is_some());
                    prop_assert_eq!(slot.put(v), old);
                }
                Op::Take => prop_assert_eq!(slot.try_take(), model.take()),
            }
        }
        prop_assert_eq!(slot.overwritten(), overwritten);
    }

    /// For monotonic timestamps the stages add up to glass-to-glass.
    #[test]
    fn stages_sum_to_total(start in 0i64..1_000_000_000, d in proptest::array::uniform5(0i64..100_000)) {
        let mut t = start;
        let mut next = |i: usize| { t += d[i]; t };
        let timings = FrameTimings {
            captured: start,
            capture_ready: next(0),
            encoded: next(1),
            received: next(2),
            decoded: next(3),
            presented: next(4),
        };
        let sum: u32 = Stage::ALL.iter().map(|s| timings.stage_us(*s)).sum();
        prop_assert_eq!(sum, timings.total_us());
    }
}
