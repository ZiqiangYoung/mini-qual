use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};

pub const MAXIMUM_SUPPORTED_CAPACITY_BYTES: u64 = 2 << 50;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Budget {
    pub nand_capacity_blocks: u64,
    pub host_visible_capacity_blocks: u64,
    pub total_host_write_budget_blocks: u128,
    pub endurance_write_budget_blocks: u128,
    pub full_rounds: u128,
    pub final_partial_round_blocks: u64,
}

impl Budget {
    pub fn new(
        pe: u64,
        nand_capacity_bytes: u64,
        host_visible_capacity_bytes: u64,
        logical_block_bytes: u64,
    ) -> Result<Self> {
        ensure!(pe > 0, "PE must be positive");
        ensure!(
            nand_capacity_bytes <= MAXIMUM_SUPPORTED_CAPACITY_BYTES,
            "--nand-capacity is {nand_capacity_bytes} bytes, above the 2 PiB limit"
        );
        ensure!(
            nand_capacity_bytes > host_visible_capacity_bytes,
            "--nand-capacity is {nand_capacity_bytes} bytes but must exceed the device's host-visible capacity of {host_visible_capacity_bytes} bytes"
        );
        ensure!(
            logical_block_bytes > 0,
            "the device reports a logical block size of 0 bytes"
        );
        ensure!(
            host_visible_capacity_bytes > 0
                && host_visible_capacity_bytes % logical_block_bytes == 0,
            "the device's host-visible capacity of {host_visible_capacity_bytes} bytes must be a positive multiple of its {logical_block_bytes}-byte logical block size"
        );
        ensure!(
            nand_capacity_bytes % logical_block_bytes == 0,
            "--nand-capacity is {nand_capacity_bytes} bytes but must be a multiple of the device's {logical_block_bytes}-byte logical block size"
        );
        let nand_capacity_blocks = nand_capacity_bytes / logical_block_bytes;
        let host_visible_capacity_blocks = host_visible_capacity_bytes / logical_block_bytes;
        let total_host_write_budget_blocks = u128::from(pe) * u128::from(nand_capacity_blocks);
        let endurance_write_budget_blocks =
            total_host_write_budget_blocks - u128::from(host_visible_capacity_blocks);
        Ok(Self {
            nand_capacity_blocks,
            host_visible_capacity_blocks,
            total_host_write_budget_blocks,
            endurance_write_budget_blocks,
            full_rounds: endurance_write_budget_blocks / u128::from(host_visible_capacity_blocks),
            final_partial_round_blocks: (endurance_write_budget_blocks
                % u128::from(host_visible_capacity_blocks))
                as u64,
        })
    }
}

pub fn parse_pe(value: &str) -> Result<u64> {
    let value = value.trim();
    let number = value.strip_suffix(['K', 'k']).unwrap_or(value).trim_end();
    decimal_units(number, 1000).map_err(|error| {
        anyhow!("--pe is in thousands of cycles (e.g. 3.25 means 3250 cycles): {error:#}")
    })
}

pub fn parse_capacity(value: &str) -> Result<u64> {
    let value = value.trim();
    let unit_start = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(value.len());
    ensure!(
        unit_start > 0,
        "capacity must start with a positive decimal number (e.g. 4TiB)"
    );
    let unit = value[unit_start..].trim().to_ascii_lowercase();
    let multiplier = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1,
        "kib" => 1 << 10,
        "mib" => 1 << 20,
        "gib" => 1 << 30,
        "tib" => 1 << 40,
        "pib" => 1 << 50,
        "kb" => 1_000,
        "mb" => 1_000_000,
        "gb" => 1_000_000_000,
        "tb" => 1_000_000_000_000,
        "pb" => 1_000_000_000_000_000,
        _ => bail!(
            "unsupported capacity unit {unit:?}. Use B, KiB, MiB, GiB, TiB, PiB or their decimal equivalents"
        ),
    };
    let bytes = decimal_units(&value[..unit_start], multiplier)
        .map_err(|error| anyhow!("capacity must represent a whole number of bytes: {error:#}"))?;
    ensure!(
        bytes <= MAXIMUM_SUPPORTED_CAPACITY_BYTES,
        "capacity is {bytes} bytes, above the 2 PiB limit"
    );
    Ok(bytes)
}

