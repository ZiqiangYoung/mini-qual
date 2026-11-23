use std::fs::File;
use std::io::{self, Read as _, Write as _};
use std::os::fd::{AsRawFd, FromRawFd};
use std::process::{Command, Stdio};

use ratatui::backend::TestBackend;

use super::{
    Dashboard, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, Terminal, format_bytes, format_rate,
    is_interrupt, render,
};
use crate::telemetry::{IoProgress, Mode, Phase, Snapshot, StageKind, Telemetry, TrialInfo};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const MB: u64 = 1_000_000;
const TEST_SCENARIO: &str = "MINI_QUAL_DASHBOARD_TEST_SCENARIO";
const ENTER_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049h";
const LEAVE_ALTERNATE_SCREEN: &[u8] = b"\x1b[?1049l";

fn trial_info(mode: Mode) -> TrialInfo {
    TrialInfo {
        mode,
        serial: "TEST_SN".into(),
        device_path: "/dev/test-disk".into(),
        log_dir: "/tmp/test-qual-logs".into(),
        started: Instant::now(),
        total_wear_rounds: (mode == Mode::Run).then_some(5),
        total_write_budget_bytes: (mode == Mode::Run).then_some(u128::from(3 * 1_000 * MB)),
    }
}

fn screen(width: u16, height: u16, snapshot: &Snapshot, stopping: bool) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| render(frame, snapshot, stopping))
        .unwrap();
    format!("{:?}", terminal.backend().buffer())
}

#[test]
fn capacity_and_bandwidth_use_decimal_megabytes_and_gigabytes() {
    assert_eq!(format_bytes(0), "0 MB");
    assert_eq!(format_bytes(512), "0.000512 MB");
    assert_eq!(format_bytes(1_000_000), "1.0 MB");
    assert_eq!(format_bytes(1_000_000_000), "1.0 GB");
    assert_eq!(format_rate(Some(480_000_000.0)), "480.0 MB/s");
    assert_eq!(format_rate(Some(1_500_000_000.0)), "1.5 GB/s");
}

#[test]
fn full_and_compact_layouts_show_work_temperature_and_bandwidth() {
    let telemetry = Telemetry::new(trial_info(Mode::Run));
    telemetry.start_round(Phase::Wear, 2, 512 * MB, 512 * MB);
    telemetry.round_completed(1);
    let completed = IoProgress {
        attempted_bytes: 512 * MB,
        successful_bytes: 512 * MB,
        bytes_per_second: Some(128.0 * MB as f64),
        ..IoProgress::default()
    };
    for kind in [StageKind::Trim, StageKind::Write] {
        telemetry.stage_started(kind);
        telemetry.stage_completed(kind, Duration::from_secs(4), Some(completed));
    }
    telemetry.stage_started(StageKind::Flush);
    telemetry.stage_completed(StageKind::Flush, Duration::from_secs(1), None);
    telemetry.stage_started(StageKind::Read);
    telemetry.io_progress(IoProgress {
        attempted_bytes: 256 * MB,
        successful_bytes: 224 * MB,
        errors: 1,
        bytes_per_second: Some(32.0 * MB as f64),
        attempted_bytes_per_second: Some(64.0 * MB as f64),
    });
    telemetry.temperature("2026-11-21T00:00:00Z", 0.0, &Ok(42.5));
    telemetry.temperature("2026-11-21T00:00:05Z", 5.0, &Ok(42.75));

    let snapshot = telemetry.snapshot().unwrap();
    for (width, height) in [(120, 30), (80, 24)] {
        let contents = screen(width, height, &snapshot, false);
        for expected in [
            "TEST_SN",
            "/dev/test-disk",
            "第 2 轮",
            "已完成 1 / 5 轮",
            "累计写入 512.0 MB / 3.0 GB 总预算",
            "TRIM",
            "写入",
            "flush",
            "读取",
            "错误定位",
            "待执行",
            "执行中",
            "完成",
            "50.0%",
            "256.0 MB / 512.0 MB",
            "32.0 MB/s",
            "预计剩余 00:00:04",
            "失败 32.0 MB",
            "42.75 °C",
        ] {
            assert!(
                contents.contains(expected),
                "{expected:?} is missing from {contents}"
            );
        }
    }
}

#[test]
fn shows_tail_round_and_retention_handoff_on_narrow_terminals() {
    let telemetry = Telemetry::new(trial_info(Mode::Run));
    telemetry.start_round(Phase::Wear, 5, 512 * MB, MB);
    telemetry.stage_started(StageKind::Write);
    telemetry.io_progress(IoProgress {
        attempted_bytes: MB / 2,
        attempted_bytes_per_second: Some(300_000.0),
        ..IoProgress::default()
    });
    let tail = screen(68, 23, &telemetry.snapshot().unwrap(), false);
    assert!(tail.contains("0.5 MB / 1.0 MB"), "{tail}");
    assert!(tail.contains("预计剩余 00:00:02"), "{tail}");

    telemetry.round_completed(5);
    telemetry.start_round(Phase::Retention, 6, 512 * MB, 512 * MB);
    telemetry.stage_started(StageKind::Handoff);
    telemetry.message("准备上报断电协调器");
    let retention = telemetry.snapshot().unwrap();
    let contents = screen(60, 18, &retention, true);
    assert!(contents.contains("retention 准备"), "{contents}");
    assert!(contents.contains("断电交接 · 执行中"), "{contents}");
    assert!(contents.contains("正在停止"), "{contents}");
    assert!(!contents.contains("错误定位"), "{contents}");
    assert!(!contents.contains("预计剩余"), "{contents}");

    let tiny = screen(33, 8, &retention, true);
    assert!(tiny.contains("断电交接 · 执行中"), "{tiny}");
    assert!(tiny.contains("正在停止"), "{tiny}");
}

