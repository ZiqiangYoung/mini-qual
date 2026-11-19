use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

pub const MAX_CAPACITY: u64 = 2 << 50;

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Budget {
    pub physical_blocks: u64,
    pub host_blocks: u64,
    pub total_blocks: u128,
    pub wear_blocks: u128,
    pub full_rounds: u128,
    pub tail_blocks: u64,
}

impl Budget {
    pub fn new(pe: u64, physical_bytes: u64, host_bytes: u64, block_bytes: u64) -> Result<Self> {
        ensure!(pe > 0, "PE must be positive");
        ensure!(
            physical_bytes > host_bytes && physical_bytes <= MAX_CAPACITY,
            "physical capacity P must be larger than host capacity U and at most 2 PiB"
        );
        ensure!(
            block_bytes > 0
                && host_bytes > 0
                && physical_bytes % block_bytes == 0
                && host_bytes % block_bytes == 0,
            "physical and host capacities must be aligned to {block_bytes} bytes"
        );
        let physical_blocks = physical_bytes / block_bytes;
        let host_blocks = host_bytes / block_bytes;
        let total_blocks = u128::from(pe) * u128::from(physical_blocks);
        let wear_blocks = total_blocks - u128::from(host_blocks);
        Ok(Self {
            physical_blocks,
            host_blocks,
            total_blocks,
            wear_blocks,
            full_rounds: wear_blocks / u128::from(host_blocks),
            tail_blocks: (wear_blocks % u128::from(host_blocks)) as u64,
        })
    }
}

pub fn parse_pe(value: &str) -> Result<u64> {
    let value = value.trim();
    let number = value.strip_suffix(['K', 'k']).unwrap_or(value).trim_end();
    decimal_units(number, 1000).context("PE is expressed in K and must expand to an integer")
}

pub fn parse_capacity(value: &str) -> Result<u64> {
    let value = value.trim();
    let unit_start = value
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(value.len());
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
        _ => bail!("unsupported capacity unit {unit:?}"),
    };
    let bytes = decimal_units(&value[..unit_start], multiplier)?;
    ensure!(bytes <= MAX_CAPACITY, "capacity exceeds 2 PiB");
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
        whole.parse::<u64>()?
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
        ensure!(numerator % divisor == 0, "value has a fractional base unit");
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

    pub fn predict_wear_seconds(&self, budget: Budget) -> f64 {
        let fixed =
            self.average.trim_seconds + self.average.flush_seconds + self.average.phase_gap_seconds;
        let scalable =
            self.average.write_seconds + self.average.read_seconds + self.average.locate_seconds;
        let mut seconds = (fixed + scalable) * budget.full_rounds as f64;
        if budget.tail_blocks > 0 {
            seconds += fixed + scalable * (budget.tail_blocks as f64 / budget.host_blocks as f64);
        }
        let rounds = budget.full_rounds + u128::from(budget.tail_blocks > 0);
        seconds + self.average.round_gap_seconds.unwrap_or(0.0) * rounds.saturating_sub(1) as f64
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct CapacityRange {
    pub reference_bytes: u64,
    pub step_bytes: u64,
    pub min_bytes: u64,
    pub max_bytes: u64,
}

#[derive(Debug, Serialize)]
pub struct Recommendation {
    pub physical_bytes: u64,
    pub capacity_steps: i128,
    pub budget: Budget,
    pub predicted_wear_seconds: f64,
    pub baseline_wear_seconds: f64,
    pub predicted_difference_seconds: f64,
}

impl CapacityRange {
    fn aligned_candidates(self, host_bytes: u64, block_bytes: u64) -> Result<(u64, u64, u64)> {
        ensure!(block_bytes > 0, "logical block size must be positive");
        ensure!(
            self.step_bytes > 0
                && self.reference_bytes > 0
                && self.reference_bytes <= MAX_CAPACITY
                && self.min_bytes <= self.max_bytes
                && self.max_bytes <= MAX_CAPACITY,
            "invalid physical capacity reference, step, or range"
        );

        let lower = self.min_bytes.max(host_bytes.saturating_add(1));
        let upper = self.max_bytes;
        let remainder = self.reference_bytes % self.step_bytes;
        let (gcd, inverse) = gcd_coefficient(self.step_bytes.into(), block_bytes.into());
        ensure!(
            i128::from(remainder) % gcd == 0,
            "no capacity on the requested step grid aligns to logical blocks"
        );
        // For P = remainder + n*S, solve S*n = -remainder (mod block_bytes).
        let modulus = i128::from(block_bytes) / gcd;
        let n = ((-i128::from(remainder) / gcd) * inverse).rem_euclid(modulus);
        let first_aligned = u128::from(remainder) + n as u128 * u128::from(self.step_bytes);
        let stride = modulus as u128 * u128::from(self.step_bytes);
        let first = first_aligned
            + u128::from(lower)
                .saturating_sub(first_aligned)
                .div_ceil(stride)
                * stride;
        ensure!(
            first <= u128::from(upper),
            "no legal P in the requested capacity range"
        );
        let last_index = (u128::from(upper) - first) / stride;
        // A stride wider than the range means there is exactly one candidate.
        Ok((
            first as u64,
            stride.min(u128::from(MAX_CAPACITY)) as u64,
            last_index as u64,
        ))
    }

    pub fn validate(self, host_bytes: u64, block_bytes: u64) -> Result<()> {
        self.aligned_candidates(host_bytes, block_bytes).map(|_| ())
    }

    pub fn recommend(
        self,
        pe: u64,
        host_bytes: u64,
        block_bytes: u64,
        model: TimingModel,
        target_seconds: f64,
    ) -> Result<Recommendation> {
        ensure!(
            target_seconds.is_finite() && target_seconds > 0.0,
            "invalid wear target"
        );
        let (first, stride, last_index) = self.aligned_candidates(host_bytes, block_bytes)?;
        let candidate = |index| -> Result<Recommendation> {
            let physical_bytes = first + index * stride;
            let budget = Budget::new(pe, physical_bytes, host_bytes, block_bytes)?;
            let predicted_wear_seconds = model.predict_wear_seconds(budget);
            Ok(Recommendation {
                physical_bytes,
                capacity_steps: (i128::from(physical_bytes) - i128::from(self.reference_bytes))
                    / i128::from(self.step_bytes),
                budget,
                predicted_wear_seconds,
                baseline_wear_seconds: target_seconds,
                predicted_difference_seconds: predicted_wear_seconds - target_seconds,
            })
        };

        let mut low = 0;
        let mut high = last_index;
        while low < high {
            let middle = low + (high - low) / 2;
            if candidate(middle)?.predicted_wear_seconds < target_seconds {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        let upper = candidate(low)?;
        if low > 0 {
            let lower = candidate(low - 1)?;
            if lower.predicted_difference_seconds.abs() <= upper.predicted_difference_seconds.abs()
            {
                return Ok(lower);
            }
        }
        Ok(upper)
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
