use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::device::{Device, ReadStatus};

const IO_BLOCK_BYTES: u64 = 128 * 1024;
const MAX_TRIM_BYTES: u64 = 64 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);

#[derive(Debug, Default)]
pub struct Stage {
    pub duration: Duration,
    pub attempted_bytes: u64,
    pub successful_bytes: u64,
    pub errors: u64,
}

impl Stage {
    fn extend(&mut self, next: Self) {
        self.duration += next.duration;
        self.attempted_bytes += next.attempted_bytes;
        self.successful_bytes += next.successful_bytes;
        self.errors += next.errors;
    }
}

pub struct Fio<'a> {
    program: PathBuf,
    output: PathBuf,
    device: &'a Device,
    stop: &'a AtomicBool,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Operation {
    Trim,
    Write,
    Read,
}

impl Operation {
    fn name(self) -> &'static str {
        match self {
            Self::Trim => "trim",
            Self::Write => "write",
            Self::Read => "read",
        }
    }
}

struct Region {
    offset: u64,
    bytes: u64,
    block_bytes: u64,
}

#[derive(Clone, Copy)]
struct Job<'a> {
    phase: &'a str,
    round: u128,
    name: &'a str,
}

impl Job<'_> {
    fn prefix(self) -> String {
        format!("{}-round-{:06}-{}", self.phase, self.round, self.name)
    }

    fn file(self, directory: &Path, extension: &str) -> PathBuf {
        directory.join(format!("{}.{extension}", self.prefix()))
    }
}

#[derive(Default)]
struct FailedRequests {
    count: u64,
    bytes: u64,
}

#[derive(Deserialize)]
struct FailedRange {
    slba: u64,
    nlb: u64,
}

#[derive(Serialize)]
struct IoErrorRecord<'a> {
    timestamp_unix_ms: i128,
    phase: &'a str,
    round: u128,
    stage: &'a str,
    slba: u64,
    nlb: u64,
    status: Value,
}

impl<'a> Fio<'a> {
    pub fn new(
        program: PathBuf,
        output: PathBuf,
        device: &'a Device,
        stop: &'a AtomicBool,
    ) -> Result<Self> {
        let program = if program.components().count() == 1 {
            program
        } else {
            program
                .canonicalize()
                .with_context(|| format!("locating fio at {}", program.display()))?
        };
        let fio = Self {
            program,
            output,
            device,
            stop,
        };
        let version = fio
            .command()
            .arg("--version")
            .output()
            .context("starting fio --version")?;
        ensure!(
            version.status.success() && version.stdout.starts_with(b"fio-"),
            "{} is not the Flexible I/O Tester",
            fio.program.display()
        );
        let mut version_log = create_log(&fio.output.join("fio-version.txt"))?;
        version_log
            .write_all(&version.stdout)
            .context("saving fio version")?;
        version_log.sync_all().context("syncing fio version")?;
        let engine = fio
            .command()
            .arg("--enghelp=libaio")
            .output()
            .context("checking fio libaio support")?;
        ensure!(
            engine.status.success(),
            "{} does not provide the libaio I/O engine: {}",
            fio.program.display(),
            String::from_utf8_lossy(&engine.stderr).trim()
        );
        ensure!(
            device.discard_max_bytes >= device.logical_block_bytes,
            "the namespace does not advertise usable discard support"
        );
        Ok(fio)
    }

    pub fn trim(&self, phase: &str, round: u128) -> Result<Stage> {
        let block_bytes = self.device.discard_max_bytes.min(MAX_TRIM_BYTES)
            / self.device.logical_block_bytes
            * self.device.logical_block_bytes;
        self.run_sequential(
            phase,
            round,
            Operation::Trim,
            self.device.size_bytes,
            block_bytes,
        )
    }

    pub fn write(&self, phase: &str, round: u128, bytes: u64) -> Result<Stage> {
        self.run_sequential(phase, round, Operation::Write, bytes, self.io_block_bytes())
    }

    pub fn read(&self, phase: &str, round: u128, bytes: u64) -> Result<Stage> {
        self.run_sequential(phase, round, Operation::Read, bytes, self.io_block_bytes())
    }

    pub fn locate(&self, phase: &str, round: u128) -> Result<Stage> {
        let start = Instant::now();
        let job = Job {
            phase,
            round,
            name: "locate",
        };
        let mut errors = BufWriter::new(create_log(&job.file(&self.output, "errors.jsonl"))?);
        let outcome = self.locate_ranges(job, &mut errors);
        let saved = errors
            .flush()
            .and_then(|()| errors.get_ref().sync_all())
            .context("saving granular read errors");
        let mut result = outcome?;
        saved?;
        self.device.check_ready()?;
        result.duration = start.elapsed();
        Ok(result)
    }

