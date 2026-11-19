use mini_qual::plan;
use mini_qual::plan::{Budget, CapacityRange, RoundTiming, TimingModel};
use mini_qual::thermal;
use mini_qual::thermal::{RetentionParameters, Sample, WearWindow};

#[test]
fn exact_inputs_and_block_budget_include_retention() {
    assert_eq!(plan::parse_pe(" 1.234 K ").unwrap(), 1234);
    assert_eq!(plan::parse_pe("0.001").unwrap(), 1);
    assert!(plan::parse_pe("1.2345").is_err());
    assert_eq!(plan::parse_capacity("1.5 KiB").unwrap(), 1536);
    assert_eq!(plan::parse_capacity("2 PiB").unwrap(), 2 << 50);
    assert_eq!(plan::parse_capacity("512 Byte").unwrap(), 512);
    assert!(plan::parse_capacity("1.1 Byte").is_err());
    assert!(plan::parse_capacity("2.001 PiB").is_err());

    let budget = Budget::new(3, 6 * 4096, 4 * 4096, 4096).unwrap();
    assert_eq!(budget.wear_blocks, 14);
    assert_eq!(budget.full_rounds, 3);
    assert_eq!(budget.tail_blocks, 2);
    assert_eq!(
        budget.wear_blocks + u128::from(budget.host_blocks),
        budget.total_blocks
    );
    assert!(Budget::new(3, 6 * 4096 + 1, 4 * 4096, 4096).is_err());

    let largest = Budget::new(u64::MAX, 2 << 50, 512, 512).unwrap();
    assert!(largest.full_rounds > u128::from(u64::MAX));
    assert!(serde_json::to_value(largest).is_ok());
}

#[test]
fn recommendation_matches_enumeration_of_the_relative_capacity_grid() {
    let timing = RoundTiming {
        trim_seconds: 30.0,
        write_seconds: 16.0,
        flush_seconds: 2.0,
        read_seconds: 12.0,
        locate_seconds: 0.5,
        phase_gap_seconds: 0.5,
        round_gap_seconds: Some(1.0),
    };
    let model = TimingModel::from_rounds(&[
        RoundTiming {
            round_gap_seconds: None,
            ..timing
        },
        timing,
    ])
    .unwrap();
    let host = 4096;
    let block = 512;
    for (reference, step) in [(5120, 1024), (5121, 3), (5121, 8), (5120, 768)] {
        let range = CapacityRange {
            reference_bytes: reference,
            step_bytes: step,
            min_bytes: 3100,
            max_bytes: 14_000,
        };
        for target in [1.0_f64, 57.0, 73.0, 103.0, 130.0, 146.7, 1000.0] {
            let expected = (range.min_bytes..=range.max_bytes)
                .filter(|p| *p > host && p % block == 0)
                .filter(|p| p.abs_diff(reference) % step == 0)
                .map(|p| {
                    let budget = Budget::new(1, p, host, block).unwrap();
                    (p, (model.predict_wear_seconds(budget) - target).abs())
                })
                .min_by(|a, b| a.1.total_cmp(&b.1));
            let actual = range.recommend(1, host, block, model, target);
            assert_eq!(
                actual.ok().map(|actual| actual.physical_bytes),
                expected.map(|result| result.0)
            );
        }
    }
}

#[test]
fn thermal_correction_matches_reference_and_preserves_the_half_factor_floor() {
    let parameters = RetentionParameters {
        baseline_temp_c: 40.0,
        baseline_hours: 101.0,
        retention_temp_c: 40.0,
    };
    let window = WearWindow {
        start_seconds: 1.0,
        end_seconds: 1.0 + thermal::WEAR_TARGET_SECONDS,
        sample_interval_seconds: thermal::WEAR_TARGET_SECONDS,
    };
    let samples = [
        Sample {
            elapsed_seconds: window.start_seconds,
            temperature_c: thermal::WEAR_TARGET_C,
        },
        Sample {
            elapsed_seconds: window.end_seconds,
            temperature_c: thermal::WEAR_TARGET_C,
        },
    ];
    let estimate = parameters.estimate(window, &samples).unwrap();
    assert_eq!(estimate.recommended_hours, parameters.baseline_hours);
    assert_eq!(estimate.wear_equivalent_hours_at_reference, 1200.0);
    assert!((estimate.wear_effective_c - thermal::WEAR_TARGET_C).abs() < 1e-10);

    let short = WearWindow {
        end_seconds: 3601.0,
        ..window
    };
    let short_estimate = parameters.estimate(short, &samples[..1]).unwrap();
    assert_eq!(short_estimate.wear_adjustment, 0.5);
    assert_eq!(short_estimate.recommended_hours, 50.5);
    let hotter = RetentionParameters {
        retention_temp_c: 80.0,
        ..parameters
    };
    assert!(
        hotter
            .estimate(short, &samples[..1])
            .unwrap()
            .recommended_hours
            < 50.5
    );
}

#[test]
fn thermal_timeline_integrates_arrhenius_rates_and_rejects_long_gaps() {
    let window = WearWindow {
        start_seconds: 10.0,
        end_seconds: 30.0,
        sample_interval_seconds: 15.0,
    };
    let samples = [
        Sample {
            elapsed_seconds: 0.0,
            temperature_c: 76.5,
        },
        Sample {
            elapsed_seconds: 20.0,
            temperature_c: 86.5,
        },
        Sample {
            elapsed_seconds: 40.0,
            temperature_c: 76.5,
        },
    ];
    let hot_rate = (1.1 / 8.617_1e-5 * (1.0 / 349.65 - 1.0 / 359.65_f64)).exp();
    let expected = 20.0 * (0.25 + 0.75 * hot_rate);
    let actual = thermal::equivalent_wear_seconds(window, &samples).unwrap();
    assert!((actual - expected).abs() < 1e-10);
    assert!(
        thermal::equivalent_wear_seconds(
            WearWindow {
                sample_interval_seconds: 5.0,
                ..window
            },
            &samples,
        )
        .is_err()
    );
}
