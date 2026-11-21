use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};

pub const ENDURANCE_REFERENCE_TEMPERATURE_C: f64 = 76.5;
pub const ENDURANCE_REFERENCE_DURATION_SECONDS: f64 = 1200.0 * 3600.0;
pub const ENDURANCE_ACTIVATION_ENERGY_EV: f64 = 1.1;
pub const RETENTION_ACTIVATION_ENERGY_EV: f64 = 0.8;
const BOLTZMANN_CONSTANT_EV_PER_KELVIN: f64 = 8.617_1e-5;
const CELSIUS_TO_KELVIN_OFFSET: f64 = 273.15;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct WearWindow {
    pub start_seconds: f64,
    pub end_seconds: f64,
    pub temperature_sample_interval_seconds: f64,
}

#[derive(Clone, Copy, Debug)]
pub struct Sample {
    pub elapsed_seconds: f64,
    pub measured_case_temperature_c: f64,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct RetentionParameters {
    pub retention_baseline_temperature_c: f64,
    pub retention_baseline_duration_hours: f64,
    pub retention_test_temperature_c: f64,
}

#[derive(Debug, Serialize)]
pub struct RetentionEstimate {
    pub parameters: RetentionParameters,
    pub endurance_reference_temperature_c: f64,
    pub endurance_reference_duration_hours: f64,
    pub endurance_activation_energy_ev: f64,
    pub retention_activation_energy_ev: f64,
    pub wear_elapsed_hours: f64,
    pub wear_equivalent_hours_at_reference: f64,
    pub effective_endurance_temperature_c: f64,
    pub wear_adjustment_before_floor: f64,
    pub wear_adjustment: f64,
    pub retention_temperature_factor: f64,
    pub recommended_hours: f64,
    pub successful_temperature_samples: usize,
    pub maximum_accepted_temperature_gap_seconds: f64,
}

pub fn temperature(value: f64) -> Result<f64> {
    ensure!(
        value.is_finite() && value > -CELSIUS_TO_KELVIN_OFFSET,
        "temperature must be finite and above -273.15 °C, got {value} °C"
    );
    Ok(value)
}

fn acceleration(
    comparison_temperature_c: f64,
    reference_temperature_c: f64,
    activation_energy_ev: f64,
) -> f64 {
    let reference_temperature_kelvin = reference_temperature_c + CELSIUS_TO_KELVIN_OFFSET;
    let comparison_temperature_kelvin = comparison_temperature_c + CELSIUS_TO_KELVIN_OFFSET;
    (activation_energy_ev / BOLTZMANN_CONSTANT_EV_PER_KELVIN
        * (1.0 / reference_temperature_kelvin - 1.0 / comparison_temperature_kelvin))
        .exp()
}

pub fn equivalent_wear_seconds(window: WearWindow, samples: &[Sample]) -> Result<f64> {
    let start = window.start_seconds;
    let end = window.end_seconds;
    let maximum_gap = 3.0 * window.temperature_sample_interval_seconds;
    ensure!(
        start.is_finite() && end.is_finite() && end > start,
        "wear window must end after it starts, with finite timestamps (start {start} s, end {end} s)"
    );
    ensure!(
        maximum_gap.is_finite() && maximum_gap > 0.0,
        "temperature sampling interval {} s does not yield a finite positive coverage limit",
        window.temperature_sample_interval_seconds
    );
    let first = samples
        .first()
        .context("no successful case temperature measurements")?;
    let last = samples
        .last()
        .context("no successful case temperature measurements")?;
    for (index, sample) in samples.iter().enumerate() {
        ensure!(
            sample.elapsed_seconds.is_finite(),
            "case temperature sample {} has an invalid elapsed time of {} s",
            index + 1,
            sample.elapsed_seconds
        );
        temperature(sample.measured_case_temperature_c).with_context(|| {
            format!(
                "case temperature sample {} at {:.3} s",
                index + 1,
                sample.elapsed_seconds
            )
        })?;
    }
    if let Some(pair) = samples
        .windows(2)
        .find(|pair| pair[0].elapsed_seconds >= pair[1].elapsed_seconds)
    {
        anyhow::bail!(
            "case temperature timestamps must increase, but {} s follows {} s",
            pair[1].elapsed_seconds,
            pair[0].elapsed_seconds
        );
    }

    let rate = |sample: &Sample| {
        acceleration(
            sample.measured_case_temperature_c,
            ENDURANCE_REFERENCE_TEMPERATURE_C,
            ENDURANCE_ACTIVATION_ENERGY_EV,
        )
    };
    let mut equivalent = 0.0;
    if start < first.elapsed_seconds {
        let delay = first.elapsed_seconds - start;
        ensure!(
            delay <= maximum_gap,
            "first valid case temperature is {delay:.3} s after wear began (maximum gap {maximum_gap:.3} s)"
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
            "case temperature samples at {:.3} s and {:.3} s are {gap:.3} s apart during wear (maximum gap {maximum_gap:.3} s)",
            previous.elapsed_seconds,
            next.elapsed_seconds
        );
        let previous_rate = rate(previous);
        let slope = (rate(next) - previous_rate) / gap;
        let left_rate = previous_rate + slope * (left - previous.elapsed_seconds);
        let right_rate = previous_rate + slope * (right - previous.elapsed_seconds);
        equivalent += (right - left) * (left_rate + right_rate) / 2.0;
    }
    if last.elapsed_seconds < end {
        let delay = end - last.elapsed_seconds;
        ensure!(
            delay <= maximum_gap,
            "last valid case temperature is {delay:.3} s before wear ended (maximum gap {maximum_gap:.3} s)"
        );
        equivalent += (end - start.max(last.elapsed_seconds)) * rate(last);
    }
    ensure!(
        equivalent.is_finite() && equivalent > 0.0,
        "case temperature conversion yielded {equivalent} s of equivalent wear, expected a finite positive duration"
    );
    Ok(equivalent)
}

impl RetentionParameters {
    pub fn validate(self) -> Result<()> {
        temperature(self.retention_baseline_temperature_c)
            .context("invalid --retention-baseline-temperature-c")?;
        temperature(self.retention_test_temperature_c)
            .context("invalid --retention-test-temperature-c")?;
        ensure!(
            self.retention_baseline_duration_hours.is_finite()
                && self.retention_baseline_duration_hours > 0.0,
            "--retention-baseline-duration-hours must be finite and positive, got {} hours",
            self.retention_baseline_duration_hours
        );
        Ok(())
    }