    fn locate_ranges(&self, job: Job<'_>, errors: &mut impl Write) -> Result<Stage> {
        let mut result = Stage::default();
        let mut reader = None;
        let mut last_check = Instant::now();
        for name in ["read", "read-tail"] {
            let read_job = Job { name, ..job };
            let path = read_job.file(&self.output, "errors.jsonl");
            let input = match File::open(&path) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("opening {}", path.display()));
                }
            };
            for line in BufReader::new(input).lines() {
                let range: FailedRange = serde_json::from_str(&line?).with_context(|| {
                    format!("reading the failed ranges from {}", path.display())
                })?;
                if reader.is_none() {
                    reader = Some(self.device.open_reader()?);
                }
                let reader = reader.as_mut().context("opening diagnostics reader")?;
                for lba in range.slba
                    ..range
                        .slba
                        .checked_add(range.nlb)
                        .context("invalid read range")?
                {
                    self.ensure_running()?;
                    if last_check.elapsed() >= DEVICE_CHECK_INTERVAL {
                        self.device.check_present()?;
                        last_check = Instant::now();
                    }
                    let status = reader
                        .read_block(lba)
                        .with_context(|| format!("reading SLBA {lba}"))?;
                    result.attempted_bytes += self.device.logical_block_bytes;
                    if matches!(status, ReadStatus::Success) {
                        result.successful_bytes += self.device.logical_block_bytes;
                    } else {
                        result.errors += 1;
                        write_error(errors, job, lba, 1, json!(status))?;
                        if matches!(
                            status,
                            ReadStatus::Os {
                                errno: Some(libc::ENODEV | libc::ENXIO),
                                ..
                            }
                        ) {
                            self.device.check_ready()?;
                        }
                    }
                }
            }
        }
        Ok(result)
    }

    fn io_block_bytes(&self) -> u64 {
        let logical_block_bytes = self.device.logical_block_bytes;
        IO_BLOCK_BYTES.max(logical_block_bytes) / logical_block_bytes * logical_block_bytes
    }

    fn run_sequential(
        &self,
        phase: &str,
        round: u128,
        operation: Operation,
        bytes: u64,
        block_bytes: u64,
    ) -> Result<Stage> {
        ensure!(
            bytes > 0
                && bytes <= self.device.size_bytes
                && bytes % self.device.logical_block_bytes == 0,
            "{} range is outside the namespace or not LBA-aligned",
            operation.name()
        );
        let start = Instant::now();
        let remainder = bytes % block_bytes;
        let main_bytes = bytes - remainder;
        let mut result = Stage::default();
        if main_bytes > 0 {
            let job = Job {
                phase,
                round,
                name: operation.name(),
            };
            let region = Region {
                offset: 0,
                bytes: main_bytes,
                block_bytes,
            };
            result.extend(self.run_range(job, operation, &region)?);
        }
        // Separate the tail because fio can round a fixed-block job's size down.
        if remainder > 0 {
            let name = format!("{}-tail", operation.name());
            let job = Job {
                phase,
                round,
                name: &name,
            };
            let region = Region {
                offset: main_bytes,
                bytes: remainder,
                block_bytes: remainder,
            };
            result.extend(self.run_range(job, operation, &region)?);
        }
        result.duration = start.elapsed();
        Ok(result)
    }

    fn run_range(&self, job: Job<'_>, operation: Operation, region: &Region) -> Result<Stage> {
        self.ensure_running()?;
        self.device.check_ready()?;
        let prefix = job.prefix();
        let stdout_path = job.file(&self.output, "fio.stdout");
        let stderr_path = job.file(&self.output, "fio.stderr");
        let error_path = job.file(&self.output, "errors.jsonl");
        let stderr_log = create_log(&stderr_path)?;
        let error_log = create_log(&error_path)?;
        let mut command = self.command();
        command
            .current_dir(&self.output)
            .process_group(0)
            .arg(format!("--name={prefix}"))
            .arg(filename_option(&self.device.path))
            .args([
                "--output-format=json",
                "--eta=never",
                "--ioengine=libaio",
                "--thread=1",
                "--numjobs=1",
                "--direct=1",
                "--allow_file_create=0",
                "--allow_mounted_write=0",
                "--fallocate=none",
                "--verify=0",
                "--error_dump=1",
                "--loops=1",
                "--log_avg_msec=1000",
                "--log_unix_epoch=1",
            ])
            .arg(format!("--write_bw_log={prefix}"))
            .arg(format!("--rw={}", operation.name()))
            .arg(format!("--offset={}", region.offset))
            .arg(format!("--size={}", region.bytes))
            .arg(format!("--bs={}", region.block_bytes))
            .arg(if operation == Operation::Trim {
                "--iodepth=1"
            } else {
                "--iodepth=32"
            })
            .stdin(Stdio::null())
            .stdout(create_log(&stdout_path)?)
            .stderr(Stdio::piped());

        if operation == Operation::Read {
            command.arg("--readonly").arg(read_error_option());
        }

        self.ensure_running()?;
        let start = Instant::now();
        let mut child = command
            .spawn()
            .with_context(|| format!("starting fio {prefix}"))?;
        let stderr = child.stderr.take().expect("fio stderr is piped");
        let (status, failed) = thread::scope(|scope| {
            let collector = scope.spawn(|| {
                collect_errors(
                    stderr,
                    stderr_log,
                    error_log,
                    job,
                    operation,
                    region,
                    self.device.logical_block_bytes,
                )
            });
            let status = self.wait_for_fio(&mut child);
            if status.is_err() {
                terminate(&mut child);
            }
            let failed = collector
                .join()
                .map_err(|_| anyhow!("fio stderr collector panicked"))?;
            Ok::<_, anyhow::Error>((status, failed))
        })?;
        let duration = start.elapsed();
        let saved = sync_fio_logs(
            [&stdout_path, &stderr_path, &error_path],
            &self.output.join(format!("{prefix}_bw.1.log")),
        );
        let status = status.with_context(|| format!("{prefix}; see {}", stderr_path.display()))?;
        let failed = failed.with_context(|| format!("recording errors for {prefix}"))?;
        saved?;

        let report = read_report(&stdout_path);
        ensure!(
            status.success(),
            "fio {prefix} failed with {status}; job errno {:?}; see {}",
            report
                .as_ref()
                .ok()
                .and_then(|report| report["error"].as_u64()),
            stderr_path.display()
        );
        let report = report?;
        let job_errno = report["error"]
            .as_u64()
            .context("fio JSON missing job errno")?;
        let successful_bytes = report[operation.name()]["io_bytes"]
            .as_u64()
            .context("fio JSON missing operation byte count")?;
        if operation == Operation::Read {
            let total_errors = report["total_err"]
                .as_u64()
                .context("fio JSON missing read error count")?;
            ensure!(
                failed.count == total_errors,
                "fio {prefix} reported {total_errors} errors but recorded {} address ranges (job errno {job_errno}); see {}",
                failed.count,
                stderr_path.display()
            );
            // With continue-on-error, fio uses the first read errno as the job's error field.
            ensure!(
                job_errno == 0 || failed.count > 0,
                "fio {prefix} reported unlocalized job errno {job_errno}"
            );
        } else {
            ensure!(
                job_errno == 0 && failed.count == 0,
                "fio {prefix} failed with errno {job_errno}"
            );
        }
        ensure!(
            successful_bytes.checked_add(failed.bytes) == Some(region.bytes),
            "fio {prefix} covered {successful_bytes} successful bytes and {} failed bytes, expected {}; see {}",
            failed.bytes,
            region.bytes,
            stdout_path.display()
        );
        self.device.check_ready()?;
        Ok(Stage {
            duration,
            attempted_bytes: region.bytes,
            successful_bytes,
            errors: failed.count,
        })
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.env("LC_ALL", "C");
        command
    }

    fn ensure_running(&self) -> Result<()> {
        crate::check_running(self.stop)
    }

    fn wait_for_fio(&self, child: &mut Child) -> Result<ExitStatus> {
        let mut last_check = Instant::now();
        loop {
            if let Some(status) = child.try_wait().context("waiting for fio")? {
                return Ok(status);
            }
            self.ensure_running()?;
            if last_check.elapsed() >= DEVICE_CHECK_INTERVAL {
                self.device.check_present()?;
                last_check = Instant::now();
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn filename_option(path: &Path) -> OsString {
    let mut argument = OsString::from("--filename=");
    argument.push(path.as_os_str());
    argument
}

fn read_error_option() -> String {
    // Accept Linux's ordinary errno range for completed read requests. fio
    // classifies EILSEQ as a verify error even for an ordinary read, so it
    // needs the third list. The second list (write failures) stays empty.
    let values = (1..=133)
        .filter(|&errno| errno != libc::EILSEQ)
        .map(|errno| errno.to_string())
        .collect::<Vec<_>>()
        .join(":");
    format!("--ignore_error={values},,{}", libc::EILSEQ)
}

fn create_log(path: &Path) -> Result<File> {
    File::create_new(path).with_context(|| format!("creating {}", path.display()))
}

fn sync_fio_logs(paths: [&Path; 3], bandwidth: &Path) -> Result<()> {
    for path in paths {
        File::open(path)
            .and_then(|file| file.sync_all())
            .with_context(|| format!("saving {}", path.display()))?;
    }
    match File::open(bandwidth) {
        Ok(file) => file.sync_all(),
        // A job can stop before fio creates its bandwidth log.
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
    .with_context(|| format!("saving {}", bandwidth.display()))
}

fn read_report(path: &Path) -> Result<Value> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut line = Vec::new();
    // fio can print a textual job failure line before its JSON report.
    while reader.read_until(b'\n', &mut line)? > 0 {
        if line.first() == Some(&b'{') {
            let mut report: Value = serde_json::from_reader(line.as_slice().chain(reader))
                .with_context(|| format!("parsing fio report {}", path.display()))?;
            let jobs = report["jobs"]
                .as_array_mut()
                .context("fio JSON missing jobs")?;
            ensure!(
                jobs.len() == 1,
                "expected one fio job in {}",
                path.display()
            );
            return Ok(jobs.remove(0));
        }
        line.clear();
    }
    bail!("no JSON job report in {}", path.display())
}

fn collect_errors(
    stderr: impl Read,
    raw: File,
    errors: File,
    job: Job<'_>,
    operation: Operation,
    region: &Region,
    logical_block_bytes: u64,
) -> Result<FailedRequests> {
    let mut stderr = BufReader::new(stderr);
    let mut raw = BufWriter::new(raw);
    let mut errors = BufWriter::new(errors);
    let mut line = Vec::new();
    let mut failed = FailedRequests::default();
    let mut invalid_range = None;
    while stderr.read_until(b'\n', &mut line)? > 0 {
        raw.write_all(&line)?;
        let line_text = String::from_utf8_lossy(&line);
        if let Some((status, offset, bytes)) = parse_io_error(&line_text, operation) {
            if bytes == 0
                || offset % logical_block_bytes != 0
                || bytes % logical_block_bytes != 0
                || offset < region.offset
                || offset
                    .checked_add(bytes)
                    .is_none_or(|end| end > region.offset + region.bytes)
            {
                invalid_range.get_or_insert((offset, bytes));
            } else {
                write_error(
                    &mut errors,
                    job,
                    offset / logical_block_bytes,
                    bytes / logical_block_bytes,
                    json!({ "source": "fio_libaio", "message": status }),
                )?;
                failed.count = failed
                    .count
                    .checked_add(1)
                    .context("too many fio error records")?;
                failed.bytes = failed
                    .bytes
                    .checked_add(bytes)
                    .context("fio failed byte count overflow")?;
            }
        }
        line.clear();
    }
    raw.flush()?;
    errors.flush()?;
    if let Some((offset, bytes)) = invalid_range {
        bail!("fio reported an invalid failed range at byte offset {offset} of length {bytes}");
    }
    Ok(failed)
}

fn parse_io_error(line: &str, operation: Operation) -> Option<(&str, u64, u64)> {
    let error = line.strip_prefix("fio: io_u error on file ")?;
    let marker = format!(": {} offset=", operation.name());
    let (status, extent) = error.trim_end().rsplit_once(&marker)?;
    let (_, status) = status.rsplit_once(": ")?;
    let (offset, bytes) = extent.split_once(", buflen=")?;
    Some((status, offset.parse().ok()?, bytes.parse().ok()?))
}

fn write_error(
    output: &mut impl Write,
    job: Job<'_>,
    slba: u64,
    nlb: u64,
    status: Value,
) -> Result<()> {
    let record = IoErrorRecord {
        timestamp_unix_ms: OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000,
        phase: job.phase,
        round: job.round,
        stage: job.name,
        slba,
        nlb,
        status,
    };
    serde_json::to_writer(&mut *output, &record)?;
    output
        .write_all(b"\n")
        .context("writing I/O error record")?;
    Ok(())
}

fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }
    let process_group = -(child.id() as libc::pid_t);
    // The child was started in its own process group. A graceful signal lets
    // fio stop submitting I/O and export bandwidth data collected so far.
    unsafe { libc::kill(process_group, libc::SIGTERM) };
    let start = Instant::now();
    while start.elapsed() < INTERRUPT_GRACE {
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        thread::sleep(POLL_INTERVAL);
    }
    // The PID is still reserved until the child is reaped.
    unsafe { libc::kill(process_group, libc::SIGKILL) };
    let _ = child.wait();
}