fn decimal_units(number: &str, multiplier: u64) -> Result<u64> {
    let (whole, fraction) = number.split_once('.').unwrap_or((number, ""));
    ensure!(
        !(whole.is_empty() && fraction.is_empty())
            && whole
                .bytes()
                .chain(fraction.bytes())
                .all(|b| b.is_ascii_digit()),
        "expected a positive decimal number"
    );
    let whole = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u64>()
            .context("integer part exceeds the supported range")?
    };
    let fraction = fraction.trim_end_matches('0');
    let mut expanded = u128::from(whole) * u128::from(multiplier);
    if !fraction.is_empty() {
        let divisor = 10_u128
            .checked_pow(fraction.len().try_into()?)
            .context("decimal precision is too large")?;
        let numerator = fraction
            .parse::<u128>()?
            .checked_mul(u128::from(multiplier))
            .context("decimal precision is too large")?;
        ensure!(
            numerator % divisor == 0,
            "the value is fractional after unit conversion"
        );
        expanded += numerator / divisor;
    }
    ensure!(expanded > 0, "value must be positive");
    expanded.try_into().context("value is too large")
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
pub struct RoundTiming {
    pub trim_seconds: f64,
    pub write_seconds: f64,
    pub flush_seconds: f64,
    pub read_seconds: f64,
    pub locate_seconds: f64,
    pub phase_gap_seconds: f64,
    pub round_gap_seconds: Option<f64>,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct TimingModel {
    pub complete_rounds: usize,
    pub average: RoundTiming,
}

impl TimingModel {
    pub fn from_rounds(rounds: &[RoundTiming]) -> Result<Self> {
        ensure!(
            rounds.len() >= 2,
            "at least two complete rounds are required"
        );
        let mut trim = 0.0;
        let mut write = 0.0;
        let mut flush = 0.0;
        let mut read = 0.0;
        let mut locate = 0.0;
        let mut phase_gap = 0.0;
        let mut round_gap = 0.0;
        for round in rounds {
            trim += round.trim_seconds;
            write += round.write_seconds;
            flush += round.flush_seconds;
            read += round.read_seconds;
            locate += round.locate_seconds;
            phase_gap += round.phase_gap_seconds;
            round_gap += round.round_gap_seconds.unwrap_or(0.0);
        }
        let count = rounds.len() as f64;
        Ok(Self {
            complete_rounds: rounds.len(),
            average: RoundTiming {
                trim_seconds: trim / count,
                write_seconds: write / count,
                flush_seconds: flush / count,
                read_seconds: read / count,
                locate_seconds: locate / count,
                phase_gap_seconds: phase_gap / count,
                round_gap_seconds: Some(round_gap / (count - 1.0)),
            },
        })
    }

