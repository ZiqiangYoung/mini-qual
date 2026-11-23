use mini_qual::plan;
use mini_qual::plan::{Budget, CapacityRange, CapacitySelection, RoundTiming, TimingModel};
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
    assert_eq!(budget.full_rounds, 3);
    let serialized_budget = serde_json::to_value(budget).unwrap();
    assert_eq!(serialized_budget["nand_capacity_blocks"], 6);
    assert_eq!(serialized_budget["host_visible_capacity_blocks"], 4);
    assert_eq!(serialized_budget["total_host_write_budget_blocks"], 18);
    assert_eq!(serialized_budget["endurance_write_budget_blocks"], 14);
    assert_eq!(serialized_budget["final_partial_round_blocks"], 2);
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
    let host_visible_capacity_bytes = 4096;
    let logical_block_bytes = 512;
    for (pretest_nand_capacity_bytes, nand_capacity_step_bytes) in
        [(5120, 1024), (5121, 3), (5121, 8), (5120, 768)]
    {
        let range = CapacityRange {
            pretest_nand_capacity_bytes,
            nand_capacity_step_bytes,
            minimum_nand_capacity_bytes: 3100,
            maximum_nand_capacity_bytes: 14_000,
        };
        for limit in [1.0, 57.0, 61.0, 73.0, 103.0, 130.0, 146.7, 1000.0] {
            let candidates: Vec<_> = (range.minimum_nand_capacity_bytes
                ..=range.maximum_nand_capacity_bytes)
                .filter(|capacity| {
                    *capacity > host_visible_capacity_bytes && capacity % logical_block_bytes == 0
                })
                .filter(|capacity| {
                    capacity.abs_diff(pretest_nand_capacity_bytes) % nand_capacity_step_bytes == 0
                })
                .map(|capacity| {
                    let budget = Budget::new(
                        1,
                        capacity,
                        host_visible_capacity_bytes,
                        logical_block_bytes,
                    )
                    .unwrap();
                    (capacity, model.predict_wear_seconds(budget))
                })
                .collect();
            let expected = candidates.iter().rfind(|(_, duration)| *duration <= limit);
            let selection = range.recommend(
                1,
                host_visible_capacity_bytes,
                logical_block_bytes,
                model,
                limit,
            );
            match (selection, expected, candidates.first()) {
                (Ok(CapacitySelection::Recommended(estimate)), Some(&(capacity, duration)), _) => {
                    assert_eq!(estimate.nand_capacity_bytes, capacity);
                    assert_eq!(estimate.predicted_wear_seconds, duration);
                    assert_eq!(
                        i128::from(estimate.nand_capacity_bytes),
                        i128::from(pretest_nand_capacity_bytes)
                            + estimate.nand_capacity_step_offset
                                * i128::from(nand_capacity_step_bytes)
                    );
                }
                (
                    Ok(CapacitySelection::NoCapacityWithinLimit {
                        minimum_legal_capacity,
                    }),
                    None,
                    Some(&(capacity, duration)),
                ) => {
                    assert_eq!(minimum_legal_capacity.nand_capacity_bytes, capacity);
                    assert_eq!(minimum_legal_capacity.predicted_wear_seconds, duration);
                    assert_eq!(
                        minimum_legal_capacity.predicted_difference_seconds,
                        duration - limit
                    );
                }
                (Err(_), None, None) => (),
                other => panic!("unexpected result at limit {limit}: {other:?}"),
            }
        }
    }
}