    pub fn estimate(self, window: WearWindow, samples: &[Sample]) -> Result<RetentionEstimate> {
        self.validate()?;
        let equivalent = equivalent_wear_seconds(window, samples)?;
        let elapsed = window.end_seconds - window.start_seconds;
        let wear_adjustment_before_floor = equivalent / ENDURANCE_REFERENCE_DURATION_SECONDS;
        let wear_adjustment = wear_adjustment_before_floor.max(0.5);
        let retention_temperature_factor = acceleration(
            self.retention_test_temperature_c,
            self.retention_baseline_temperature_c,
            RETENTION_ACTIVATION_ENERGY_EV,
        );
        ensure!(
            retention_temperature_factor.is_finite() && retention_temperature_factor > 0.0,
            "retention temperature conversion from {} °C to {} °C yielded an unusable acceleration factor of {retention_temperature_factor}",
            self.retention_baseline_temperature_c,
            self.retention_test_temperature_c,
        );
        let recommended_hours =
            self.retention_baseline_duration_hours * wear_adjustment / retention_temperature_factor;
        ensure!(
            recommended_hours.is_finite() && recommended_hours > 0.0,
            "converting {} baseline hours with temperature factor {retention_temperature_factor} and wear factor {wear_adjustment} yielded {recommended_hours} hours",
            self.retention_baseline_duration_hours
        );
        let reciprocal_effective_kelvin = 1.0
            / (ENDURANCE_REFERENCE_TEMPERATURE_C + CELSIUS_TO_KELVIN_OFFSET)
            - (equivalent / elapsed).ln() * BOLTZMANN_CONSTANT_EV_PER_KELVIN
                / ENDURANCE_ACTIVATION_ENERGY_EV;
        Ok(RetentionEstimate {
            parameters: self,
            endurance_reference_temperature_c: ENDURANCE_REFERENCE_TEMPERATURE_C,
            endurance_reference_duration_hours: ENDURANCE_REFERENCE_DURATION_SECONDS / 3600.0,
            endurance_activation_energy_ev: ENDURANCE_ACTIVATION_ENERGY_EV,
            retention_activation_energy_ev: RETENTION_ACTIVATION_ENERGY_EV,
            wear_elapsed_hours: elapsed / 3600.0,
            wear_equivalent_hours_at_reference: equivalent / 3600.0,
            effective_endurance_temperature_c: 1.0 / reciprocal_effective_kelvin
                - CELSIUS_TO_KELVIN_OFFSET,
            wear_adjustment_before_floor,
            wear_adjustment,
            retention_temperature_factor,
            recommended_hours,
            successful_temperature_samples: samples.len(),
            maximum_accepted_temperature_gap_seconds: 3.0
                * window.temperature_sample_interval_seconds,
        })
    }
}
