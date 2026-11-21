mod device;
mod fio;
mod run_log;
mod temperature_monitor;

use std::fs::File;
use std::io::BufReader;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use clap::{Args, Parser, Subcommand};
use mini_qual::plan;
use mini_qual::plan::{Budget, CapacityRange, RoundTiming, TimingModel};
use mini_qual::thermal;
use mini_qual::thermal::{RetentionParameters, WearWindow};
use mini_qual::worker::{Outcome, Worker};
use serde_json::json;
use uuid::Uuid;

use crate::device::Device;
use crate::device::DeviceLease;
use crate::fio::Fio;
use crate::run_log::RunLog;
use crate::temperature_monitor::TemperatureSource;

#[derive(Parser)]
#[command(version, about = "NVMe SSD 高温顺序磨损与断电 retention 准备")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// 按写入预算磨损，准备 retention 数据，并向断电协调服务交接
    Run(RunArgs),
    /// 在专用盘上持续预测试，Ctrl+C 后用完整轮次推荐 NAND 物理容量
    Pretest(PretestArgs),
    /// 使用正式运行保存的实测壳温时间线重新计算 retention 建议
    Retention(RetentionArgs),
}

#[derive(Args)]
struct DeviceArgs {
    /// 目标 NVMe 控制器序列号
    #[arg(long)]
    sn: String,
    /// 目标 PE，单位 K，如 3.25 表示 3250
    #[arg(long, value_parser = plan::parse_pe)]
    pe: u64,
    /// 日志根目录，每次调用创建独立子目录
    #[arg(long, default_value = "./qual-logs")]
    log_dir: PathBuf,
    /// fio 可执行文件
    #[arg(long, default_value = "fio")]
    fio: PathBuf,
    /// SSD 外壳温度 HTTP GET 地址
    #[arg(long, default_value = temperature_monitor::DEFAULT_URL)]
    temperature_url: String,
    /// 壳温数值的 JSON Pointer（响应结构与内置识别规则不同时使用）
    #[arg(long)]
    temperature_pointer: Option<String>,
    /// 温度查询间隔，单位秒
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..=86400))]
    temperature_sample_interval_seconds: u64,
}

#[derive(Args)]
struct RetentionConditions {
    /// retention 基准保存温度，单位 °C
    #[arg(long, allow_negative_numbers = true)]
    retention_baseline_temperature_c: f64,
    /// retention 基准保存时长，单位小时
    #[arg(long)]
    retention_baseline_duration_hours: f64,
    /// 实际断电 retention 温度，单位 °C
    #[arg(long, allow_negative_numbers = true)]
    retention_test_temperature_c: f64,
}

impl From<&RetentionConditions> for RetentionParameters {
    fn from(value: &RetentionConditions) -> Self {
        Self {
            retention_baseline_temperature_c: value.retention_baseline_temperature_c,
            retention_baseline_duration_hours: value.retention_baseline_duration_hours,
            retention_test_temperature_c: value.retention_test_temperature_c,
        }
    }
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    device: DeviceArgs,
    /// 正式试验盘已烧录的 NAND 物理容量
    #[arg(long, value_parser = plan::parse_capacity)]
    nand_capacity: u64,
    #[command(flatten)]
    retention: RetentionConditions,
    /// 部署方指定的断电协调批次 UUID
    #[arg(long)]
    run_id: Uuid,
    /// 断电协调服务 HTTP 地址
    #[arg(long, default_value = "http://127.0.0.1:17653")]
    coordinator: String,
}

#[derive(Args)]
struct PretestArgs {
    #[command(flatten)]
    device: DeviceArgs,
    /// 预测试盘当前烧录的 NAND 物理容量，用作候选容量的步进起点
    #[arg(long, value_parser = plan::parse_capacity)]
    pretest_nand_capacity: u64,
    /// 从预测试盘当前 NAND 容量向上或向下调整时每档的字节差
    #[arg(long, value_parser = plan::parse_capacity)]
    nand_capacity_step: u64,
    /// 候选 NAND 物理容量下限
    #[arg(long, value_parser = plan::parse_capacity)]
    minimum_nand_capacity: u64,
    /// 候选 NAND 物理容量上限
    #[arg(long, value_parser = plan::parse_capacity)]
    maximum_nand_capacity: u64,
}