    pub fn predict_wear_seconds(
        &self,
        write_bytes: u128,
        measured_round_bytes: u64,
    ) -> Result<f64> {
        ensure!(
            measured_round_bytes > 0,
            "the measured pretest round size must be positive"
        );
        let round_bytes = u128::from(measured_round_bytes);
        let full_rounds = write_bytes / round_bytes;
        let final_round_bytes = write_bytes % round_bytes;
        let fixed =
            self.average.trim_seconds + self.average.flush_seconds + self.average.phase_gap_seconds;
        let scalable =
            self.average.write_seconds + self.average.read_seconds + self.average.locate_seconds;
        let mut seconds = (fixed + scalable) * full_rounds as f64;
        if final_round_bytes > 0 {
            seconds += fixed + scalable * (final_round_bytes as f64 / measured_round_bytes as f64);
        }
        let rounds = full_rounds + u128::from(final_round_bytes > 0);
        Ok(seconds
            + self.average.round_gap_seconds.unwrap_or(0.0) * rounds.saturating_sub(1) as f64)
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CapacityRange {
    pub pretest_nand_capacity_bytes: u64,
    pub nand_capacity_step_bytes: u64,
    pub minimum_nand_capacity_bytes: u64,
    pub maximum_nand_capacity_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct CapacityEstimate {
    pub nand_capacity_bytes: u64,
    pub nand_capacity_step_offset: i128,
    pub predicted_wear_seconds: f64,
    pub baseline_wear_seconds: f64,
    pub predicted_difference_seconds: f64,
}

#[derive(Debug)]
pub enum CapacitySelection {
    Recommended(CapacityEstimate),
    NoCapacityWithinLimit {
        minimum_legal_capacity: CapacityEstimate,
    },
}

impl CapacityRange {
    fn aligned_candidates(self, logical_block_bytes: u64) -> Result<(u64, u64, u64)> {
        ensure!(
            logical_block_bytes > 0,
            "logical block size must be positive"
        );
        ensure!(
            self.nand_capacity_step_bytes > 0,
            "--nand-capacity-step must be greater than 0 bytes"
        );
        ensure!(
            self.pretest_nand_capacity_bytes > 0
                && self.pretest_nand_capacity_bytes <= MAXIMUM_SUPPORTED_CAPACITY_BYTES,
            "--pretest-nand-capacity is {} bytes, expected 1 byte to 2 PiB",
            self.pretest_nand_capacity_bytes
        );
        ensure!(
            self.maximum_nand_capacity_bytes <= MAXIMUM_SUPPORTED_CAPACITY_BYTES,
            "--maximum-nand-capacity is {} bytes, above the 2 PiB limit",
            self.maximum_nand_capacity_bytes
        );
        ensure!(
            self.minimum_nand_capacity_bytes <= self.maximum_nand_capacity_bytes,
            "--minimum-nand-capacity ({} bytes) exceeds --maximum-nand-capacity ({} bytes)",
            self.minimum_nand_capacity_bytes,
            self.maximum_nand_capacity_bytes
        );
        ensure!(
            self.minimum_nand_capacity_bytes > 0,
            "--minimum-nand-capacity must be greater than 0 bytes"
        );
        let lower = self.minimum_nand_capacity_bytes;
        let upper = self.maximum_nand_capacity_bytes;
        let remainder = self.pretest_nand_capacity_bytes % self.nand_capacity_step_bytes;
        let (gcd, inverse) = gcd_coefficient(
            self.nand_capacity_step_bytes.into(),
            logical_block_bytes.into(),
        );
        ensure!(
            i128::from(remainder) % gcd == 0,
            "no capacity reached from --pretest-nand-capacity ({} bytes) in steps of {} bytes can align to {logical_block_bytes}-byte logical blocks",
            self.pretest_nand_capacity_bytes,
            self.nand_capacity_step_bytes
        );
        // The starting remainder plus this many NAND capacity steps must be a
        // multiple of the logical block size.
        let modulus = i128::from(logical_block_bytes) / gcd;
        let steps_to_first_alignment =
            ((-i128::from(remainder) / gcd) * inverse).rem_euclid(modulus);
        let first_aligned = u128::from(remainder)
            + steps_to_first_alignment as u128 * u128::from(self.nand_capacity_step_bytes);
        let aligned_candidate_spacing_bytes =
            modulus as u128 * u128::from(self.nand_capacity_step_bytes);
        let first = first_aligned
            + u128::from(lower)
                .saturating_sub(first_aligned)
                .div_ceil(aligned_candidate_spacing_bytes)
                * aligned_candidate_spacing_bytes;
        ensure!(
            first <= u128::from(upper),
            "the first aligned NAND capacity at or above {lower} bytes is {first} bytes, above --maximum-nand-capacity ({upper} bytes)"
        );
        let last_index = (u128::from(upper) - first) / aligned_candidate_spacing_bytes;
        // A spacing wider than the range means there is exactly one candidate.
        Ok((
            first as u64,
            aligned_candidate_spacing_bytes.min(u128::from(MAXIMUM_SUPPORTED_CAPACITY_BYTES))
                as u64,
            last_index as u64,
        ))
    }

    pub fn validate(self, logical_block_bytes: u64) -> Result<()> {
        self.aligned_candidates(logical_block_bytes).map(|_| ())
    }

    pub fn recommend(
        self,
        logical_block_bytes: u64,
        reference_wear_seconds: f64,
        limit_seconds: f64,
    ) -> Result<CapacitySelection> {
        ensure!(
            limit_seconds.is_finite() && limit_seconds > 0.0,
            "wear duration limit must be finite and positive, got {limit_seconds} s"
        );
        ensure!(
            reference_wear_seconds.is_finite() && reference_wear_seconds > 0.0,
            "predicted wear at the reference NAND capacity must take a finite positive time, got {reference_wear_seconds} s"
        );
        let (first, aligned_candidate_spacing_bytes, last_index) =
            self.aligned_candidates(logical_block_bytes)?;
        let candidate = |index| -> Result<CapacityEstimate> {
            let nand_capacity_bytes = first + index * aligned_candidate_spacing_bytes;
            let capacity_ratio =
                nand_capacity_bytes as f64 / self.pretest_nand_capacity_bytes as f64;
            let predicted_wear_seconds = reference_wear_seconds * capacity_ratio;
            ensure!(
                predicted_wear_seconds.is_finite() && predicted_wear_seconds >= 0.0,
                "invalid predicted wear duration {predicted_wear_seconds} s for {nand_capacity_bytes} bytes"
            );
            Ok(CapacityEstimate {
                nand_capacity_bytes,
                nand_capacity_step_offset: (i128::from(nand_capacity_bytes)
                    - i128::from(self.pretest_nand_capacity_bytes))
                    / i128::from(self.nand_capacity_step_bytes),
                predicted_wear_seconds,
                baseline_wear_seconds: limit_seconds,
                predicted_difference_seconds: predicted_wear_seconds - limit_seconds,
            })
        };

        let minimum_legal_capacity = candidate(0)?;
        if minimum_legal_capacity.predicted_wear_seconds > limit_seconds {
            return Ok(CapacitySelection::NoCapacityWithinLimit {
                minimum_legal_capacity,
            });
        }

        // More NAND increases the wear budget. Find the last capacity at or below the limit.
        let mut low = 0;
        let mut high = last_index;
        while low < high {
            let middle = low + (high - low).div_ceil(2);
            if candidate(middle)?.predicted_wear_seconds <= limit_seconds {
                low = middle;
            } else {
                high = middle - 1;
            }
        }
        Ok(CapacitySelection::Recommended(candidate(low)?))
    }
}

fn gcd_coefficient(left: i128, right: i128) -> (i128, i128) {
    let (mut previous, mut current) = (left, right);
    let (mut previous_coefficient, mut coefficient) = (1, 0);
    while current != 0 {
        let quotient = previous / current;
        (previous, current) = (current, previous % current);
        (previous_coefficient, coefficient) =
            (coefficient, previous_coefficient - quotient * coefficient);
    }
    (previous, previous_coefficient)
}
