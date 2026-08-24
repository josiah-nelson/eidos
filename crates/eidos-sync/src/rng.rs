//! In-crate deterministic RNG (SplitMix64).
//!
//! Deliberately not an external RNG crate: failing seeds are stored in
//! issues and CI logs as long-lived reproducers, so the stream for a given
//! seed must never change out from under them via a dependency upgrade.
//! Statistical quality only needs to be good enough for fault scheduling.

#[derive(Debug, Clone)]
pub struct DeterministicRng {
    state: u64,
}

impl DeterministicRng {
    pub fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// SplitMix64 step (Steele, Lea, Flood 2014). Passes BigCrush as a
    /// 64-bit generator; one add and three xor-shift-multiplies.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Uniform value in `0..n` (`n > 0`). Modulo bias is irrelevant at the
    /// ranges the simulation uses (≪ 2^32).
    pub fn below(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0);
        self.next_u64() % n
    }

    /// True with probability `permille`/1000. Integer probabilities keep the
    /// stream free of float rounding differences across targets.
    pub fn chance(&mut self, permille: u32) -> bool {
        debug_assert!(permille <= 1000);
        self.below(1000) < u64::from(permille)
    }

    /// Fork an independent stream (for sub-schedules that must not perturb
    /// the parent's draw sequence when their draw count varies).
    pub fn fork(&mut self) -> Self {
        Self::new(self.next_u64())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_is_stable_forever() {
        // These exact values are part of the reproducer contract. If this
        // test fails, stored failing seeds no longer reproduce: do not
        // update the constants — fix the generator.
        // Reference vectors for SplitMix64 with seed 0.
        let mut rng = DeterministicRng::new(0);
        assert_eq!(rng.next_u64(), 0xE220_A839_7B1D_CDAF);
        assert_eq!(rng.next_u64(), 0x6E78_9E6A_A1B9_65F4);
    }

    #[test]
    fn same_seed_same_stream() {
        let mut a = DeterministicRng::new(7);
        let mut b = DeterministicRng::new(7);
        for _ in 0..1000 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn chance_bounds() {
        let mut rng = DeterministicRng::new(1);
        assert!((0..1000).all(|_| !rng.chance(0)));
        assert!((0..1000).all(|_| rng.chance(1000)));
    }
}
