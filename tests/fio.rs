use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde_json::Value;
use uuid::Uuid;

use crate::device::Device;
use crate::fio::Fio;
use crate::telemetry::Telemetry;

// Tests the production fio runner without constructing a real NVMe namespace.
#[path = "../src/fio.rs"]
mod fio;

fn check_running(stop: &AtomicBool) -> Result<()> {
    ensure!(!stop.load(Ordering::Relaxed), "I/O interrupted");
    Ok(())
}

mod device {
    use std::path::PathBuf;

    use anyhow::Result;
    use serde::Serialize;

    pub struct Device {
        pub path: PathBuf,
        pub host_visible_capacity_bytes: u64,
        pub logical_block_bytes: u64,
        pub maximum_discard_request_bytes: u64,
    }

    impl Device {
        pub fn check_ready(&self) -> Result<()> {
            Ok(())
        }

        pub fn check_present(&self) -> Result<()> {
            Ok(())
        }

        pub fn open_reader(&self) -> Result<BlockReader> {
            Ok(BlockReader)
        }
    }

    #[derive(Serialize)]
    #[serde(tag = "kind", rename_all = "snake_case")]
    pub enum ReadStatus {
        Success,
        Os { errno: Option<i32>, message: String },
    }

    pub struct BlockReader;

    impl BlockReader {
        pub fn read_block(&mut self, lba: u64) -> Result<ReadStatus> {
            Ok(if lba == 37 {
                ReadStatus::Os {
                    errno: Some(libc::EREMOTEIO),
                    message: "unclassified read error".to_owned(),
                }
            } else {
                ReadStatus::Success
            })
        }
    }
}

mod telemetry {
    use std::sync::{Arc, Mutex};

    #[derive(Debug, Clone, Copy, Default)]
    pub struct IoProgress {
        pub attempted_bytes: u64,
        pub successful_bytes: u64,
        pub errors: u64,
        pub bytes_per_second: Option<f64>,
        pub attempted_bytes_per_second: Option<f64>,
    }

    #[derive(Default, Clone)]
    pub struct Telemetry(pub Option<Arc<Mutex<Vec<IoProgress>>>>);

    impl Telemetry {
        pub fn is_active(&self) -> bool {
            self.0.is_some()
        }

        pub fn io_progress(&self, progress: IoProgress) {
            if let Some(samples) = &self.0 {
                samples.lock().unwrap().push(progress);
            }
        }
    }
}

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fio_fixture() -> Result<(TestDirectory, PathBuf, Device)> {
    let directory =
        TestDirectory(std::env::temp_dir().join(format!("mini-qual-fio-{}", Uuid::new_v4())));
    fs::create_dir(&directory.0)?;
    let program = directory.0.join("fio-mock");
    fs::write(&program, include_str!("fixtures/fio-mock.sh"))?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;

    let device = Device {
        path: directory.0.join("unused-device"),
        host_visible_capacity_bytes: 266240,
        logical_block_bytes: 4096,
        maximum_discard_request_bytes: 262144,
    };
    Ok((directory, program, device))
}

