use rand::Rng;
use serde::{Deserialize, Serialize};

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
            && (0.000_001..=100_000_000.0).contains(&self.variance)
    }

    // Sample the discrete normal distribution over the allowed whole minutes.
    // Normalize relative to the mode so narrow or far-tail ranges remain sampleable.
    fn sample(self, upper_bound: u32, rng: &mut impl Rng) -> u32 {
        let max = self.max.min(upper_bound);
        let mode = self.mean.round().clamp(f64::from(self.min), f64::from(max));
        let mode_distance = (mode - self.mean).powi(2);
        let weight = |minute| {
            let distance = (f64::from(minute) - self.mean).powi(2);
            ((mode_distance - distance) / (2.0 * self.variance)).exp()
        };
        let mut target = rng.gen::<f64>() * (self.min..=max).map(weight).sum::<f64>();
        for minute in self.min..=max {
            target -= weight(minute);
            if target < 0.0 {
                return minute;
            }
        }
        max
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AccelerationSettings {
    pub price: i64,
    pub normal: WaitDistribution,
    pub accelerated: WaitDistribution,
}

impl Default for AccelerationSettings {
    fn default() -> Self {
        Self {
            price: 1,
            normal: WaitDistribution {
                min: 15,
                max: 90,
                mean: 45.0,
                variance: 225.0,
            },
            accelerated: WaitDistribution {
                min: 1,
                max: 20,
                mean: 8.0,
                variance: 16.0,
            },
        }
    }
}

#[derive(Debug, Serialize)]
pub struct WaitEstimate {
    pub current_minutes: u32,
    pub accelerated_minutes: u32,
    pub saved_minutes: u32,
}

impl AccelerationSettings {
    pub fn valid(self) -> bool {
        (1..=1_000_000).contains(&self.price)
            && self.normal.valid()
            && self.accelerated.valid()
            && self.accelerated.min <= self.normal.min
    }

    pub fn estimate(self, rng: &mut impl Rng) -> WaitEstimate {
        let current_minutes = self.normal.sample(self.normal.max, rng);
        let accelerated_minutes = self.accelerated.sample(current_minutes, rng);
        WaitEstimate {
            current_minutes,
            accelerated_minutes,
            saved_minutes: current_minutes - accelerated_minutes,
        }
    }
}
