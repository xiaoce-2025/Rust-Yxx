use rand::{rngs::StdRng, Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MANUAL_PRIORITY: u32 = 4;
pub const ACCELERATED_PRIORITY: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WaitDistribution {
    pub min: u32,
    pub max: u32,
    pub mean: f64,
    pub variance: f64,
}

impl WaitDistribution {
    fn valid(self) -> bool {
        self.min <= self.max
            && self.max <= 10_080
            && (f64::from(self.min)..=f64::from(self.max)).contains(&self.mean)
            && (0.0..=100_000_000.0).contains(&self.variance)
    }

    // Sample the discrete normal distribution over the allowed whole minutes.
    // Normalize relative to the mode so narrow or far-tail ranges remain sampleable.
    pub(super) fn sampler(self) -> ProcessingSampler {
        let mode = self.mean.round();
        if self.min == self.max || self.variance == 0.0 {
            return ProcessingSampler {
                min: mode as u32,
                cumulative: Vec::new(),
            };
        }
        let mode_distance = (mode - self.mean).powi(2);
        let mut total = 0.0;
        let cumulative = (self.min..=self.max)
            .map(|minute| {
                let distance = (f64::from(minute) - self.mean).powi(2);
                total += ((mode_distance - distance) / (2.0 * self.variance)).exp();
                total
            })
            .collect();
        ProcessingSampler {
            min: self.min,
            cumulative,
        }
    }
}

pub(super) struct ProcessingSampler {
    min: u32,
    cumulative: Vec<f64>,
}

impl ProcessingSampler {
    pub(super) fn seconds(&self, id: Uuid) -> u64 {
        let Some(&total) = self.cumulative.last() else {
            return u64::from(self.min) * 60;
        };
        // A job gets the same sample across refreshes and both sides of the quote.
        let mut seed = [0; 32];
        seed[..16].copy_from_slice(id.as_bytes());
        let target = StdRng::from_seed(seed).gen::<f64>() * total;
        let index = self.cumulative.partition_point(|weight| *weight <= target);
        (u64::from(self.min) + index.min(self.cumulative.len() - 1) as u64) * 60
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccelerationSettings {
    pub price: i64,
    pub processing_time: WaitDistribution,
}

impl Default for AccelerationSettings {
    fn default() -> Self {
        Self {
            price: 1,
            processing_time: WaitDistribution {
                min: 15,
                max: 90,
                mean: 45.0,
                variance: 225.0,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WaitEstimate {
    pub current_minutes: u64,
    pub accelerated_minutes: u64,
    pub saved_minutes: u64,
}

impl AccelerationSettings {
    pub fn valid(self) -> bool {
        (1..=1_000_000).contains(&self.price) && self.processing_time.valid()
    }
}

impl WaitEstimate {
    pub(super) fn from_seconds(current: u64, accelerated: u64) -> Self {
        let current_minutes = current.div_ceil(60);
        let accelerated_minutes = accelerated.div_ceil(60);
        Self {
            current_minutes,
            accelerated_minutes,
            saved_minutes: current_minutes.saturating_sub(accelerated_minutes),
        }
    }
}
