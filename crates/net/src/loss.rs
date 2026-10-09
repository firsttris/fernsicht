//! Deterministic packet-loss injection (`--loss 0.01` in the apps, tests).

#[derive(Clone, Debug)]
pub struct LossSim {
    ratio: f64,
    state: u64,
}

impl LossSim {
    pub fn new(ratio: f64, seed: u64) -> Self {
        Self {
            ratio: ratio.clamp(0.0, 1.0),
            state: seed.max(1),
        }
    }

    pub fn ratio(&self) -> f64 {
        self.ratio
    }

    /// Returns `true` if the next packet should be dropped.
    pub fn drop_packet(&mut self) -> bool {
        if self.ratio <= 0.0 {
            return false;
        }
        // xorshift64*
        self.state ^= self.state >> 12;
        self.state ^= self.state << 25;
        self.state ^= self.state >> 27;
        let r = self.state.wrapping_mul(0x2545_F491_4F6C_DD1D);
        ((r >> 11) as f64 / (1u64 << 53) as f64) < self.ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_is_roughly_right() {
        let mut l = LossSim::new(0.05, 42);
        let dropped = (0..100_000).filter(|_| l.drop_packet()).count();
        assert!((4_500..5_500).contains(&dropped), "{dropped}");
        let mut none = LossSim::new(0.0, 1);
        assert!((0..1000).all(|_| !none.drop_packet()));
    }
}