#[test]
fn enforces_the_1200_hour_limit_for_a_3_84_tb_device() -> anyhow::Result<()> {
    let host_visible_capacity_bytes = plan::parse_capacity("3.84TB")?;
    let seconds_per_full_io = host_visible_capacity_bytes as f64 / 3_400_000_000.0;
    let round = RoundTiming {
        write_seconds: seconds_per_full_io,
        read_seconds: seconds_per_full_io,
        ..RoundTiming::default()
    };
    let model = TimingModel::from_rounds(&[round, round])?;
    let range = CapacityRange {
        pretest_nand_capacity_bytes: plan::parse_capacity("4.096TB")?,
        nand_capacity_step_bytes: plan::parse_capacity("128GB")?,
        minimum_nand_capacity_bytes: host_visible_capacity_bytes,
        maximum_nand_capacity_bytes: plan::parse_capacity("5TB")?,
    };
    let limit = thermal::ENDURANCE_REFERENCE_DURATION_SECONDS;
    let logical_block_bytes = 512;
    let pe = plan::parse_pe("1.8K")?;
    let CapacitySelection::Recommended(estimate) = range.recommend(
        pe,
        host_visible_capacity_bytes,
        logical_block_bytes,
        model,
        limit,
    )?
    else {
        panic!("the 3.968 TB capacity fits in 1200 hours");
    };
    assert_eq!(
        estimate.nand_capacity_bytes,
        plan::parse_capacity("3.968TB")?
    );
    assert!(estimate.predicted_wear_seconds < limit);
    // The next step is closer to 1200 hours in absolute terms, but exceeds the limit.
    let next_budget = Budget::new(
        pe,
        range.pretest_nand_capacity_bytes,
        host_visible_capacity_bytes,
        logical_block_bytes,
    )?;
    let next_seconds = model.predict_wear_seconds(next_budget);
    assert!(next_seconds > limit);
    assert!(next_seconds - limit < limit - estimate.predicted_wear_seconds);

    let CapacitySelection::NoCapacityWithinLimit {
        minimum_legal_capacity,
    } = range.recommend(
        plan::parse_pe("3.25K")?,
        host_visible_capacity_bytes,
        logical_block_bytes,
        model,
        limit,
    )?
    else {
        panic!("even the minimum legal capacity exceeds 1200 hours at 3250 cycles");
    };
    assert_eq!(
        minimum_legal_capacity.nand_capacity_bytes,
        estimate.nand_capacity_bytes
    );
    assert!(minimum_legal_capacity.predicted_wear_seconds > 2_000.0 * 3600.0);
    Ok(())
}

#[test]
fn thermal_correction_matches_reference_and_preserves_the_half_factor_floor() {
    let parameters = RetentionParameters {
        retention_baseline_temperature_c: 40.0,
        retention_baseline_duration_hours: 101.0,
        retention_test_temperature_c: 40.0,
    };
    let window = WearWindow {
        start_seconds: 1.0,
        end_seconds: 1.0 + thermal::ENDURANCE_REFERENCE_DURATION_SECONDS,
        temperature_sample_interval_seconds: thermal::ENDURANCE_REFERENCE_DURATION_SECONDS,
    };
    let samples = [
        Sample {
            elapsed_seconds: window.start_seconds,
            measured_case_temperature_c: thermal::ENDURANCE_REFERENCE_TEMPERATURE_C,
        },
        Sample {
            elapsed_seconds: window.end_seconds,
            measured_case_temperature_c: thermal::ENDURANCE_REFERENCE_TEMPERATURE_C,
        },
    ];
    let estimate = parameters.estimate(window, &samples).unwrap();
    assert_eq!(
        estimate.recommended_hours,
        parameters.retention_baseline_duration_hours
    );
    assert_eq!(estimate.wear_equivalent_hours_at_reference, 1200.0);
    assert!(
        (estimate.effective_endurance_temperature_c - thermal::ENDURANCE_REFERENCE_TEMPERATURE_C)
            .abs()
            < 1e-10
    );

    let short = WearWindow {
        end_seconds: 3601.0,
        ..window
    };
    let short_estimate = parameters.estimate(short, &samples[..1]).unwrap();
    assert_eq!(short_estimate.wear_adjustment, 0.5);
    assert_eq!(short_estimate.recommended_hours, 50.5);
    let hotter = RetentionParameters {
        retention_test_temperature_c: 80.0,
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
        temperature_sample_interval_seconds: 15.0,
    };
    let samples = [
        Sample {
            elapsed_seconds: 0.0,
            measured_case_temperature_c: 76.5,
        },
        Sample {
            elapsed_seconds: 20.0,
            measured_case_temperature_c: 86.5,
        },
        Sample {
            elapsed_seconds: 40.0,
            measured_case_temperature_c: 76.5,
        },
    ];
    let hot_rate = (1.1 / 8.617_1e-5 * (1.0 / 349.65 - 1.0 / 359.65_f64)).exp();
    let expected = 20.0 * (0.25 + 0.75 * hot_rate);
    let actual = thermal::equivalent_wear_seconds(window, &samples).unwrap();
    assert!((actual - expected).abs() < 1e-10);
    assert!(
        thermal::equivalent_wear_seconds(
            WearWindow {
                temperature_sample_interval_seconds: 5.0,
                ..window
            },
            &samples,
        )
        .is_err()
    );
}
