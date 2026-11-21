use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use mini_qual::thermal;
use mini_qual::thermal::Sample;
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::run_log;

#[cfg(test)]
#[path = "../tests/temperature/mod.rs"]
mod tests;

pub const DEFAULT_URL: &str = "http://127.0.0.1:30030/devices/036JNGZES9000051/temperature";
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

pub struct TemperatureMonitor {
    stop: Sender<()>,
    thread: Option<JoinHandle<Result<()>>>,
}

#[derive(Deserialize, Serialize)]
struct Measurement {
    query_time: String,
    completed_time: String,
    elapsed_seconds: f64,
    http_status: Option<u16>,
    raw_json_body: Option<String>,
    measured_case_temperature_c: Option<f64>,
    error: Option<String>,
}

pub struct TemperatureSource {
    url: Url,
    pointer: Option<String>,
    interval: Duration,
}

impl TemperatureSource {
    pub fn new(url: &str, pointer: Option<String>, interval: Duration) -> Result<Self> {
        let url = Url::parse(url).context("invalid temperature endpoint")?;
        ensure!(
            url.scheme() == "http" && url.has_host(),
            "temperature endpoint must be an HTTP URL"
        );
        ensure!(
            !interval.is_zero(),
            "temperature sample interval must be positive"
        );
        if let Some(pointer) = &pointer {
            ensure!(
                pointer.is_empty() || pointer.starts_with('/'),
                "temperature JSON pointer must be empty or begin with '/'"
            );
        }
        Ok(Self {
            url,
            pointer,
            interval,
        })
    }

    pub fn start(
        self,
        log_dir: &Path,
        started: Instant,
        print_each: bool,
    ) -> Result<TemperatureMonitor> {
        let file = File::create_new(log_dir.join("temperature.jsonl"))?;
        let client = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        let (stop, receiver) = mpsc::channel();
        let (first_attempt, first_attempt_ready) = mpsc::channel();
        let thread = thread::Builder::new()
            .name("case-temperature".to_string())
            .spawn(move || {
                let mut file = file;
                let mut first_attempt = Some(first_attempt);
                loop {
                    let query_started = Instant::now();
                    let query_time = run_log::utc_now();
                    let (http_status, raw_json_body, error) = runtime.block_on(self.query(&client));
                    let elapsed_seconds = started.elapsed().as_secs_f64();
                    let completed_time = run_log::utc_now();
                    let parsed = match (&raw_json_body, error) {
                        (_, Some(error)) => Err(error),
                        (Some(body), None) => self
                            .parse_temperature(body)
                            .map_err(|error| format!("{error:#}")),
                        (None, None) => Err("response body missing".to_string()),
                    };
                    if print_each {
                        match &parsed {
                            Ok(value) => eprintln!("[{query_time}] SSD 外壳温度 {value:.2}°C"),
                            Err(error) => eprintln!("[{query_time}] SSD 外壳温度查询失败 {error}"),
                        }
                    }
                    let measurement = Measurement {
                        query_time,
                        completed_time,
                        elapsed_seconds,
                        http_status,
                        raw_json_body,
                        measured_case_temperature_c: parsed.as_ref().ok().copied(),
                        error: parsed.err(),
                    };
                    run_log::write_json_line(&mut file, &measurement)?;
                    if let Some(first_attempt) = first_attempt.take() {
                        let _ = first_attempt.send(());
                    }
                    let remaining = self.interval.saturating_sub(query_started.elapsed());
                    match receiver.recv_timeout(remaining) {
                        Err(RecvTimeoutError::Timeout) => (),
                        _ => return file.sync_all().context("syncing the temperature timeline"),
                    }
                }
            })?;
        let monitor = TemperatureMonitor {
            stop,
            thread: Some(thread),
        };
        if first_attempt_ready.recv().is_err() {
            monitor.finish()?;
            bail!("temperature monitor stopped before its first sample");
        }
        Ok(monitor)
    }

    async fn query(&self, client: &Client) -> (Option<u16>, Option<String>, Option<String>) {
        let mut response = match client.get(self.url.clone()).send().await {
            Ok(response) => response,
            Err(error) => return (None, None, Some(error.to_string())),
        };
        let status = response.status();
        let mut body = Vec::new();
        loop {
            match response.chunk().await {
                Ok(Some(chunk)) if chunk.len() <= MAX_RESPONSE_BYTES.saturating_sub(body.len()) => {
                    body.extend_from_slice(&chunk);
                }
                Ok(Some(_)) => {
                    return (
                        Some(status.as_u16()),
                        None,
                        Some("response exceeds 64 KiB".into()),
                    );
                }
                Ok(None) => break,
                Err(error) => return (Some(status.as_u16()), None, Some(error.to_string())),
            }
        }
        match String::from_utf8(body) {
            Ok(body) => (
                Some(status.as_u16()),
                Some(body),
                (!status.is_success()).then(|| format!("HTTP {}", status.as_u16())),
            ),
            Err(error) => (Some(status.as_u16()), None, Some(error.to_string())),
        }
    }

    fn parse_temperature(&self, body: &str) -> Result<f64> {
        let value: Value = serde_json::from_str(body).context("invalid temperature JSON")?;
        if let Some(pointer) = &self.pointer {
            let field = value
                .pointer(pointer)
                .context("temperature JSON pointer is missing")?;
            return thermal::temperature(
                numeric(field).context("temperature field is not numeric")?,
            );
        }
        if let Some(number) = numeric(&value) {
            return thermal::temperature(number);
        }
        for keys in [
            &[
                "case_temperature_c",
                "shell_temperature_c",
                "caseTemperatureC",
                "shellTemperatureC",
                "case_temperature",
                "shell_temperature",
                "caseTemperature",
                "shellTemperature",
            ][..],
            &["temperature_c", "temperatureC", "temperature"][..],
        ] {
            let mut matches = Vec::new();
            find_temperatures(&value, keys, &mut matches);
            match matches.as_slice() {
                [number] => return thermal::temperature(*number),
                [] => (),
                _ => bail!("ambiguous case temperature JSON; set --temperature-pointer"),
            }
        }
        bail!("no recognizable case temperature field; set --temperature-pointer")
    }
}

fn numeric(value: &Value) -> Option<f64> {
    value.as_f64().or_else(|| value.as_str()?.parse().ok())
}

fn find_temperatures(value: &Value, keys: &[&str], matches: &mut Vec<f64>) {
    match value {
        Value::Object(fields) => {
            for (name, field) in fields {
                if keys.contains(&name.as_str()) {
                    if let Some(number) =
                        numeric(field).or_else(|| field.get("celsius").and_then(numeric))
                    {
                        matches.push(number);
                    }
                }
                find_temperatures(field, keys, matches);
            }
        }
        Value::Array(fields) => {
            for field in fields {
                find_temperatures(field, keys, matches);
            }
        }
        _ => (),
    }
}

impl TemperatureMonitor {
    pub fn finish(mut self) -> Result<()> {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("temperature thread panicked"))??;
        }
        Ok(())
    }
}

impl Drop for TemperatureMonitor {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub fn read_samples(log_dir: &Path) -> Result<Vec<Sample>> {
    let file = File::open(log_dir.join("temperature.jsonl"))?;
    let mut samples = Vec::new();
    for line in BufReader::new(file).lines() {
        let entry: Measurement =
            serde_json::from_str(&line?).context("reading temperature timeline")?;
        if let Some(measured_case_temperature_c) = entry.measured_case_temperature_c {
            samples.push(Sample {
                elapsed_seconds: entry.elapsed_seconds,
                measured_case_temperature_c,
            });
        }
    }
    Ok(samples)
}