#[test]
fn sequential_io_preserves_aligned_tails_and_recovered_read_errors() -> Result<()> {
    let (directory, program, device) = fio_fixture()?;
    let stop = AtomicBool::new(false);
    let fio = Fio::new(program, directory.0.clone(), &device, &stop)?;
    let trim = fio.trim("pretest", 1)?;
    assert_eq!(trim.successful_bytes, device.host_visible_capacity_bytes);
    let write = fio.write("pretest", 1, device.host_visible_capacity_bytes)?;
    assert_eq!(write.successful_bytes, device.host_visible_capacity_bytes);
    let read = fio.read("pretest", 1, device.host_visible_capacity_bytes)?;
    assert_eq!(read.attempted_bytes, device.host_visible_capacity_bytes);
    assert_eq!(read.successful_bytes, 4096);
    assert_eq!(read.errors, 2);
    let locate = fio.locate("pretest", 1)?;
    assert_eq!(locate.attempted_bytes, 262144);
    assert_eq!(locate.successful_bytes, 258048);
    assert_eq!(locate.errors, 1);

    let original = fs::read_to_string(directory.0.join("pretest-round-000001-read.errors.jsonl"))?;
    let original: Vec<Value> = original
        .lines()
        .map(serde_json::from_str)
        .collect::<serde_json::Result<_>>()?;
    assert_eq!(original.len(), 2);
    assert_eq!(original[0]["slba"], 0);
    assert_eq!(original[0]["nlb"], 32);
    assert_eq!(original[1]["slba"], 32);
    assert_eq!(original[1]["nlb"], 32);
    let localized =
        fs::read_to_string(directory.0.join("pretest-round-000001-locate.errors.jsonl"))?;
    let localized: Value = serde_json::from_str(&localized)?;
    assert_eq!(localized["slba"], 37);
    assert_eq!(localized["nlb"], 1);
    assert_eq!(localized["status"]["errno"], libc::EREMOTEIO);

    let invocations = fs::read_to_string(directory.0.join("invocations"))?;
    assert_eq!(
        invocations,
        concat!(
            "trim 0 262144 262144 1\n",
            "trim 262144 4096 4096 1\n",
            "write 0 262144 131072 32\n",
            "write 262144 4096 4096 32\n",
            "read 0 262144 131072 32\n",
            "read 262144 4096 4096 32\n",
        )
    );

    let error = fio
        .write("pretest", 2, device.host_visible_capacity_bytes)
        .expect_err("a failed fio write must exit the stage");
    let message = format!("{error:#}");
    assert!(message.contains("No space left on device"), "{message}");
    assert!(message.contains("unused-device"), "{message}");
    assert!(message.contains("byte offset 0"), "{message}");
    assert!(
        message.contains("pretest-round-000002-write.fio.stderr"),
        "{message}"
    );

    let error = fio
        .write("pretest", 3, device.host_visible_capacity_bytes)
        .expect_err("fio startup errors must exit the stage");
    assert!(
        error
            .to_string()
            .contains("cannot open namespace for writing: Permission denied"),
        "{error:#}"
    );
    Ok(())
}

#[test]
fn live_status_keeps_only_final_reports_and_merges_read_tail_progress() -> Result<()> {
    let (directory, program, device) = fio_fixture()?;
    let stop = AtomicBool::new(false);
    let samples = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let fio = Fio::new(program, directory.0.clone(), &device, &stop)?
        .with_telemetry(Telemetry(Some(samples.clone())));
    let trim = fio.trim("wear", 1)?;
    assert_eq!(trim.successful_bytes, device.host_visible_capacity_bytes);
    samples.lock().unwrap().clear();

    let read = fio.read("wear", 1, device.host_visible_capacity_bytes)?;
    assert_eq!(read.attempted_bytes, device.host_visible_capacity_bytes);
    assert_eq!(read.successful_bytes, 4096);
    assert_eq!(read.errors, 2);

    for stage in ["trim", "trim-tail", "read", "read-tail"] {
        let output = fs::read(
            directory
                .0
                .join(format!("wear-round-000001-{stage}.fio.stdout")),
        )?;
        let final_report: Value = serde_json::from_slice(&output)?;
        assert!(final_report.get("snapshot").is_none());
        assert_eq!(
            final_report["jobs"][0]["jobname"],
            format!("wear-round-000001-{stage}")
        );
    }
    assert!(samples.lock().unwrap().iter().any(|sample| {
        sample.attempted_bytes == device.host_visible_capacity_bytes
            && sample.successful_bytes == 4096
            && sample.errors == 2
            && sample.bytes_per_second.is_some_and(|rate| rate > 0.0)
            && sample
                .attempted_bytes_per_second
                .is_some_and(|rate| rate > 0.0)
    }));
    Ok(())
}

#[test]
fn failed_fio_capture_never_persists_a_previous_complete_snapshot() -> Result<()> {
    let (directory, program, device) = fio_fixture()?;
    let stop = AtomicBool::new(false);
    let fio = Fio::new(program, directory.0.clone(), &device, &stop)?
        .with_telemetry(Telemetry(Some(Default::default())));
    for (round, expected_error) in [
        (4, "fio stdout ended within the final JSON report"),
        (5, "signal: 9"),
    ] {
        let error = fio
            .write("wear", round, device.host_visible_capacity_bytes)
            .expect_err("a completed snapshot cannot replace the final report");
        let error = format!("{error:#}");
        assert!(error.contains(expected_error), "{error}");
        let raw = fs::read_to_string(
            directory
                .0
                .join(format!("wear-round-{round:06}-write.fio.stdout")),
        )?;
        assert!(raw.is_empty(), "unexpected persisted snapshot: {raw}");
    }
    Ok(())
}