#[derive(Args)]
struct RetentionArgs {
    /// 正式运行生成的单次运行目录
    #[arg(long)]
    log_dir: PathBuf,
    #[command(flatten)]
    conditions: RetentionConditions,
}

fn main() -> std::process::ExitCode {
    let result = match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::Pretest(args) => pretest(args),
        Command::Retention(args) => retention(args),
    };
    if let Err(error) = result {
        eprintln!("错误 {error:#}");
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}

fn install_signal_handlers() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    for signal in [libc::SIGINT, libc::SIGTERM] {
        let stop = Arc::clone(&stop);
        // SAFETY: the handler only performs a lock-free atomic bool store.
        unsafe {
            signal_hook_registry::register(signal, move || stop.store(true, Ordering::Release))
        }
        .context("installing signal handler")?;
    }
    Ok(stop)
}

fn check_running(stop: &AtomicBool) -> Result<()> {
    if stop.load(Ordering::Acquire) {
        return Err(Interrupted.into());
    }
    Ok(())
}

#[derive(Debug)]
struct Interrupted;

impl std::fmt::Display for Interrupted {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("execution interrupted")
    }
}

impl std::error::Error for Interrupted {}

fn open_log(
    args: &DeviceArgs,
    device: &Device,
    lease: &DeviceLease,
    command: &str,
) -> Result<RunLog> {
    let mut log = RunLog::create(&args.log_dir)?;
    eprintln!("试验日志 {}", log.dir.display());
    log.event(
        "run_inputs",
        json!({
            "command": command,
            "serial": device.serial,
            "device": device.path,
            "host_visible_capacity_bytes": device.host_visible_capacity_bytes,
            "logical_block_bytes": device.logical_block_bytes,
            "pe": args.pe,
            "fio": args.fio,
            "temperature_url": args.temperature_url,
            "temperature_pointer": args.temperature_pointer,
            "temperature_sample_interval_seconds": args.temperature_sample_interval_seconds,
            "endurance_reference_temperature_c": thermal::ENDURANCE_REFERENCE_TEMPERATURE_C,
            "endurance_reference_duration_seconds": thermal::ENDURANCE_REFERENCE_DURATION_SECONDS,
            "endurance_activation_energy_ev": thermal::ENDURANCE_ACTIVATION_ENERGY_EV,
            "retention_activation_energy_ev": thermal::RETENTION_ACTIVATION_ENERGY_EV,
        }),
    )?;
    if !lease.partially_inspected_pids.is_empty() {
        eprintln!("部分同 UID 进程的占用信息受内核访问限制，详见试验日志。");
        log.event(
            "process_inspection_restricted",
            json!({"pids": lease.partially_inspected_pids}),
        )?;
    }
    Ok(log)
}

fn temperature_source(args: &DeviceArgs) -> Result<TemperatureSource> {
    TemperatureSource::new(
        &args.temperature_url,
        args.temperature_pointer.clone(),
        Duration::from_secs(args.temperature_sample_interval_seconds),
    )
}

