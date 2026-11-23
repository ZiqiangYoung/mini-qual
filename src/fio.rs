use std::ffi::OsString;
use std::fs::File;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use time::OffsetDateTime;

use crate::device::{Device, ReadStatus};
use crate::telemetry::{IoProgress, Telemetry};

const IO_BLOCK_BYTES: u64 = 128 * 1024;
const MAX_TRIM_BYTES: u64 = 64 * 1024 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
const DEVICE_CHECK_INTERVAL: Duration = Duration::from_secs(5);
const INTERRUPT_GRACE: Duration = Duration::from_secs(3);
const DIAGNOSTIC_PREFIX_BYTES: u64 = 4096;
const MAX_STATUS_REPORT_BYTES: usize = 2 * 1024 * 1024;
const LOCALIZATION_SAMPLE_INTERVAL: Duration = Duration::from_millis(500);

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

    fn progress(&self, bytes_per_second: Option<f64>) -> IoProgress {
        IoProgress {
            attempted_bytes: self.attempted_bytes,
            successful_bytes: self.successful_bytes,
            errors: self.errors,
            bytes_per_second,
        }
    }
}

pub struct Fio<'a> {
    program: PathBuf,
    output: PathBuf,
    device: &'a Device,
    stop: &'a AtomicBool,
    telemetry: Telemetry,
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

#[derive(Default)]
struct LiveFailures {
    count: AtomicU64,
    bytes: AtomicU64,
}

struct ErrorLogs {
    stderr: File,
    ranges: File,
}

struct StatusObserver<'a> {
    telemetry: &'a Telemetry,
    job_name: &'a str,
    operation: Operation,
    region: &'a Region,
    completed: &'a Stage,
    failures: &'a LiveFailures,
    previous_sample: Option<(Instant, u64)>,
}

