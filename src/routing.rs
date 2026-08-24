use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

const GOLDEN_GAMMA: u64 = 0x9e37_79b9_7f4a_7c15;

pub struct RandomSelector {
    state: AtomicU64,
}

impl Default for RandomSelector {
    fn default() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos() as u64)
            .unwrap_or(GOLDEN_GAMMA);
        Self::with_seed(nanos ^ u64::from(std::process::id()))
    }
}

impl RandomSelector {
    pub const fn with_seed(seed: u64) -> Self {
        Self {
            state: AtomicU64::new(seed),
        }
    }

    pub fn order(&self, len: usize) -> Vec<usize> {
        let mut order = (0..len).collect::<Vec<_>>();
        if len <= 1 {
            return order;
        }
        let mut state = self.state.fetch_add(GOLDEN_GAMMA, Ordering::Relaxed);
        for upper in (1..len).rev() {
            let selected = bounded(&mut state, upper + 1);
            order.swap(upper, selected);
        }
        order
    }

    pub fn range_inclusive(&self, minimum: u64, maximum: u64) -> u64 {
        assert!(minimum <= maximum, "random range must not be inverted");
        let width = maximum
            .checked_sub(minimum)
            .and_then(|difference| difference.checked_add(1))
            .expect("random range width must fit in u64");
        let mut state = self.state.fetch_add(GOLDEN_GAMMA, Ordering::Relaxed);
        minimum + bounded_u64(&mut state, width)
    }
}

fn bounded(state: &mut u64, upper: usize) -> usize {
    debug_assert!(upper > 0);
    bounded_u64(state, upper as u64) as usize
}

fn bounded_u64(state: &mut u64, upper: u64) -> u64 {
    debug_assert!(upper > 0);
    let zone = u64::MAX - u64::MAX % upper;
    loop {
        let value = splitmix64(state);
        if value < zone {
            return value % upper;
        }
    }
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(GOLDEN_GAMMA);
    let mut value = *state;
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_random_work_for_zero_or_one_target() {
        let selector = RandomSelector::with_seed(7);
        assert!(selector.order(0).is_empty());
        assert_eq!(selector.order(1), vec![0]);
    }

    #[test]
    fn returns_each_target_exactly_once() {
        let selector = RandomSelector::with_seed(42);
        for _ in 0..100 {
            let mut order = selector.order(9);
            order.sort_unstable();
            assert_eq!(order, (0..9).collect::<Vec<_>>());
        }
    }

    #[test]
    fn visits_every_target_as_the_first_choice() {
        let selector = RandomSelector::with_seed(1234);
        let mut first_counts = [0_u32; 3];
        for _ in 0..3_000 {
            first_counts[selector.order(3)[0]] += 1;
        }
        assert!(first_counts.iter().all(|count| *count > 850));
        assert!(first_counts.iter().all(|count| *count < 1_150));
    }

    #[test]
    fn inclusive_ranges_stay_within_bounds() {
        let selector = RandomSelector::with_seed(99);
        for _ in 0..1_000 {
            assert!((200..=500).contains(&selector.range_inclusive(200, 500)));
        }
        assert_eq!(selector.range_inclusive(7, 7), 7);
    }
}