fn run(args: RunArgs) -> Result<()> {
    let stop = install_signal_handlers()?;
    let parameters = RetentionParameters::from(&args.retention);
    parameters.validate()?;
    let source = temperature_source(&args.device)?;
    let device = Device::discover(&args.device.sn)?;
    let budget = Budget::new(
        args.device.pe,
        args.nand_capacity,
        device.host_visible_capacity_bytes,
        device.logical_block_bytes,
    )?;
    let ownership = device.acquire()?;
    let mut log = open_log(&args.device, &device, &ownership, "run")?;
    let setup = (|| {
        log.event(
            "wear_parameters",
            json!({
                "nand_capacity_bytes": args.nand_capacity,
                "budget": budget,
                "retention": parameters,
                "coordinator": args.coordinator,
                "run_id": args.run_id,
            }),
        )?;
        let fio = Fio::new(args.device.fio.clone(), log.dir.clone(), &device, &stop)?;
        check_running(&stop)?;
        let worker = Worker::register(
            &args.coordinator,
            args.run_id,
            &format!("mini-qual/{}", device.serial),
            Arc::clone(&stop),
        )
        .context("registering the poweroff worker")?;
        Ok((fio, worker))
    })();
    let (fio, worker) = match setup {
        Ok(setup) => setup,
        Err(error) => return seal_trial(&mut log, Err(error)),
    };
    let operation: Result<()> = (|| {
        log.event(
            "poweroff_registered",
            json!({"instance_id": worker.instance_id(), "timing": worker.timing()}),
        )?;
        log.sync()?;
        worker.check_active()?;
        check_running(&stop)?;
        let monitor = source.start(&log.dir, log.started, false)?;
        let result = formal_io(
            &mut log,
            &device,
            &fio,
            &stop,
            budget,
            args.device.temperature_sample_interval_seconds,
        );
        let sampler_result = monitor.finish();
        let window = result?;
        sampler_result?;
        let samples = temperature_monitor::read_samples(&log.dir)?;
        let estimate = parameters.estimate(window, &samples)?;
        log.report("retention.json", &estimate)?;
        log.event("retention_estimate", serde_json::to_value(&estimate)?)?;
        println!(
            "建议断电 retention {:.6} 小时（{:.3}°C）；实测等效磨损 {:.6} 小时（{:.1}°C）。",
            estimate.recommended_hours,
            parameters.retention_test_temperature_c,
            estimate.wear_equivalent_hours_at_reference,
            thermal::ENDURANCE_REFERENCE_TEMPERATURE_C
        );
        std::io::Write::flush(&mut std::io::stdout())?;
        worker.check_active()?;
        Ok(())
    })();
    drop(ownership);
    let operation = match (operation, worker.check_active()) {
        (Err(error), Err(coordinator)) => Err(error.context(format!("{coordinator:#}"))),
        (operation, _) => operation,
    };
    let operation = seal_trial(&mut log, operation);
    let (outcome, message) = match &operation {
        Ok(()) => (
            Outcome::Succeeded,
            format!(
                "{} flushed; results in {}",
                device.serial,
                log.dir.display()
            ),
        ),
        Err(error) => (
            Outcome::Failed,
            format!(
                "{}: {error:#}; logs in {}",
                device.serial,
                log.dir.display()
            ),
        ),
    };
    let handoff = worker
        .complete(outcome, &message)
        .context("reporting final outcome to the poweroff coordinator");
    match (operation, handoff) {
        (Ok(()), result) => result,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(handoff)) => Err(error.context(format!("{handoff:#}"))),
    }
}

fn formal_io(
    log: &mut RunLog,
    device: &Device,
    fio: &Fio<'_>,
    stop: &AtomicBool,
    budget: Budget,
    temperature_sample_interval_seconds: u64,
) -> Result<WearWindow> {
    let mut trial = Trial {
        log,
        device,
        fio,
        stop,
        written_blocks: 0,
    };
    let wear_started = trial.log.started.elapsed().as_secs_f64();
    trial.log.event("wear_started", json!({"budget": budget}))?;
    let mut last_end = None;
    let rounds = budget.full_rounds + u128::from(budget.final_partial_round_blocks > 0);
    for round in 1..=rounds {
        let bytes = if round <= budget.full_rounds {
            device.host_visible_capacity_bytes
        } else {
            budget.final_partial_round_blocks * device.logical_block_bytes
        };
        let (_, ended) = trial.wear_round(round, bytes, last_end)?;
        last_end = Some(ended);
        if round < rounds {
            trial.log.sync()?;
        }
    }
    let wear_ended = trial.log.started.elapsed().as_secs_f64();
    let window = WearWindow {
        start_seconds: wear_started,
        end_seconds: wear_ended,
        temperature_sample_interval_seconds: temperature_sample_interval_seconds as f64,
    };

    trial
        .log
        .event("wear_finished", serde_json::to_value(window)?)?;
    let preparation = (|| {
        trial.trim("retention", rounds + 1)?;
        trial.write("retention", rounds + 1, device.host_visible_capacity_bytes)?;
        trial.flush("retention", rounds + 1)?;
        ensure!(
            trial.written_blocks == budget.total_host_write_budget_blocks,
            "host write budget mismatch"
        );
        Ok::<_, anyhow::Error>(())
    })();
    let window_saved = trial.log.report("wear-window.json", &window);
    preparation?;
    window_saved?;
    trial.log.event(
        "retention_prepared",
        json!({
            "host_visible_capacity_bytes": device.host_visible_capacity_bytes,
            "cumulative_written_blocks": trial.written_blocks,
        }),
    )?;
    Ok(window)
}

