use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TEMPERATURE_HISTORY_LIMIT: usize = 120;
const BANDWIDTH_HISTORY_LIMIT: usize = 128;

pub const BANDWIDTH_HISTORY_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Mode {
    Run,
    Pretest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Preparing,
    Wear,
    Retention,
}

impl Phase {
    pub fn name(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Wear => "wear",
            Self::Retention => "retention",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageKind {
    Trim,
    Write,
    Flush,
    Read,
    Locate,
    Handoff,
}

impl StageKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Trim => "trim",
            Self::Write => "write",
            Self::Flush => "flush",
            Self::Read => "read",
            Self::Locate => "locate",
            Self::Handoff => "handoff",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StageStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Interrupted,
}

#[derive(Clone, Debug)]
pub struct TrialInfo {
    pub mode: Mode,
    pub serial: String,
    pub device_path: PathBuf,
    pub log_dir: PathBuf,
    pub started: Instant,
    pub total_wear_rounds: Option<u128>,
    pub total_write_budget_bytes: Option<u128>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct IoProgress {
    pub attempted_bytes: u64,
    pub successful_bytes: u64,
    pub errors: u64,
    pub bytes_per_second: Option<f64>,
    pub attempted_bytes_per_second: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
pub struct BandwidthSample {
    pub observed_at: Instant,
    pub bytes_per_second: f64,
}

#[derive(Clone, Debug)]
pub struct Task {
    pub kind: StageKind,
    pub status: StageStatus,
    pub target_bytes: Option<u64>,
    pub started: Option<Instant>,
    pub duration: Option<Duration>,
    pub progress: Option<IoProgress>,
    pub progress_at: Option<Instant>,
    pub recent_bandwidth: VecDeque<BandwidthSample>,
}

impl Task {
    fn pending(kind: StageKind, target_bytes: Option<u64>) -> Self {
        Self {
            kind,
            status: StageStatus::Pending,
            target_bytes,
            started: None,
            duration: None,
            progress: None,
            progress_at: None,
            recent_bandwidth: VecDeque::new(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct TemperatureReading {
    pub query_time: String,
    pub observed_at: Instant,
    pub value: Result<f64, String>,
}

#[derive(Clone, Debug)]
pub struct TemperatureSample {
    pub elapsed_seconds: f64,
    pub temperature_c: f64,
    pub query_time: String,
}

#[derive(Clone, Debug, Default)]
pub struct TemperatureState {
    pub latest: Option<TemperatureReading>,
    pub recent: VecDeque<TemperatureSample>,
}

#[derive(Clone, Debug)]
pub struct Snapshot {
    pub info: TrialInfo,
    pub phase: Phase,
    pub round: Option<u128>,
    pub completed_rounds: u128,
    pub tasks: Vec<Task>,
    pub cumulative_written_bytes: u128,
    pub read_errors: u128,
    pub locate_errors: u128,
    pub temperature: TemperatureState,
    pub message: Option<String>,
    pub last_error: Option<String>,
}

struct Shared {
    active: AtomicBool,
    state: Mutex<Snapshot>,
}

#[derive(Clone, Default)]
pub struct Telemetry(Option<Arc<Shared>>);

impl Telemetry {
    pub fn new(info: TrialInfo) -> Self {
        let state = Snapshot {
            info,
            phase: Phase::Preparing,
            round: None,
            completed_rounds: 0,
            tasks: Vec::new(),
            cumulative_written_bytes: 0,
            read_errors: 0,
            locate_errors: 0,
            temperature: TemperatureState::default(),
            message: None,
            last_error: None,
        };
        Self(Some(Arc::new(Shared {
            active: AtomicBool::new(true),
            state: Mutex::new(state),
        })))
    }

    pub fn is_active(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|shared| shared.active.load(Ordering::Acquire))
    }

    pub fn disable(&self) {
        if let Some(shared) = &self.0 {
            shared.active.store(false, Ordering::Release);
        }
    }

    pub fn snapshot(&self) -> Option<Snapshot> {
        let shared = self.0.as_ref()?;
        shared.state.lock().ok().map(|state| state.clone())
    }

    fn update(&self, apply: impl FnOnce(&mut Snapshot)) {
        if let Some(shared) = &self.0 {
            if shared.active.load(Ordering::Acquire) {
                if let Ok(mut state) = shared.state.lock() {
                    apply(&mut state);
                }
            }
        }
    }

    pub fn message(&self, message: &str) {
        self.update(|state| state.message = Some(message.into()));
    }

    pub fn start_round(
        &self,
        phase: Phase,
        round: u128,
        host_visible_capacity_bytes: u64,
        wear_bytes: u64,
    ) {
        self.update(|state| {
            state.phase = phase;
            state.round = Some(round);
            state.message = None;
            state.tasks = vec![
                Task::pending(StageKind::Trim, Some(host_visible_capacity_bytes)),
                Task::pending(StageKind::Write, Some(wear_bytes)),
                Task::pending(StageKind::Flush, None),
            ];
            if phase == Phase::Retention {
                state.tasks.push(Task::pending(StageKind::Handoff, None));
            } else {
                state
                    .tasks
                    .push(Task::pending(StageKind::Read, Some(wear_bytes)));
                state.tasks.push(Task::pending(StageKind::Locate, None));
            }
        });
    }

    pub fn round_completed(&self, round: u128) {
        self.update(|state| state.completed_rounds = round);
    }

    pub fn stage_started(&self, kind: StageKind) {
        self.update(|state| {
            if let Some(task) = state.tasks.iter_mut().find(|task| task.kind == kind) {
                task.status = StageStatus::Running;
                task.started = Some(Instant::now());
            }
        });
    }

    pub fn io_progress(&self, progress: IoProgress) {
        self.update(|state| {
            if let Some(task) = state
                .tasks
                .iter_mut()
                .find(|task| task.status == StageStatus::Running)
            {
                let observed_at = Instant::now();
                task.progress = Some(progress);
                task.progress_at = Some(observed_at);
                if let Some(bytes_per_second) = progress
                    .bytes_per_second
                    .filter(|rate| rate.is_finite() && *rate >= 0.0)
                {
                    if task.recent_bandwidth.len() == BANDWIDTH_HISTORY_LIMIT {
                        task.recent_bandwidth.pop_front();
                    }
                    task.recent_bandwidth.push_back(BandwidthSample {
                        observed_at,
                        bytes_per_second,
                    });
                }
            }
        });
    }

    pub fn stage_completed(
        &self,
        kind: StageKind,
        duration: Duration,
        progress: Option<IoProgress>,
    ) {
        self.update(|state| {
            if let Some(task) = state.tasks.iter_mut().find(|task| task.kind == kind) {
                task.status = StageStatus::Completed;
                task.duration = Some(duration);
                task.progress = progress;
            }
            if let Some(progress) = progress {
                match kind {
                    StageKind::Write => {
                        state.cumulative_written_bytes += u128::from(progress.successful_bytes)
                    }
                    StageKind::Read => {
                        state.read_errors += u128::from(progress.errors);
                        if let Some(task) = state
                            .tasks
                            .iter_mut()
                            .find(|task| task.kind == StageKind::Locate)
                        {
                            task.target_bytes = Some(
                                progress
                                    .attempted_bytes
                                    .saturating_sub(progress.successful_bytes),
                            );
                        }
                    }
                    StageKind::Locate => state.locate_errors += u128::from(progress.errors),
                    _ => (),
                }
            }
        });
    }

    pub fn stage_failed(&self, kind: StageKind, interrupted: bool, reason: &str) {
        self.update(|state| {
            if let Some(task) = state.tasks.iter_mut().find(|task| task.kind == kind) {
                task.status = if interrupted {
                    StageStatus::Interrupted
                } else {
                    StageStatus::Failed
                };
                task.duration = task.started.map(|start| start.elapsed());
            }
            if !interrupted {
                state.last_error = Some(reason.into());
            }
        });
    }

    pub fn temperature(
        &self,
        query_time: &str,
        elapsed_seconds: f64,
        reading: &Result<f64, String>,
    ) {
        self.update(|state| {
            state.temperature.latest = Some(TemperatureReading {
                query_time: query_time.into(),
                observed_at: Instant::now(),
                value: reading.clone(),
            });
            if let Ok(temperature_c) = reading {
                let recent = &mut state.temperature.recent;
                if recent.len() == TEMPERATURE_HISTORY_LIMIT {
                    recent.pop_front();
                }
                recent.push_back(TemperatureSample {
                    elapsed_seconds,
                    temperature_c: *temperature_c,
                    query_time: query_time.into(),
                });
            }
        });
    }
}