#[test]
fn running_task_eta_requires_a_fresh_positive_processing_rate() {
    let telemetry = Telemetry::new(trial_info(Mode::Pretest));
    telemetry.start_round(Phase::Wear, 1, 2 * MB, 2 * MB);
    telemetry.stage_started(StageKind::Trim);
    let contents = screen(80, 12, &telemetry.snapshot().unwrap(), false);
    assert!(contents.contains("预计剩余 —"), "{contents}");

    for rate in [Some(0.0), Some(f64::NAN), Some(100_000.0)] {
        telemetry.io_progress(IoProgress {
            attempted_bytes: MB,
            attempted_bytes_per_second: rate,
            ..IoProgress::default()
        });
        let mut snapshot = telemetry.snapshot().unwrap();
        if rate == Some(100_000.0) {
            let contents = screen(80, 12, &snapshot, false);
            assert!(contents.contains("预计剩余 00:00:10"), "{contents}");
            snapshot.tasks[0].progress_at = Some(Instant::now() - Duration::from_secs(4));
        }
        let contents = screen(80, 12, &snapshot, false);
        assert!(contents.contains("预计剩余 —"), "{contents}");
    }
}

#[test]
fn bandwidth_chart_shows_recent_samples_in_wide_and_compact_layouts() {
    let telemetry = Telemetry::new(trial_info(Mode::Pretest));
    telemetry.start_round(Phase::Wear, 1, 10_000 * MB, 10_000 * MB);
    telemetry.stage_started(StageKind::Read);
    for rate in [9_500.0, 2_500.0, 1_500.0, 0.0] {
        telemetry.io_progress(IoProgress {
            attempted_bytes: 4_000 * MB,
            successful_bytes: 3_000 * MB,
            bytes_per_second: Some(rate * MB as f64),
            attempted_bytes_per_second: Some(1_000.0 * MB as f64),
            ..IoProgress::default()
        });
    }
    let mut snapshot = telemetry.snapshot().unwrap();
    let now = Instant::now();
    for (sample, age) in snapshot.tasks[3]
        .recent_bandwidth
        .iter_mut()
        .zip([61, 8, 6, 2])
    {
        sample.observed_at = now - Duration::from_secs(age);
    }

    for (width, height) in [(120, 30), (100, 25), (80, 24)] {
        let contents = screen(width, height, &snapshot, false);
        for expected in ["−10 s", "现在", "2.5 GB/s", "预计剩余 00:00:06", "壳温"] {
            assert!(
                contents.contains(expected),
                "{expected} missing from {contents}"
            );
        }
        assert!(!contents.contains("9.5 GB/s"), "{contents}");
        assert!(
            contents.contains(|ch| matches!(ch, '\u{2801}'..='\u{28ff}')),
            "{contents}"
        );
    }

    let short = screen(80, 12, &snapshot, false);
    assert!(short.contains("预计剩余 00:00:06"), "{short}");
    assert!(short.contains("近期 0 MB/s"), "{short}");
    assert!(!short.contains("−10 s"), "{short}");
}

#[test]
fn bandwidth_history_keeps_measured_zero_and_skips_absent_and_final_rates() {
    let telemetry = Telemetry::new(trial_info(Mode::Pretest));
    telemetry.start_round(Phase::Wear, 1, MB, MB);
    telemetry.stage_started(StageKind::Trim);
    for rate in [None, Some(-1.0), Some(f64::NAN), Some(0.0)] {
        telemetry.io_progress(IoProgress {
            bytes_per_second: rate,
            ..IoProgress::default()
        });
    }
    let contents = screen(80, 24, &telemetry.snapshot().unwrap(), false);
    assert!(contents.contains("实时带宽"), "{contents}");
    assert!(contents.contains("现在"), "{contents}");

    telemetry.stage_completed(
        StageKind::Trim,
        Duration::from_secs(1),
        Some(IoProgress {
            bytes_per_second: Some(MB as f64),
            ..IoProgress::default()
        }),
    );
    telemetry.stage_started(StageKind::Write);
    let snapshot = telemetry.snapshot().unwrap();
    let history = &snapshot.tasks[0].recent_bandwidth;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].bytes_per_second, 0.0);
    assert!(snapshot.tasks[1].recent_bandwidth.is_empty());
    let contents = screen(80, 24, &snapshot, false);
    assert!(!contents.contains("实时带宽"), "{contents}");
}