fn pretest(args: PretestArgs) -> Result<()> {
    let stop = install_signal_handlers()?;
    let source = temperature_source(&args.device)?;
    let device = Device::discover(&args.device.sn)?;
    let range = CapacityRange {
        pretest_nand_capacity_bytes: args.pretest_nand_capacity,
        nand_capacity_step_bytes: args.nand_capacity_step,
        minimum_nand_capacity_bytes: args.minimum_nand_capacity,
        maximum_nand_capacity_bytes: args.maximum_nand_capacity,
    };
    ensure!(
        args.pretest_nand_capacity > device.host_visible_capacity_bytes,
        "pretest NAND capacity must be larger than the host-visible capacity"
    );
    range.validate(
        device.host_visible_capacity_bytes,
        device.logical_block_bytes,
    )?;
    let ownership = device.acquire()?;
    let mut log = open_log(&args.device, &device, &ownership, "pretest")?;
    let result = (|| {
        log.event("capacity_parameters", serde_json::to_value(range)?)?;
        let fio = Fio::new(args.device.fio.clone(), log.dir.clone(), &device, &stop)?;
        let monitor = source.start(&log.dir, log.started, true)?;
        let mut trial = Trial {
            log: &mut log,
            device: &device,
            fio: &fio,
            stop: &stop,
            written_blocks: 0,
        };
        let mut timings = Vec::new();
        let mut last_end = None;
        let mut last_completed_elapsed = 0.0;
        let first_started_elapsed = trial.log.started.elapsed().as_secs_f64();
        let mut round = 1;
        let work_result = loop {
            if stop.load(Ordering::Acquire) {
                break Ok(());
            }
            match trial.wear_round(round, device.host_visible_capacity_bytes, last_end) {
                Ok((timing, ended)) => {
                    timings.push(timing);
                    last_end = Some(ended);
                    last_completed_elapsed = ended.duration_since(trial.log.started).as_secs_f64();
                    trial.log.sync()?;
                    round += 1;
                }
                Err(error) if error.is::<Interrupted>() => {
                    trial.log.event(
                        "incomplete_round",
                        json!({"round": round, "reason": format!("{error:#}")}),
                    )?;
                    break Ok(());
                }
                Err(error) => break Err(error),
            }
        };
        let sampler_result = monitor.finish();
        work_result?;
        sampler_result?;
        if timings.len() < 2 {
            trial.log.event(
                "insufficient_pretest",
                json!({"complete_rounds": timings.len()}),
            )?;
            eprintln!(
                "预测试数据不足，已完成 {} 轮，容量估算至少需要两轮。",
                timings.len()
            );
            return Ok(());
        }
        let model = TimingModel::from_rounds(&timings)?;
        let window = WearWindow {
            start_seconds: first_started_elapsed,
            end_seconds: last_completed_elapsed,
            temperature_sample_interval_seconds: args.device.temperature_sample_interval_seconds
                as f64,
        };
        let measured = temperature_monitor::read_samples(&trial.log.dir)
            .and_then(|samples| thermal::equivalent_wear_seconds(window, &samples));
        let (equivalent_rate, temperature_error) = match measured {
            Ok(seconds) => (
                Some(seconds / (window.end_seconds - window.start_seconds)),
                None,
            ),
            Err(error) => (None, Some(format!("{error:#}"))),
        };
        let recommendation = range.recommend(
            args.device.pe,
            device.host_visible_capacity_bytes,
            device.logical_block_bytes,
            model,
            thermal::ENDURANCE_REFERENCE_DURATION_SECONDS,
        )?;
        let report = json!({
            "host_visible_capacity_bytes": device.host_visible_capacity_bytes,
            "pe": args.device.pe,
            "range": range,
            "model": model,
            "recommendation": recommendation,
            "predicted_equivalent_wear_seconds_at_measured_temperatures":
                equivalent_rate.map(|rate| rate * recommendation.predicted_wear_seconds),
            "completed_rounds_temperature_window": window,
            "measured_equivalent_to_elapsed_ratio": equivalent_rate,
            "temperature_calculation_error": temperature_error,
            "endurance_reference_temperature_c": thermal::ENDURANCE_REFERENCE_TEMPERATURE_C,
        });
        trial.log.report("capacity-recommendation.json", &report)?;
        trial.log.event("capacity_recommendation", report)?;
        println!(
            "推荐 NAND 物理容量 {} Byte，档位偏移 {:+}，当前主机可见容量 {} Byte。",
            recommendation.nand_capacity_bytes,
            recommendation.nand_capacity_step_offset,
            device.host_visible_capacity_bytes
        );
        println!(
            "预计磨损 {:.6} 小时，企业级基准 {:.0} 小时，差值 {:+.6} 小时。",
            recommendation.predicted_wear_seconds / 3600.0,
            thermal::ENDURANCE_REFERENCE_DURATION_SECONDS / 3600.0,
            recommendation.predicted_difference_seconds / 3600.0
        );
        Ok(())
    })();
    drop(ownership);
    seal_trial(&mut log, result)
}

