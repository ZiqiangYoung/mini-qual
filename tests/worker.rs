use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use mini_qual::worker::{Outcome, Worker};
use serde_json::Value;
use uuid::Uuid;

#[test]
fn worker_rejects_invalid_registration_inputs() {
    let run_id = Uuid::new_v4();
    let stop = Arc::new(AtomicBool::new(false));
    for (endpoint, name, timeout) in [
        ("https://127.0.0.1:17653", "test", None),
        ("http://127.0.0.1:17653", "", None),
        ("http://127.0.0.1:17653", "test", Some(0)),
    ] {
        assert!(Worker::register(endpoint, run_id, name, timeout, Arc::clone(&stop)).is_err());
    }
}

#[test]
#[ignore = "requires a running local coordinator in dry-run mode"]
fn coordinator_dry_run() -> Result<()> {
    let endpoint = env::var("MINI_QUAL_DRY_RUN_ENDPOINT")?;
    let run_id: Uuid = env::var("MINI_QUAL_DRY_RUN_ID")?.parse()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()?;
    let health: Value = runtime.block_on(async {
        client
            .get(format!("{endpoint}/healthz"))
            .send()
            .await?
            .json()
            .await
    })?;
    ensure!(
        health["mode"] == "dry_run",
        "expected a dry-run coordinator"
    );
    ensure!(health["run_id"] == run_id.to_string(), "run ID mismatch");

    let stop = Arc::new(AtomicBool::new(false));
    let mismatched = Worker::register(
        &endpoint,
        Uuid::new_v4(),
        "dry-run-wrong-id",
        None,
        Arc::clone(&stop),
    );
    ensure!(
        matches!(mismatched, Err(error) if format!("{error:#}").contains("run_mismatch")),
        "a registration with the wrong run ID must fail"
    );
    let worker = Worker::register(
        &endpoint,
        run_id,
        "dry-run-success",
        None,
        Arc::clone(&stop),
    )?;
    let timing = worker.timing();
    ensure!(
        timing.heartbeat_timeout_ms <= 5000,
        "start the dry-run coordinator with a short heartbeat interval and timeout"
    );
    let succeeded_id = worker.instance_id();
    thread::sleep(Duration::from_millis(timing.heartbeat_timeout_ms + 100));
    ensure!(
        !stop.load(Ordering::Acquire),
        "heartbeat did not preserve the lease"
    );
    worker.check_active()?;
    worker.complete(Outcome::Succeeded, "simulated data flushed")?;

    let stop = Arc::new(AtomicBool::new(false));
    let worker = Worker::register(
        &endpoint,
        run_id,
        "dry-run-failure",
        None,
        Arc::clone(&stop),
    )?;
    let failed_id = worker.instance_id();
    stop.store(true, Ordering::Release);
    worker.check_active()?;
    worker.complete(Outcome::Failed, "simulated interruption")?;

    let stop = Arc::new(AtomicBool::new(false));
    let worker = Worker::register(
        &endpoint,
        run_id,
        "dry-run-expiry",
        Some(750),
        Arc::clone(&stop),
    )?;
    let expiration_limit = Instant::now() + Duration::from_secs(3);
    while !stop.load(Ordering::Acquire) && Instant::now() < expiration_limit {
        thread::sleep(Duration::from_millis(20));
    }
    ensure!(
        stop.load(Ordering::Acquire),
        "task expiry did not cancel business work"
    );
    ensure!(worker.check_active().is_err(), "expired task still active");
    ensure!(
        worker.complete(Outcome::Succeeded, "too late").is_err(),
        "expired task accepted completion"
    );

    let status: Value = runtime.block_on(async {
        client
            .get(format!("{endpoint}/v1/runs/{run_id}"))
            .send()
            .await?
            .json()
            .await
    })?;
    let workers = status["workers"]
        .as_array()
        .context("missing coordinator workers")?;
    for (instance_id, outcome) in [(succeeded_id, "succeeded"), (failed_id, "failed")] {
        ensure!(
            workers
                .iter()
                .any(|worker| worker["instance_id"] == instance_id.to_string()
                    && worker["state"] == outcome),
            "worker {instance_id} missing expected outcome {outcome}"
        );
    }
    Ok(())
}
