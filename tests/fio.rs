use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, ensure};
use serde_json::Value;
use uuid::Uuid;

use crate::device::Device;
use crate::fio::Fio;

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
        pub size_bytes: u64,
        pub logical_block_bytes: u64,
        pub discard_max_bytes: u64,
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

struct TestDirectory(PathBuf);

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn sequential_io_preserves_aligned_tails_and_recovered_read_errors() -> Result<()> {
    let directory =
        TestDirectory(std::env::temp_dir().join(format!("mini-qual-fio-{}", Uuid::new_v4())));
    fs::create_dir(&directory.0)?;
    let program = directory.0.join("fio-mock");
    fs::write(&program, include_str!("fixtures/fio-mock.sh"))?;
    fs::set_permissions(&program, fs::Permissions::from_mode(0o700))?;

    let device = Device {
        path: directory.0.join("unused-device"),
        size_bytes: 266240,
        logical_block_bytes: 4096,
        discard_max_bytes: 262144,
    };
    let stop = AtomicBool::new(false);
    let fio = Fio::new(program, directory.0.clone(), &device, &stop)?;
    let trim = fio.trim("pretest", 1)?;
    assert_eq!(trim.successful_bytes, device.size_bytes);
    let write = fio.write("pretest", 1, device.size_bytes)?;
    assert_eq!(write.successful_bytes, device.size_bytes);
    let read = fio.read("pretest", 1, device.size_bytes)?;
    assert_eq!(read.attempted_bytes, device.size_bytes);
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
    Ok(())
}
