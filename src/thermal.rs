use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const WEAR_TARGET_C: f64 = 76.5;
pub const WEAR_TARGET_SECONDS: f64 = 1200.0 * 3600.0;
pub const WEAR_ACTIVATION_EV: f64 = 1.1;
pub const RETENTION_ACTIVATION_EV: f64 = 0.8;
const BOLTZMANN_EV_PER_K: f64 = 8.617_1e-5;
const CELSIUS_ZERO_KELVIN: f64 = 273.15;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct WearWindow {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub sample_interval_seconds: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub elapsed_seconds: f64,
    pub temperature_c: f64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RetentionParameters {
    pub baseline_temp_c: f64,
    pub baseline_hours: f64,
    pub retention_temp_c: f64,
}

#[derive(Debug, Serialize)]
pub struct RetentionEstimate {
    pub parameters: RetentionParameters,
    pub wear_reference_c: f64,
    pub wear_reference_hours: f64,
    pub wear_activation_ev: f64,
    pub retention_activation_ev: f64,
    pub wear_elapsed_hours: f64,
    pub wear_equivalent_hours_at_reference: f64,
    pub wear_effective_c: f64,
    pub wear_adjustment_before_floor: f64,
    pub wear_adjustment: f64,
    pub retention_temperature_factor: f64,
    pub recommended_hours: f64,
    pub successful_temperature_samples: usize,
    pub maximum_accepted_temperature_gap_seconds: f64,
}

pub fn temperature(value: f64) -> Result<f64> {
    ensure!(
        value.is_finite() && value > -CELSIUS_ZERO_KELVIN,
        "temperature in °C must be finite and above absolute zero"
    );
    Ok(value)
}

fn acceleration(value_c: f64, reference_c: f64, activation_ev: f64) -> f64 {
    let reference_k = reference_c + CELSIUS_ZERO_KELVIN;
    let value_k = value_c + CELSIUS_ZERO_KELVIN;
    (activation_ev / BOLTZMANN_EV_PER_K * (1.0 / reference_k - 1.0 / value_k)).exp()
}

pub fn equivalent_wear_seconds(window: WearWindow, samples: &[Sample]) -> Result<f64> {
    let start = window.start_seconds;
    let end = window.end_seconds;
    let maximum_gap = 3.0 * window.sample_interval_seconds;
    ensure!(
        start.is_finite()
            && end.is_finite()
            && end > start
            && maximum_gap.is_finite()
            && maximum_gap > 0.0,
        "invalid wear window"
    );
    let first = samples
        .first()
        .context("no successful case temperature measurements")?;
    let last = samples
        .last()
        .context("no successful case temperature measurements")?;
    ensure!(
        samples
            .iter()
            .all(|s| s.elapsed_seconds.is_finite() && temperature(s.temperature_c).is_ok())
            && samples
                .windows(2)
                .all(|pair| pair[0].elapsed_seconds < pair[1].elapsed_seconds),
        "invalid case temperature timeline"
    );

    let rate =
        |sample: &Sample| acceleration(sample.temperature_c, WEAR_TARGET_C, WEAR_ACTIVATION_EV);
    let mut equivalent = 0.0;
    if start < first.elapsed_seconds {
        ensure!(
            first.elapsed_seconds - start <= maximum_gap,
            "temperature measurements begin too late to cover the wear window"
        );
        equivalent += (end.min(first.elapsed_seconds) - start) * rate(first);
    }
    for pair in samples.windows(2) {
        let previous = &pair[0];
        let next = &pair[1];
        let left = start.max(previous.elapsed_seconds);
        let right = end.min(next.elapsed_seconds);
        if left >= right {
            continue;
        }
        let gap = next.elapsed_seconds - previous.elapsed_seconds;
        ensure!(
            gap <= maximum_gap,
            "temperature measurement gap of {gap:.3}s in the wear window"
        );
        let previous_rate = rate(previous);
        let slope = (rate(next) - previous_rate) / gap;
        let left_rate = previous_rate + slope * (left - previous.elapsed_seconds);
        let right_rate = previous_rate + slope * (right - previous.elapsed_seconds);
        equivalent += (right - left) * (left_rate + right_rate) / 2.0;
    }
    if last.elapsed_seconds < end {
        ensure!(
            end - last.elapsed_seconds <= maximum_gap,
            "temperature measurements end too early to cover the wear window"
        );
        equivalent += (end - start.max(last.elapsed_seconds)) * rate(last);
    }
    ensure!(
        equivalent.is_finite() && equivalent > 0.0,
        "invalid equivalent wear time"
    );
    Ok(equivalent)
}

impl RetentionParameters {
    pub fn validate(self) -> Result<()> {
        temperature(self.baseline_temp_c)?;
        temperature(self.retention_temp_c)?;
        ensure!(
            self.baseline_hours.is_finite() && self.baseline_hours > 0.0,
            "baseline retention hours must be finite and positive"
        );
        Ok(())
    }

    pub fn estimate(self, window: WearWindow, samples: &[Sample]) -> Result<RetentionEstimate> {
        self.validate()?;
        let equivalent = equivalent_wear_seconds(window, samples)?;
        let elapsed = window.end_seconds - window.start_seconds;
        let wear_adjustment_before_floor = equivalent / WEAR_TARGET_SECONDS;
        let wear_adjustment = wear_adjustment_before_floor.max(0.5);
        let retention_temperature_factor = acceleration(
            self.retention_temp_c,
            self.baseline_temp_c,
            RETENTION_ACTIVATION_EV,
        );
        let recommended_hours =
            self.baseline_hours * wear_adjustment / retention_temperature_factor;
        ensure!(
            retention_temperature_factor.is_finite()
                && recommended_hours.is_finite()
                && recommended_hours > 0.0,
            "retention temperatures or duration are outside the calculation range"
        );
        let inverse_effective_k = 1.0 / (WEAR_TARGET_C + CELSIUS_ZERO_KELVIN)
            - (equivalent / elapsed).ln() * BOLTZMANN_EV_PER_K / WEAR_ACTIVATION_EV;
        Ok(RetentionEstimate {
            parameters: self,
            wear_reference_c: WEAR_TARGET_C,
            wear_reference_hours: WEAR_TARGET_SECONDS / 3600.0,
            wear_activation_ev: WEAR_ACTIVATION_EV,
            retention_activation_ev: RETENTION_ACTIVATION_EV,
            wear_elapsed_hours: elapsed / 3600.0,
            wear_equivalent_hours_at_reference: equivalent / 3600.0,
            wear_effective_c: 1.0 / inverse_effective_k - CELSIUS_ZERO_KELVIN,
            wear_adjustment_before_floor,
            wear_adjustment,
            retention_temperature_factor,
            recommended_hours,
            successful_temperature_samples: samples.len(),
            maximum_accepted_temperature_gap_seconds: 3.0 * window.sample_interval_seconds,
        })
    }
}