#[test]
fn only_ctrl_c_requests_interruption() {
    assert!(is_interrupt(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL
    )));
    assert!(!is_interrupt(KeyEvent::new(
        KeyCode::Char('q'),
        KeyModifiers::NONE
    )));
    assert!(!is_interrupt(KeyEvent::new(
        KeyCode::Char('c'),
        KeyModifiers::NONE
    )));
    assert!(!is_interrupt(KeyEvent::new_with_kind(
        KeyCode::Char('c'),
        KeyModifiers::CONTROL,
        KeyEventKind::Release,
    )));
}

#[test]
#[ignore = "invoked by the pseudo-terminal tests"]
fn dashboard_child() {
    let scenario = std::env::var(TEST_SCENARIO).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let mut dashboard = Dashboard::start(
        scenario == "disabled",
        trial_info(Mode::Pretest),
        Arc::clone(&stop),
    );
    if matches!(scenario.as_str(), "plain" | "disabled") {
        assert!(!dashboard.telemetry().is_active());
    } else {
        assert!(dashboard.telemetry().is_active());
        assert_ne!(scenario, "panic", "simulated trial failure");
        let deadline = Instant::now() + Duration::from_secs(4);
        while !stop.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            stop.load(Ordering::Acquire),
            "Ctrl+C did not stop the dashboard fixture"
        );
    }
    dashboard.finish();
    println!("dashboard fixture finished");
}

#[test]
fn redirected_output_stays_in_text_mode() -> io::Result<()> {
    let output = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "dashboard::tests::dashboard_child",
            "--nocapture",
        ])
        .env(TEST_SCENARIO, "plain")
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(find_bytes(&output.stdout, b"dashboard fixture finished").is_some());
    assert!(find_bytes(&output.stdout, ENTER_ALTERNATE_SCREEN).is_none());
    Ok(())
}

#[test]
#[ignore = "requires access to a pseudo-terminal"]
fn pseudo_terminal_restores_on_interrupt_and_panic() -> io::Result<()> {
    let (success, output) = run_pseudo_terminal("interrupt")?;
    assert!(success, "{}", String::from_utf8_lossy(&output));
    let leave = find_bytes(&output, LEAVE_ALTERNATE_SCREEN).expect("terminal restored");
    let finish = find_bytes(&output, b"dashboard fixture finished").expect("fixture finished");
    assert!(leave < finish);

    let (success, output) = run_pseudo_terminal("panic")?;
    assert!(!success, "{}", String::from_utf8_lossy(&output));
    let enter = find_bytes(&output, ENTER_ALTERNATE_SCREEN).expect("terminal entered");
    let leave = find_bytes(&output, LEAVE_ALTERNATE_SCREEN).expect("terminal restored");
    assert!(enter < leave);
    let (success, output) = run_pseudo_terminal("disabled")?;
    assert!(success, "{}", String::from_utf8_lossy(&output));
    assert!(find_bytes(&output, ENTER_ALTERNATE_SCREEN).is_none());
    Ok(())
}

fn run_pseudo_terminal(scenario: &str) -> io::Result<(bool, Vec<u8>)> {
    let mut master = -1;
    let mut slave = -1;
    let size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: The output descriptors and window size are valid for the call.
    // Null pointers select the default terminal settings and skip the name.
    let opened = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &size,
        )
    };
    if opened != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded and returned these two owned descriptors.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    let mut child = Command::new(std::env::current_exe()?)
        .args([
            "--ignored",
            "--exact",
            "dashboard::tests::dashboard_child",
            "--nocapture",
        ])
        .env(TEST_SCENARIO, scenario)
        .env("TERM", "xterm-256color")
        .stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave))
        .spawn()?;
    let mut bytes = Vec::new();
    let mut chunk = [0; 4096];
    let mut sent_q = None;
    let mut sent_interrupt = false;
    let deadline = Instant::now() + Duration::from_secs(6);
    while Instant::now() < deadline {
        let mut master_poll = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: The pollfd points to one initialized record and master remains open.
        let ready = unsafe { libc::poll(&mut master_poll, 1, 50) };
        if ready < 0 {
            return Err(io::Error::last_os_error());
        }
        if ready > 0 {
            match master.read(&mut chunk) {
                Ok(0) => break,
                Ok(length) => bytes.extend_from_slice(&chunk[..length]),
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) => return Err(error),
            }
        }
        if scenario == "interrupt" && find_bytes(&bytes, ENTER_ALTERNATE_SCREEN).is_some() {
            match sent_q {
                None => {
                    master.write_all(b"q")?;
                    sent_q = Some(Instant::now());
                }
                Some(sent) if !sent_interrupt && sent.elapsed() >= Duration::from_millis(250) => {
                    master.write_all(b"\x03")?;
                    sent_interrupt = true;
                }
                _ => (),
            }
        }
    }
    if Instant::now() >= deadline {
        child.kill()?;
        child.wait()?;
        panic!("dashboard fixture timed out");
    }
    let status = child.wait()?;
    assert!(
        scenario != "interrupt" || sent_interrupt,
        "q ended the dashboard fixture"
    );
    Ok((status.success(), bytes))
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}
