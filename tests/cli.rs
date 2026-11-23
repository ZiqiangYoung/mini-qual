use std::fs;
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;
use uuid::Uuid;

#[test]
fn parses_run_and_pretest_options_before_device_discovery() -> anyhow::Result<()> {
    let binary = env!("CARGO_BIN_EXE_mini-qual");
    let pretest_options = [
        "--pretest-nand-capacity",
        "4TiB",
        "--nand-capacity-step",
        "128GiB",
        "--minimum-nand-capacity",
        "3TiB",
        "--maximum-nand-capacity",
        "8TiB",
    ];
    let run_options = [
        "--nand-capacity",
        "4TiB",
        "--run-id",
        "11111111-1111-4111-8111-111111111111",
        "--retention-baseline-temperature-c",
        "40",
        "--retention-baseline-duration-hours",
        "2160",
        "--retention-test-temperature-c",
        "80",
    ];
    for (subcommand, options) in [
        ("pretest", pretest_options.as_slice()),
        ("run", run_options.as_slice()),
    ] {
        // An invalid URL stops after argument parsing and before any device access.
        let output = Command::new(binary)
            .arg(subcommand)
            .args([
                "--sn",
                "TEST_SN",
                "--pe",
                "3.25",
                "--temperature-sample-interval-seconds",
                "60",
                "--no-tui",
                "--temperature-url",
                "invalid",
            ])
            .args(options)
            .output()?;
        let error = String::from_utf8(output.stderr)?;
        assert_eq!(output.status.code(), Some(1), "{subcommand}: {error}");
        assert!(error.contains("--temperature-url"), "{error}");
    }
    Ok(())
}

struct RunLogFixture(PathBuf);

impl Drop for RunLogFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn invalid_pe_explains_the_failed_unit_conversion() -> anyhow::Result<()> {
    let output = Command::new(env!("CARGO_BIN_EXE_mini-qual"))
        .args(["pretest", "--pe", "3.2505"])
        .output()?;
    let error = String::from_utf8(output.stderr)?;
    assert_eq!(output.status.code(), Some(2), "{error}");
    assert!(
        error.contains("fractional after unit conversion"),
        "{error}"
    );
    Ok(())
}

#[test]
fn retention_uses_recorded_temperatures_and_reports_unusable_logs() -> anyhow::Result<()> {
    let path = std::env::temp_dir().join(format!("mini-qual-cli-{}", Uuid::new_v4()));
    fs::create_dir(&path)?;
    let log = RunLogFixture(path);
    fs::write(
        log.0.join("wear-window.json"),
        r#"{"start_seconds":0.0,"end_seconds":60.0,"temperature_sample_interval_seconds":60.0}"#,
    )?;
    fs::write(
        log.0.join("temperature.jsonl"),
        concat!(
            r#"{"query_time":"2026-11-21T00:00:00Z","completed_time":"2026-11-21T00:00:00Z","elapsed_seconds":0.0,"http_status":200,"raw_json_body":"76.5","measured_case_temperature_c":76.5,"error":null}"#,
            "\n",
            r#"{"query_time":"2026-11-21T00:01:00Z","completed_time":"2026-11-21T00:01:00Z","elapsed_seconds":60.0,"http_status":200,"raw_json_body":"76.5","measured_case_temperature_c":76.5,"error":null}"#,
            "\n",
        ),
    )?;
    let mut command = Command::new(env!("CARGO_BIN_EXE_mini-qual"));
    command.args(["retention", "--log-dir"]).arg(&log.0).args([
        "--retention-baseline-temperature-c",
        "40",
        "--retention-baseline-duration-hours",
        "101",
        "--retention-test-temperature-c",
        "40",
    ]);
    let output = command.output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(report["recommended_hours"], 50.5);
    assert_eq!(report["successful_temperature_samples"], 2);
    assert_eq!(report["endurance_reference_temperature_c"], 76.5);
    assert_eq!(report["endurance_reference_duration_hours"], 1200.0);
    assert_eq!(report["endurance_activation_energy_ev"], 1.1);
    assert_eq!(report["retention_activation_energy_ev"], 0.8);
    assert_eq!(
        report["parameters"]["retention_baseline_temperature_c"],
        40.0
    );
    assert_eq!(
        report["parameters"]["retention_baseline_duration_hours"],
        101.0
    );
    assert_eq!(report["parameters"]["retention_test_temperature_c"], 40.0);

    let temperature_path = log.0.join("temperature.jsonl");
    let failed_sample = r#"{"query_time":"2026-11-21T00:00:00Z","completed_time":"2026-11-21T00:00:00Z","elapsed_seconds":0.0,"error":"case temperature GET returned HTTP 503 Service Unavailable"}"#;
    fs::write(&temperature_path, failed_sample)?;
    let output = command.output()?;
    let error = String::from_utf8(output.stderr)?;
    assert!(!output.status.success());
    assert!(
        error.contains(&temperature_path.display().to_string()),
        "{error}"
    );
    assert!(error.contains("HTTP 503 Service Unavailable"), "{error}");

    fs::write(
        &temperature_path,
        format!("{failed_sample}\ninvalid JSON\n"),
    )?;
    let output = command.output()?;
    let error = String::from_utf8(output.stderr)?;
    assert!(!output.status.success());
    assert!(error.contains("line 2"), "{error}");
    Ok(())
}
