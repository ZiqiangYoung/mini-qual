use std::cmp;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use reqwest::{Client, Method, StatusCode, Url, redirect};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::runtime::{Builder, Runtime};
use uuid::Uuid;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const RETRY_BUDGET: Duration = Duration::from_secs(30);
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);
const MAX_BACKOFF: Duration = Duration::from_secs(2);
const MAX_REQUEST_BYTES: usize = 16 * 1024;
const MAX_MESSAGE_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    Succeeded,
    Failed,
}

impl Outcome {
    fn state(self) -> WorkerState {
        match self {
            Self::Succeeded => WorkerState::Succeeded,
            Self::Failed => WorkerState::Failed,
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
enum WorkerState {
    Active,
    Succeeded,
    Failed,
    Lost,
    TimedOut,
    #[serde(other)]
    Unknown,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
pub struct Timing {
    pub heartbeat_interval_ms: u64,
    pub heartbeat_timeout_ms: u64,
}

#[derive(Deserialize)]
struct Registration {
    run_id: Uuid,
    instance_id: Uuid,
    state: WorkerState,
    task_timeout_ms: Option<u64>,
    #[serde(flatten)]
    timing: Timing,
}

#[derive(Deserialize)]
struct WorkerStatus {
    instance_id: Uuid,
    state: WorkerState,
}

#[derive(Deserialize)]
struct RunStatus {
    run_id: Uuid,
    workers: Vec<WorkerStatus>,
}

#[derive(Debug, Deserialize)]
struct ApiError {
    code: String,
    message: String,
    current_state: Option<WorkerState>,
    current_run_id: Option<Uuid>,
}

#[derive(Deserialize)]
struct ErrorResponse {
    error: ApiError,
}

#[derive(Debug)]
enum RequestError {
    Transport(reqwest::Error),
    Http(StatusCode, Option<ApiError>),
    Protocol(serde_json::Error),
}

impl RequestError {
    fn retryable(&self) -> bool {
        match self {
            Self::Transport(_) => true,
            Self::Http(status, _) => status.is_server_error(),
            Self::Protocol(_) => false,
        }
    }

    fn terminal_state(&self) -> Option<WorkerState> {
        match self {
            Self::Http(_, Some(error)) if error.code == "worker_terminal" => {
                Some(error.current_state.unwrap_or(WorkerState::Unknown))
            }
            _ => None,
        }
    }

    fn should_reconcile(&self) -> bool {
        match self {
            Self::Http(_, Some(error)) => {
                matches!(error.code.as_str(), "result_conflict" | "worker_terminal")
                    || self.retryable()
            }
            // A successful HTTP response with an unreadable body may follow a committed result.
            Self::Protocol(_) => true,
            _ => self.retryable(),
        }
    }

    fn could_have_reached_server(&self) -> bool {
        !matches!(self, Self::Transport(error) if error.is_connect())
    }
}

impl fmt::Display for RequestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Transport(error) => write!(f, "HTTP transport error: {error}"),
            Self::Http(status, Some(error)) => {
                write!(f, "HTTP {status} ({}): {}", error.code, error.message)?;
                if let Some(state) = error.current_state {
                    write!(f, " (state: {state:?})")?;
                }
                if let Some(run_id) = error.current_run_id {
                    write!(f, " (current run: {run_id})")?;
                }
                Ok(())
            }
            Self::Http(status, None) => write!(f, "HTTP {status}"),
            Self::Protocol(error) => write!(f, "invalid coordinator response: {error}"),
        }
    }
}

impl std::error::Error for RequestError {}

struct Session {
    client: Client,
    runtime: Runtime,
    run_url: String,
    worker_url: String,
    run_id: Uuid,
    instance_id: Uuid,
}

impl Session {
    fn new(run_url: String, run_id: Uuid, instance_id: Uuid) -> Result<Self> {
        let runtime = Builder::new_current_thread()
            .enable_all()
            .build()
            .context("initializing coordinator HTTP runtime")?;
        let client = Client::builder()
            .no_proxy()
            .redirect(redirect::Policy::none())
            .build()
            .context("initializing coordinator HTTP client")?;
        Ok(Self {
            client,
            runtime,
            worker_url: format!("{run_url}/workers/{instance_id}"),
            run_url,
            run_id,
            instance_id,
        })
    }

    fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        url: &str,
        payload: Option<&Value>,
        timeout: Duration,
        expected: &[StatusCode],
    ) -> std::result::Result<T, RequestError> {
        let mut request = self.client.request(method, url).timeout(timeout);
        if let Some(payload) = payload {
            request = request.json(payload);
        }
        self.runtime.block_on(async {
            let response = request.send().await.map_err(RequestError::Transport)?;
            let status = response.status();
            if !expected.contains(&status) {
                let error = response
                    .json::<ErrorResponse>()
                    .await
                    .ok()
                    .map(|body| body.error);
                return Err(RequestError::Http(status, error));
            }
            let body = response.bytes().await.map_err(RequestError::Transport)?;
            serde_json::from_slice(&body).map_err(RequestError::Protocol)
        })
    }

    fn post(
        &self,
        action: &str,
        payload: &Value,
        timeout: Duration,
    ) -> std::result::Result<WorkerStatus, RequestError> {
        self.request(
            Method::POST,
            &format!("{}/{action}", self.worker_url),
            Some(payload),
            timeout,
            &[StatusCode::OK],
        )
    }
}

struct Health {
    lease_expires_at: Instant,
    terminal: Option<WorkerState>,
    failure: Option<String>,
}

impl Health {
    fn expiry(&self, now: Instant) -> Option<&'static str> {
        (now >= self.lease_expires_at).then_some(
            "coordinator worker heartbeat lease could not be renewed before its deadline",
        )
    }
}

fn lock_health(health: &Mutex<Health>) -> MutexGuard<'_, Health> {
    health
        .lock()
        .expect("coordinator worker health is poisoned")
}

fn remaining(deadline: Instant) -> Option<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|time| !time.is_zero())
}

fn validate_endpoint(endpoint: &str) -> Result<String> {
    let endpoint = Url::parse(endpoint).context("invalid coordinator endpoint")?;
    ensure!(
        endpoint.scheme() == "http" && endpoint.has_host(),
        "coordinator endpoint must use HTTP and include a host"
    );
    ensure!(
        endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none(),
        "coordinator endpoint cannot contain credentials, a query, or a fragment"
    );
    Ok(endpoint.as_str().trim_end_matches('/').to_owned())
}

fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    let mut end = cmp::min(max_bytes, text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

struct Heartbeat {
    shutdown: Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Heartbeat {
    fn drop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn heartbeat(
    session: &Session,
    health: &Mutex<Health>,
    stop: &AtomicBool,
    timing: Timing,
    shutdown: Receiver<()>,
) {
    let interval = Duration::from_millis(timing.heartbeat_interval_ms);
    let lease = Duration::from_millis(timing.heartbeat_timeout_ms);
    let payload = json!({});
    loop {
        if shutdown.try_recv().is_ok() {
            return;
        }
        let sent_at = Instant::now();
        let timeout = {
            let mut health = lock_health(health);
            if let Some(reason) = health.expiry(sent_at) {
                health.failure = Some(reason.to_owned());
                stop.store(true, Ordering::Release);
                return;
            }
            cmp::min(
                REQUEST_TIMEOUT,
                health.lease_expires_at.duration_since(sent_at),
            )
        };
        let reply = session.post("heartbeat", &payload, timeout);
        let mut health = lock_health(health);
        match reply {
            Ok(status) if status.instance_id != session.instance_id => {
                health.failure = Some("heartbeat returned a different instance ID".to_owned());
            }
            Ok(status) if status.state == WorkerState::Active => {
                if let Some(expiry) = sent_at.checked_add(lease) {
                    health.lease_expires_at = expiry;
                } else {
                    health.failure = Some("heartbeat lease is outside the timer range".to_owned());
                }
            }
            Ok(status) => health.terminal = Some(status.state),
            Err(error) if error.retryable() => {}
            Err(error) => {
                health.terminal = error.terminal_state();
                health.failure = Some(format!("coordinator heartbeat failed: {error}"));
            }
        }
        if health.terminal.is_some() || health.failure.is_some() {
            stop.store(true, Ordering::Release);
            return;
        }
        let next = sent_at
            .checked_add(interval)
            .unwrap_or(health.lease_expires_at);
        let wait =
            cmp::min(next, health.lease_expires_at).saturating_duration_since(Instant::now());
        drop(health);
        if !matches!(
            shutdown.recv_timeout(wait),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            return;
        }
    }
}

/// An active shutdown-barrier registration for this process.
pub struct Worker {
    session: Session,
    timing: Timing,
    health: Arc<Mutex<Health>>,
    _heartbeat: Heartbeat,
}

impl Worker {
    pub fn register(
        endpoint: &str,
        run_id: Uuid,
        name: &str,
        stop: Arc<AtomicBool>,
    ) -> Result<Self> {
        ensure!(
            !name.is_empty() && name.len() <= 128,
            "worker name must be 1–128 UTF-8 bytes"
        );
        let endpoint = validate_endpoint(endpoint)?;
        let instance_id = Uuid::new_v4();
        let run_url = format!("{endpoint}/v1/runs/{run_id}");
        let session = Session::new(run_url.clone(), run_id, instance_id)?;
        let heartbeat_session = Session::new(run_url, run_id, instance_id)?;
        let payload = json!({ "name": name });

        let registration_deadline = Instant::now() + RETRY_BUDGET;
        let mut backoff = INITIAL_BACKOFF;
        let mut first_delivered_request_at = None;
        let (registration, registered_at): (Registration, Instant) = loop {
            ensure!(
                !stop.load(Ordering::Acquire),
                "coordinator registration unconfirmed for worker {instance_id}: interrupted"
            );
            let timeout = remaining(registration_deadline).with_context(|| {
                format!("coordinator registration unconfirmed for worker {instance_id}")
            })?;
            let sent_at = Instant::now();
            let reply = session.request(
                Method::PUT,
                &session.worker_url,
                Some(&payload),
                cmp::min(REQUEST_TIMEOUT, timeout),
                &[StatusCode::CREATED, StatusCode::OK],
            );
            match reply {
                Ok(registration) => {
                    break (registration, first_delivered_request_at.unwrap_or(sent_at));
                }
                Err(error) if error.retryable() => {
                    // A request with a lost response may have already started the heartbeat lease.
                    if error.could_have_reached_server() {
                        first_delivered_request_at.get_or_insert(sent_at);
                    }
                    let time_left = remaining(registration_deadline).with_context(|| {
                        format!(
                            "coordinator registration unconfirmed for worker {instance_id}: {error}"
                        )
                    })?;
                    thread::sleep(cmp::min(backoff, time_left));
                    backoff = cmp::min(MAX_BACKOFF, backoff * 2);
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("registering coordinator worker {instance_id}"));
                }
            }
        };
        ensure!(
            registration.run_id == run_id && registration.instance_id == instance_id,
            "coordinator registration returned mismatched run or instance ID"
        );
        ensure!(
            registration.state == WorkerState::Active,
            "coordinator worker {instance_id} registered in a non-active state: {:?}",
            registration.state
        );
        ensure!(
            registration.task_timeout_ms.unwrap_or(0) == 0,
            "coordinator reports a finite worker task timeout; disable it before starting endurance"
        );
        let timing = registration.timing;
        ensure!(
            timing.heartbeat_interval_ms > 0 && timing.heartbeat_timeout_ms > 0,
            "coordinator returned a zero heartbeat interval or timeout"
        );
        let lease_expires_at = registered_at
            .checked_add(Duration::from_millis(timing.heartbeat_timeout_ms))
            .context("coordinator heartbeat lease is outside the timer range")?;
        let health = Arc::new(Mutex::new(Health {
            lease_expires_at,
            terminal: None,
            failure: None,
        }));
        let (shutdown, receive_shutdown) = mpsc::channel();
        let heartbeat_health = Arc::clone(&health);
        let thread = thread::Builder::new()
            .name("coordinator-heartbeat".to_owned())
            .spawn(move || {
                heartbeat(
                    &heartbeat_session,
                    &heartbeat_health,
                    &stop,
                    timing,
                    receive_shutdown,
                )
            })
            .context("starting coordinator heartbeat")?;
        let worker = Self {
            session,
            timing,
            health,
            _heartbeat: Heartbeat {
                shutdown,
                thread: Some(thread),
            },
        };
        worker.check_active()?;
        Ok(worker)
    }

    pub fn instance_id(&self) -> Uuid {
        self.session.instance_id
    }

    pub fn timing(&self) -> Timing {
        self.timing
    }

    /// Checks the coordinator lease independently of user interruption.
    pub fn check_active(&self) -> Result<()> {
        let health = lock_health(&self.health);
        if let Some(failure) = &health.failure {
            bail!("{failure}");
        }
        if let Some(state) = health.terminal {
            bail!("coordinator worker became terminal: {state:?}");
        }
        if let Some(reason) = health.expiry(Instant::now()) {
            bail!("{reason}");
        }
        Ok(())
    }

    /// Reports the final outcome after all work and local cleanup have completed.
    pub fn complete(self, outcome: Outcome, message: &str) -> Result<()> {
        let mut payload = json!({ "outcome": outcome });
        if !message.is_empty() {
            payload["message"] = json!(truncate_utf8(message, MAX_MESSAGE_BYTES));
            // JSON escapes can expand control characters to as many as six bytes each.
            if payload.to_string().len() > MAX_REQUEST_BYTES {
                payload["message"] = json!(truncate_utf8(message, MAX_MESSAGE_BYTES / 2));
            }
        }
        let deadline = Instant::now() + RETRY_BUDGET;
        let mut backoff = INITIAL_BACKOFF;
        let reason = loop {
            let health = lock_health(&self.health);
            if let Some(state) = health.terminal {
                if state == WorkerState::Unknown {
                    break "heartbeat reported an unspecified terminal state".to_owned();
                }
                return confirm_state(outcome, state);
            }
            let failure = health.failure.clone();
            let send_deadline = cmp::min(deadline, health.lease_expires_at);
            drop(health);
            if let Some(failure) = failure {
                break failure;
            }
            let Some(timeout) = remaining(send_deadline) else {
                break "coordinator completion deadline reached".to_owned();
            };
            match self
                .session
                .post("complete", &payload, cmp::min(REQUEST_TIMEOUT, timeout))
            {
                Ok(status) if status.instance_id == self.session.instance_id => {
                    return confirm_state(outcome, status.state);
                }
                Ok(_) => break "completion returned a different instance ID".to_owned(),
                Err(error) if error.retryable() => {
                    let Some(time_left) = remaining(deadline) else {
                        break error.to_string();
                    };
                    thread::sleep(cmp::min(backoff, time_left));
                    backoff = cmp::min(MAX_BACKOFF, backoff * 2);
                }
                Err(error) if error.should_reconcile() => break error.to_string(),
                Err(error) => {
                    if let Some(state) = self.known_terminal_state() {
                        return confirm_state(outcome, state);
                    }
                    return Err(error).context("reporting coordinator completion");
                }
            }
        };
        self.reconcile_completion(outcome, &reason)
    }

    fn known_terminal_state(&self) -> Option<WorkerState> {
        lock_health(&self.health)
            .terminal
            .filter(|state| *state != WorkerState::Unknown)
    }

    fn reconcile_completion(&self, outcome: Outcome, reason: &str) -> Result<()> {
        if let Some(state) = self.known_terminal_state() {
            return confirm_state(outcome, state);
        }
        let run_status: std::result::Result<RunStatus, _> = self.session.request(
            Method::GET,
            &self.session.run_url,
            None,
            REQUEST_TIMEOUT,
            &[StatusCode::OK],
        );
        if let Some(state) = self.known_terminal_state() {
            return confirm_state(outcome, state);
        }
        match run_status {
            Ok(status) if status.run_id == self.session.run_id => {
                if let Some(worker) = status
                    .workers
                    .iter()
                    .find(|worker| worker.instance_id == self.session.instance_id)
                {
                    if worker.state != WorkerState::Active {
                        return confirm_state(outcome, worker.state);
                    }
                }
                bail!("coordinator completion unconfirmed: {reason}");
            }
            Ok(_) => bail!(
                "coordinator completion unconfirmed: {reason}; status returned a different run ID"
            ),
            Err(error) => Err(anyhow!(
                "coordinator completion unconfirmed: {reason}; status query failed: {error}"
            )),
        }
    }
}

fn confirm_state(outcome: Outcome, state: WorkerState) -> Result<()> {
    if state == outcome.state() {
        return Ok(());
    }
    match state {
        WorkerState::Succeeded | WorkerState::Failed => {
            bail!("coordinator completion conflict: expected {outcome:?}, recorded {state:?}");
        }
        WorkerState::Lost | WorkerState::TimedOut => {
            bail!("coordinator released the worker before completion: {state:?}");
        }
        WorkerState::Active | WorkerState::Unknown => {
            bail!("coordinator completion unconfirmed: worker state {state:?}");
        }
    }
}