fn seal_trial(log: &mut RunLog, result: Result<()>) -> Result<()> {
    let record = match &result {
        Ok(()) => log.event("run_finished", json!({"outcome": "succeeded"})),
        Err(error) => log.event(
            "run_finished",
            json!({"outcome": "failed", "reason": format!("{error:#}")}),
        ),
    };
    let sync = log.sync();
    match (result, record.and(sync)) {
        (result, Ok(())) => result,
        (Ok(()), Err(error)) => Err(error.context("sealing trial logs")),
        (Err(error), Err(logging)) => {
            Err(error.context(format!("sealing trial logs: {logging:#}")))
        }
    }
}

fn retention(args: RetentionArgs) -> Result<()> {
    let parameters = RetentionParameters::from(&args.conditions);
    parameters.validate()?;
    let window_file =
        File::open(args.log_dir.join("wear-window.json")).context("opening wear window")?;
    let window =
        serde_json::from_reader(BufReader::new(window_file)).context("reading wear window")?;
    let samples = temperature_monitor::read_samples(&args.log_dir)?;
    let estimate = parameters.estimate(window, &samples)?;
    println!("{}", serde_json::to_string_pretty(&estimate)?);
    Ok(())
}

struct Trial<'a> {
    log: &'a mut RunLog,
    device: &'a Device,
    fio: &'a Fio<'a>,
    stop: &'a AtomicBool,
    written_blocks: u128,
}