impl StatusObserver<'_> {
    fn observe(&mut self, bytes: &[u8]) {
        if !self.telemetry.is_active() {
            return;
        }
        let Ok(report) = serde_json::from_slice::<Value>(bytes) else {
            return;
        };
        let job = &report["jobs"][0];
        if job["jobname"].as_str() != Some(self.job_name) {
            return;
        }
        let Some(successful_bytes) = job[self.operation.name()]["io_bytes"].as_u64() else {
            return;
        };
        let successful_bytes = successful_bytes.min(self.region.bytes);
        let now = Instant::now();
        let bytes_per_second = self
            .previous_sample
            .and_then(|(previous_at, previous_bytes)| {
                let elapsed = now.duration_since(previous_at).as_secs_f64();
                let delta = successful_bytes.checked_sub(previous_bytes)?;
                (elapsed > 0.0).then(|| delta as f64 / elapsed)
            });
        self.previous_sample = Some((now, successful_bytes));
        let failed_count = self.failures.count.load(Ordering::Acquire);
        let failed_bytes = self.failures.bytes.load(Ordering::Relaxed);
        self.telemetry.io_progress(IoProgress {
            attempted_bytes: self.completed.attempted_bytes
                + successful_bytes
                    .saturating_add(failed_bytes)
                    .min(self.region.bytes),
            successful_bytes: self.completed.successful_bytes + successful_bytes,
            errors: self.completed.errors + failed_count,
            bytes_per_second,
        });
    }
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
            telemetry: Telemetry::default(),
        };
        let version = fio
            .command()
            .arg("--version")
            .output()
            .with_context(|| format!("running {} --version", fio.program.display()))?;
        ensure!(
            version.status.success(),
            "{} --version returned {}. {}",
            fio.program.display(),
            version.status,
            diagnostic_line(&version.stderr).unwrap_or_else(|| "no message on stderr".into())
        );
        ensure!(
            version.stdout.starts_with(b"fio-"),
            "{} --version did not identify the Flexible I/O Tester. Expected output beginning with 'fio-', got {}",
            fio.program.display(),
            diagnostic_line(&version.stdout).unwrap_or_else(|| "empty output".into())
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
            .with_context(|| {
                format!(
                    "checking libaio support with {} --enghelp=libaio",
                    fio.program.display()
                )
            })?;
        ensure!(
            engine.status.success(),
            "{} --enghelp=libaio returned {}. {}",
            fio.program.display(),
            engine.status,
            diagnostic_line(&engine.stderr).unwrap_or_else(|| {
                "the selected fio binary could not start its libaio engine".into()
            })
        );
        ensure!(
            device.maximum_discard_request_bytes >= device.logical_block_bytes,
            "{} reports a maximum discard request of {} bytes, smaller than its {}-byte logical block. The trial requires TRIM",
            device.path.display(),
            device.maximum_discard_request_bytes,
            device.logical_block_bytes
        );
        Ok(fio)
    }

    pub fn with_telemetry(mut self, telemetry: Telemetry) -> Self {
        self.telemetry = telemetry;
        self
    }

    pub fn trim(&self, phase: &str, round: u128) -> Result<Stage> {
        let block_bytes = self
            .device
            .maximum_discard_request_bytes
            .min(MAX_TRIM_BYTES)
            / self.device.logical_block_bytes
            * self.device.logical_block_bytes;
        self.run_sequential(
            phase,
            round,
            Operation::Trim,
            self.device.host_visible_capacity_bytes,
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
        self.telemetry.io_progress(result.progress(None));
        Ok(result)
    }

    fn locate_ranges(&self, job: Job<'_>, errors: &mut impl Write) -> Result<Stage> {
        let mut result = Stage::default();
        let mut reader = None;
        let mut last_check = Instant::now();
        let mut last_progress = (last_check, 0);
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
                    if self.telemetry.is_active()
                        && last_progress.0.elapsed() >= LOCALIZATION_SAMPLE_INTERVAL
                    {
                        let now = Instant::now();
                        let bytes_per_second = (result.successful_bytes - last_progress.1) as f64
                            / now.duration_since(last_progress.0).as_secs_f64();
                        self.telemetry
                            .io_progress(result.progress(Some(bytes_per_second)));
                        last_progress = (now, result.successful_bytes);
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
                && bytes <= self.device.host_visible_capacity_bytes
                && bytes % self.device.logical_block_bytes == 0,
            "{} requested {bytes} bytes on {}. The length must be positive, at most {} bytes, and a multiple of {} bytes",
            operation.name(),
            self.device.path.display(),
            self.device.host_visible_capacity_bytes,
            self.device.logical_block_bytes
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
            result.extend(self.run_range(job, operation, &region, &result)?);
            self.telemetry.io_progress(result.progress(None));
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
            result.extend(self.run_range(job, operation, &region, &result)?);
            self.telemetry.io_progress(result.progress(None));
        }
        result.duration = start.elapsed();
        Ok(result)
    }

    fn run_range(
        &self,
        job: Job<'_>,
        operation: Operation,
        region: &Region,
        completed: &Stage,
    ) -> Result<Stage> {
        self.ensure_running()?;
        self.device.check_ready()?;
        let prefix = job.prefix();
        let stdout_path = job.file(&self.output, "fio.stdout");
        let stderr_path = job.file(&self.output, "fio.stderr");
        let error_path = job.file(&self.output, "errors.jsonl");
        let error_logs = ErrorLogs {
            stderr: create_log(&stderr_path)?,
            ranges: create_log(&error_path)?,
        };
        let stdout_log = create_log(&stdout_path)?;
        let live_failures = self.telemetry.is_active().then(LiveFailures::default);
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
            .stderr(Stdio::piped());

        let captured_stdout = if live_failures.is_some() {
            command.arg("--status-interval=1").stdout(Stdio::piped());
            Some(stdout_log)
        } else {
            command.stdout(stdout_log);
            None
        };
        if operation == Operation::Read {
            command.arg("--readonly").arg(read_error_option());
        }

        self.ensure_running()?;
        let start = Instant::now();
        let mut child = command.spawn().with_context(|| {
            format!(
                "starting {} for job {prefix} on {}",
                self.program.display(),
                self.device.path.display()
            )
        })?;
        let stderr = child.stderr.take().expect("fio stderr is piped");
        let stdout = child.stdout.take();
        let (status, failed, captured) = thread::scope(|scope| {
            let collector = scope.spawn(|| {
                collect_errors(
                    stderr,
                    error_logs,
                    job,
                    operation,
                    region,
                    self.device.logical_block_bytes,
                    live_failures.as_ref(),
                )
            });
            let report_collector = captured_stdout.zip(stdout).map(|(output, stdout)| {
                scope.spawn(|| {
                    let mut observer = StatusObserver {
                        telemetry: &self.telemetry,
                        job_name: &prefix,
                        operation,
                        region,
                        completed,
                        failures: live_failures
                            .as_ref()
                            .expect("live capture tracks failed ranges"),
                        previous_sample: None,
                    };
                    capture_final_report(stdout, output, |report| observer.observe(report))
                })
            });
            let status = self.wait_for_fio(&mut child);
            let process_exit = match status.as_ref() {
                Ok(exited) => Some(*exited),
                Err(_) => terminate(&mut child),
            };
            let failed = collector
                .join()
                .map_err(|_| anyhow!("fio stderr collector panicked"))?;
            let captured = report_collector
                .map(|collector| {
                    collector
                        .join()
                        .map(|result| result.map(Some))
                        .map_err(|_| anyhow!("fio stdout collector panicked"))
                })
                .transpose()?
                .unwrap_or(Ok(None))
                .and_then(|report| match report {
                    // Signal termination can leave a complete periodic snapshot at EOF.
                    Some(report) if process_exit.is_some_and(|status| status.code().is_some()) => {
                        report.save()
                    }
                    _ => Ok(()),
                });
            Ok::<_, anyhow::Error>((status, failed, captured))
        })?;
        let duration = start.elapsed();
        let saved = sync_fio_logs(
            [&stdout_path, &stderr_path, &error_path],
            &self.output.join(format!("{prefix}_bw.1.log")),
        );
        let status = status.with_context(|| {
            format!(
                "waiting for fio job {prefix} (stderr log {})",
                stderr_path.display()
            )
        })?;
        let report = read_report(&stdout_path);
        ensure!(
            status.success(),
            "fio job {prefix} on {} returned {status} at byte offset {} with a requested length of {} bytes. {} Logs: {} and {}",
            self.device.path.display(),
            region.offset,
            region.bytes,
            fio_failure_detail(
                &stderr_path,
                report
                    .as_ref()
                    .ok()
                    .and_then(|report| report["error"].as_u64())
            ),
            stderr_path.display(),
            stdout_path.display()
        );
        let failed = failed.with_context(|| {
            format!(
                "recording errors for fio job {prefix} (stderr log {})",
                stderr_path.display()
            )
        })?;
        captured
            .with_context(|| format!("capturing final fio report {}", stdout_path.display()))?;
        saved?;
        let report = report?;
        let job_errno = report["error"].as_u64().with_context(|| {
            format!(
                "fio report {} is missing the integer jobs[0].error field",
                stdout_path.display()
            )
        })?;
        let successful_bytes =
            report[operation.name()]["io_bytes"]
                .as_u64()
                .with_context(|| {
                    format!(
                        "fio report {} is missing the integer jobs[0].{}.io_bytes field",
                        stdout_path.display(),
                        operation.name()
                    )
                })?;
        if operation == Operation::Read {
            let total_errors = report["total_err"]
                .as_u64()
                .with_context(|| format!("fio report {} is missing the integer jobs[0].total_err field required to verify read coverage", stdout_path.display()))?;
            ensure!(
                failed.count == total_errors,
                "fio job {prefix} reported {total_errors} read errors but logged {} failed address ranges (job errno {job_errno}). The unreadable ranges cannot be verified from {}",
                failed.count,
                stderr_path.display()
            );
            // With continue-on-error, fio uses the first read errno as the job's error field.
            ensure!(
                job_errno == 0 || failed.count > 0,
                "fio job {prefix} failed without reporting a failed read range. {} Log: {}",
                fio_failure_detail(&stderr_path, Some(job_errno)),
                stderr_path.display()
            );
        } else {
            ensure!(
                job_errno == 0 && failed.count == 0,
                "fio job {prefix} reported {} failed requests. {} Log: {}",
                failed.count,
                fio_failure_detail(&stderr_path, Some(job_errno)),
                stderr_path.display()
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

fn diagnostic_line(bytes: &[u8]) -> Option<String> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(300).collect())
}

fn fio_failure_detail(stderr_path: &Path, job_errno: Option<u64>) -> String {
    let mut stderr = Vec::new();
    if let Ok(file) = File::open(stderr_path) {
        let _ = file.take(DIAGNOSTIC_PREFIX_BYTES).read_to_end(&mut stderr);
    }
    let stderr_message = diagnostic_line(&stderr);
    let job_message = job_errno.filter(|errno| *errno > 0).map(|errno| {
        i32::try_from(errno)
            .map(|errno| io::Error::from_raw_os_error(errno).to_string())
            .unwrap_or_else(|_| format!("fio reported job errno {errno}"))
    });
    match (stderr_message, job_message) {
        (Some(stderr), Some(job)) => format!("{stderr} (fio job error: {job})"),
        (Some(message), None) | (None, Some(message)) => message,
        _ => "fio provided neither a message on stderr nor a job errno.".into(),
    }
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
        if line
            .iter()
            .copied()
            .find(|byte| !byte.is_ascii_whitespace())
            == Some(b'{')
        {
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

#[derive(Default)]
struct StatusFrame {
    bytes: Vec<u8>,
    depth: usize,
    in_string: bool,
    escaped: bool,
    oversized: bool,
}

impl StatusFrame {
    fn push(&mut self, byte: u8) {
        if self.bytes.len() < MAX_STATUS_REPORT_BYTES {
            self.bytes.push(byte);
        } else {
            self.oversized = true;
        }
        if self.in_string {
            if self.escaped {
                self.escaped = false;
            } else if byte == b'\\' {
                self.escaped = true;
            } else if byte == b'"' {
                self.in_string = false;
            }
        } else {
            match byte {
                b'{' => self.depth += 1,
                b'}' => self.depth -= 1,
                b'"' => self.in_string = true,
                _ => (),
            }
        }
    }
}

struct FinalReport {
    output: BufWriter<File>,
    raw_json: Vec<u8>,
    starts_on_new_line: bool,
}

impl FinalReport {
    fn save(mut self) -> Result<()> {
        if !self.starts_on_new_line {
            self.output.write_all(b"\n")?;
        }
        self.output
            .write_all(&self.raw_json)
            .context("saving the final fio JSON report")?;
        self.output.write_all(b"\n")?;
        self.output
            .flush()
            .context("flushing the final fio JSON report")?;
        Ok(())
    }
}

fn capture_final_report(
    input: impl Read,
    output: File,
    mut observe: impl FnMut(&[u8]),
) -> Result<FinalReport> {
    let mut output = BufWriter::new(output);
    let mut frame = StatusFrame::default();
    let mut at_line_start = true;
    let mut after_report = false;
    let mut last_written = b'\n';
    for byte in BufReader::new(input).bytes() {
        let byte = byte.context("reading fio stdout")?;
        if frame.depth == 0 {
            if !at_line_start || byte != b'{' {
                // Whitespace between snapshots has no diagnostic value.
                if !after_report || !byte.is_ascii_whitespace() {
                    output.write_all(&[byte])?;
                    after_report = false;
                    last_written = byte;
                }
                at_line_start = byte == b'\n' || (at_line_start && byte.is_ascii_whitespace());
                continue;
            }
            // A new report invalidates the previous candidate even if the new report is truncated.
            frame.bytes.clear();
            frame.oversized = false;
        }
        frame.push(byte);
        if frame.depth == 0 {
            if !frame.oversized {
                observe(&frame.bytes);
            }
            at_line_start = true;
            after_report = true;
        }
    }
    output.flush().context("saving fio stdout diagnostics")?;
    ensure!(
        frame.depth == 0,
        "fio stdout ended within the final JSON report"
    );
    ensure!(
        !frame.oversized,
        "final fio JSON report exceeds {MAX_STATUS_REPORT_BYTES} bytes"
    );
    ensure!(
        !frame.bytes.is_empty(),
        "fio stdout ended without a final JSON report"
    );
    // fio stops the periodic status thread before printing the final report.
    Ok(FinalReport {
        output,
        raw_json: frame.bytes,
        starts_on_new_line: last_written == b'\n',
    })
}

fn collect_errors(
    stderr: impl Read,
    logs: ErrorLogs,
    job: Job<'_>,
    operation: Operation,
    region: &Region,
    logical_block_bytes: u64,
    live_failures: Option<&LiveFailures>,
) -> Result<FailedRequests> {
    let mut stderr = BufReader::new(stderr);
    let mut raw = BufWriter::new(logs.stderr);
    let mut errors = BufWriter::new(logs.ranges);
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
                if let Some(live) = live_failures {
                    live.bytes.store(failed.bytes, Ordering::Relaxed);
                    live.count.store(failed.count, Ordering::Release);
                }
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

fn terminate(child: &mut Child) -> Option<ExitStatus> {
    if let Ok(Some(status)) = child.try_wait() {
        return Some(status);
    }
    let process_group = -(child.id() as libc::pid_t);
    // The child was started in its own process group. A graceful signal lets
    // fio stop submitting I/O and export bandwidth data collected so far.
    unsafe { libc::kill(process_group, libc::SIGTERM) };
    let start = Instant::now();
    while start.elapsed() < INTERRUPT_GRACE {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        thread::sleep(POLL_INTERVAL);
    }
    // The PID is still reserved until the child is reaped.
    unsafe { libc::kill(process_group, libc::SIGKILL) };
    child.wait().ok()
}
