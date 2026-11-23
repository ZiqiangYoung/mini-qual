use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration, Instant};

use uuid::Uuid;

use crate::telemetry::{Mode, Telemetry, TrialInfo};

use super::{TemperatureSource, read_samples};

#[test]
fn extracts_case_temperature_and_asks_for_a_pointer_for_ambiguous_responses() {
    let source = TemperatureSource::new("http://127.0.0.1", None, Duration::from_secs(60)).unwrap();
    assert_eq!(source.parse_temperature("76.5").unwrap(), 76.5);
    assert_eq!(
        source
            .parse_temperature(r#"{"temperature_c":76.5}"#)
            .unwrap(),
        76.5
    );
    assert_eq!(
        source
            .parse_temperature(r#"{"data":{"case_temperature_c":76.75,"temperature":60}}"#)
            .unwrap(),
        76.75
    );
    assert!(
        source
            .parse_temperature(r#"[{"temperature":76.5},{"temperature":58}]"#)
            .is_err()
    );
}

#[test]
#[ignore = "requires permission to bind a local TCP listener"]
fn records_raw_temperature_from_http_get_and_closes_the_sampler() {
    let response = r#"{"data":{"value_c":76.75},"extra":true}"#;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!(
        "http://{}/devices/036JNGZES9000051/temperature",
        listener.local_addr().unwrap()
    );
    let thread = thread::spawn(move || {
        let (mut socket, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(&socket);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut request = line.split_whitespace();
        assert_eq!(request.next(), Some("GET"));
        assert_eq!(
            request.next(),
            Some("/devices/036JNGZES9000051/temperature")
        );
        while !line.trim().is_empty() {
            line.clear();
            reader.read_line(&mut line).unwrap();
        }
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
            response.len()
        )
        .unwrap();
    });
    let folder = std::env::temp_dir().join(format!("mini-qual-temperature-{}", Uuid::new_v4()));
    fs::create_dir(&folder).unwrap();
    let started = Instant::now();
    let telemetry = Telemetry::new(TrialInfo {
        mode: Mode::Pretest,
        serial: "temperature-fixture".into(),
        device_path: "/dev/mock".into(),
        log_dir: folder.clone(),
        started,
        total_wear_rounds: None,
        total_write_budget_bytes: None,
    });
    let monitor = TemperatureSource::new(
        &endpoint,
        Some("/data/value_c".into()),
        Duration::from_secs(60),
    )
    .unwrap()
    .start(&folder, started, false, telemetry.clone())
    .unwrap();
    monitor.finish().unwrap();
    thread.join().unwrap();
    let samples = read_samples(&folder).unwrap();
    let content = fs::read_to_string(folder.join("temperature.jsonl")).unwrap();
    fs::remove_dir_all(folder).unwrap();
    let record: serde_json::Value = serde_json::from_str(&content).unwrap();
    assert_eq!(record["raw_json_body"], response);
    assert_eq!(record["http_status"], 200);
    assert_eq!(samples.len(), 1);
    assert_eq!(samples[0].measured_case_temperature_c, 76.75);
    let latest = telemetry.snapshot().unwrap().temperature.latest.unwrap();
    assert_eq!(latest.value, Ok(76.75));
    assert_eq!(latest.query_time, record["query_time"].as_str().unwrap());
}