impl Trial<'_> {
    fn wear_round(
        &mut self,
        round: u128,
        bytes: u64,
        previous_end: Option<Instant>,
    ) -> Result<(RoundTiming, Instant)> {
        let started = Instant::now();
        let round_gap_seconds = previous_end.map(|end| started.duration_since(end).as_secs_f64());
        self.log.event("round_started", json!({
            "round": round, "write_bytes": bytes, "read_bytes": bytes, "round_gap_seconds": round_gap_seconds,
        }))?;
        let phase = "wear";
        let trim_seconds = self.trim(phase, round)?;
        let write_seconds = self.write(phase, round, bytes)?;
        let flush_seconds = self.flush(phase, round)?;
        let read_seconds = self
            .fio_stage(phase, round, "read", |fio| fio.read(phase, round, bytes))?
            .duration
            .as_secs_f64();
        let locate_seconds = self
            .fio_stage(phase, round, "locate", |fio| fio.locate(phase, round))?
            .duration
            .as_secs_f64();
        let ended = std::time::Instant::now();
        let measured = trim_seconds + write_seconds + flush_seconds + read_seconds + locate_seconds;
        let timing = RoundTiming {
            trim_seconds,
            write_seconds,
            flush_seconds,
            read_seconds,
            locate_seconds,
            phase_gap_seconds: (ended.duration_since(started).as_secs_f64() - measured).max(0.0),
            round_gap_seconds,
        };
        self.log.event(
            "round_finished",
            json!({
                "round": round,
                "write_bytes": bytes,
                "read_bytes": bytes,
                "cumulative_written_blocks": self.written_blocks,
                "timing": timing,
            }),
        )?;
        Ok((timing, ended))
    }

    fn trim(&mut self, phase: &str, round: u128) -> Result<f64> {
        Ok(self
            .fio_stage(phase, round, "trim", |fio| fio.trim(phase, round))?
            .duration
            .as_secs_f64())
    }

    fn write(&mut self, phase: &str, round: u128, bytes: u64) -> Result<f64> {
        let stage = self.fio_stage(phase, round, "write", |fio| fio.write(phase, round, bytes))?;
        ensure!(
            stage.successful_bytes == bytes,
            "incomplete sequential write"
        );
        self.written_blocks += u128::from(bytes / self.device.logical_block_bytes);
        self.log.event(
            "write_budget_progress",
            json!({
                "phase": phase, "round": round, "cumulative_written_blocks": self.written_blocks,
                "cumulative_written_bytes": self.written_blocks * u128::from(self.device.logical_block_bytes),
            }),
        )?;
        Ok(stage.duration.as_secs_f64())
    }

    fn flush(&mut self, phase: &str, round: u128) -> Result<f64> {
        self.stage_start(phase, round, "flush")?;
        let started = Instant::now();
        if let Err(error) = self.device.flush() {
            return self.stage_error(phase, round, "flush", error);
        }
        let duration = started.elapsed().as_secs_f64();
        self.log.event(
            "stage_finished",
            json!({
                "phase": phase, "round": round, "operation": "flush", "seconds": duration,
            }),
        )?;
        Ok(duration)
    }

    fn fio_stage(
        &mut self,
        phase: &str,
        round: u128,
        operation: &str,
        action: impl FnOnce(&Fio<'_>) -> Result<fio::Stage>,
    ) -> Result<fio::Stage> {
        self.stage_start(phase, round, operation)?;
        let stage = match action(self.fio) {
            Ok(stage) => stage,
            Err(error) => return self.stage_error(phase, round, operation, error),
        };
        let seconds = stage.duration.as_secs_f64();
        self.log.event("stage_finished", json!({
            "phase": phase, "round": round, "operation": operation,
            "seconds": seconds,
            "attempted_bytes": stage.attempted_bytes, "successful_bytes": stage.successful_bytes,
            "error_count": stage.errors,
            "attempted_bytes_per_second": (seconds > 0.0).then(|| stage.attempted_bytes as f64 / seconds),
            "successful_bytes_per_second": (seconds > 0.0).then(|| stage.successful_bytes as f64 / seconds),
        }))?;
        Ok(stage)
    }

    fn stage_start(&mut self, phase: &str, round: u128, operation: &str) -> Result<()> {
        check_running(self.stop)?;
        self.log.event(
            "stage_started",
            json!({"phase": phase, "round": round, "operation": operation}),
        )?;
        if let Err(error) = self.device.check_ready() {
            return self.stage_error(phase, round, operation, error);
        }
        check_running(self.stop)
    }

    fn stage_error<T>(
        &mut self,
        phase: &str,
        round: u128,
        operation: &str,
        error: anyhow::Error,
    ) -> Result<T> {
        self.log.event("stage_error", json!({
            "phase": phase, "round": round, "operation": operation, "reason": format!("{error:#}"),
        }))?;
        Err(error).with_context(|| format!("{phase} round {round} {operation}"))
    }
}
