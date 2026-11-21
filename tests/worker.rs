use std::env;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use mini_qual::worker::{Outcome, Worker};
use reqwest::Client;
use serde_json::Value;
use tokio::runtime::Runtime;
use uuid::Uuid;

#[test]
fn worker_rejects_invalid_registration_inputs() {
    let run_id = Uuid::new_v4();
    let stop = Arc::new(AtomicBool::new(false));
    for (endpoint, name) in [
        ("https://127.0.0.1:17653", "test"),
        ("http://127.0.0.1:17653", ""),
    ] {
        assert!(Worker::register(endpoint, run_id, name, Arc::clone(&stop)).is_err());
    }
}

struct DryRunCoordinator {
    endpoint: String,
    run_id: Uuid,
    runtime: Runtime,
    client: Client,
}

fn connect_dry_run_coordinator() -> Result<DryRunCoordinator> {
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
    Ok(DryRunCoordinator {
        endpoint,
        run_id,
        runtime,
        client,
    })
}

#[test]
#[ignore = "requires a running local coordinator in dry-run mode with no task deadline"]
fn coordinator_dry_run() -> Result<()> {
    let DryRunCoordinator {
        endpoint,
        run_id,
        runtime,
        client,
    } = connect_dry_run_coordinator()?;

    let stop = Arc::new(AtomicBool::new(false));
    let mismatched = Worker::register(
        &endpoint,
        Uuid::new_v4(),
        "dry-run-wrong-id",
        Arc::clone(&stop),
    );
    ensure!(
        matches!(mismatched, Err(error) if format!("{error:#}").contains("run_mismatch")),
        "a registration with the wrong run ID must fail"
    );
    let worker = Worker::register(&endpoint, run_id, "dry-run-success", Arc::clone(&stop))?;
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
    let worker = Worker::register(&endpoint, run_id, "dry-run-failure", Arc::clone(&stop))?;
    let failed_id = worker.instance_id();
    stop.store(true, Ordering::Release);
    worker.check_active()?;
    worker.complete(Outcome::Failed, "simulated interruption")?;

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

#[test]
#[ignore = "requires a running local coordinator in dry-run mode with a finite task deadline"]
fn coordinator_rejects_finite_task_deadline() -> Result<()> {
    let coordinator = connect_dry_run_coordinator()?;
    let error = Worker::register(
        &coordinator.endpoint,
        coordinator.run_id,
        "dry-run-finite-deadline",
        Arc::new(AtomicBool::new(false)),
    )
    .err()
    .context("worker accepted a finite coordinator task deadline")?;
    ensure!(
        error
            .to_string()
            .contains("coordinator reports a finite worker task timeout"),
        "unexpected registration error: {error:#}"
    );
    Ok(())
}
