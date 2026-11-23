use std::io::{self, IsTerminal, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Axis, Block, Cell, Chart, Dataset, Gauge, GraphType, Paragraph, Row, Table, Wrap,
};
use ratatui::{Frame, Terminal};

use crate::telemetry::{Mode, Phase, Snapshot, StageKind, StageStatus, Task, Telemetry, TrialInfo};

const FRAME_INTERVAL: Duration = Duration::from_millis(250);
const INPUT_INTERVAL: Duration = Duration::from_millis(50);
const ACCENT: Color = Color::LightCyan;

pub struct Dashboard {
    telemetry: Telemetry,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Dashboard {
    pub fn start(no_tui: bool, info: TrialInfo, stop: Arc<AtomicBool>) -> Self {
        let mut dashboard = Self {
            telemetry: Telemetry::default(),
            shutdown: Arc::new(AtomicBool::new(false)),
            thread: None,
        };
        if no_tui
            || !io::stdin().is_terminal()
            || !io::stdout().is_terminal()
            || !io::stderr().is_terminal()
            || std::env::var_os("TERM").is_some_and(|term| term == "dumb")
        {
            return dashboard;
        }

        let telemetry = Telemetry::new(info);
        let terminal = match ratatui::try_init() {
            Ok(terminal) => terminal,
            Err(error) => {
                ratatui::restore();
                eprintln!("无法打开全屏界面，使用文本输出。{error}");
                return dashboard;
            }
        };
        let mut session = TerminalSession {
            terminal,
            telemetry: telemetry.clone(),
        };
        let shutdown = Arc::clone(&dashboard.shutdown);
        match thread::Builder::new()
            .name("trial-dashboard".into())
            .spawn(move || {
                let result = session.display(&shutdown, &stop);
                drop(session);
                if let Err(error) = result {
                    eprintln!("全屏界面已退出，试验继续并使用文本输出。{error}");
                }
            }) {
            Ok(thread) => {
                dashboard.telemetry = telemetry;
                dashboard.thread = Some(thread);
            }
            Err(error) => eprintln!("无法启动全屏界面，使用文本输出。{error}"),
        }
        dashboard
    }

    pub fn telemetry(&self) -> &Telemetry {
        &self.telemetry
    }

    pub fn finish(&mut self) {
        self.shutdown.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            if thread.join().is_err() {
                eprintln!("全屏界面异常退出，终端已恢复。");
            }
        }
    }
}

impl Drop for Dashboard {
    fn drop(&mut self) {
        self.finish();
    }
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
    telemetry: Telemetry,
}

impl TerminalSession {
    fn display(&mut self, shutdown: &AtomicBool, stop: &AtomicBool) -> io::Result<()> {
        let mut next_frame = Instant::now();
        while !shutdown.load(Ordering::Acquire) {
            let now = Instant::now();
            if now >= next_frame {
                let Some(snapshot) = self.telemetry.snapshot() else {
                    break;
                };
                let stopping = stop.load(Ordering::Acquire);
                self.terminal
                    .draw(|frame| render(frame, &snapshot, stopping))?;
                next_frame = now + FRAME_INTERVAL;
            }
            let wait = INPUT_INTERVAL.min(next_frame.saturating_duration_since(Instant::now()));
            if event::poll(wait)? {
                match event::read()? {
                    Event::Key(key) if is_interrupt(key) => {
                        stop.store(true, Ordering::Release);
                        next_frame = Instant::now();
                    }
                    Event::Resize(_, _) => next_frame = Instant::now(),
                    _ => (),
                }
            }
        }
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        ratatui::restore();
        self.telemetry.disable();
    }
}

fn is_interrupt(key: KeyEvent) -> bool {
    key.kind != KeyEventKind::Release
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('c' | 'C'))
}

fn render(frame: &mut Frame<'_>, view: &Snapshot, stopping: bool) {
    let area = frame.area();
    if area.width >= 100 && area.height >= 25 {
        let [overview, work, observations, footer] = Layout::vertical([
            Constraint::Length(5),
            Constraint::Min(8),
            Constraint::Length(if area.height >= 30 { 11 } else { 8 }),
            Constraint::Length(1),
        ])
        .areas(area);
        let [tasks, progress] =
            Layout::horizontal([Constraint::Percentage(53), Constraint::Percentage(47)])
                .areas(work);
        let [temperature, events] =
            Layout::horizontal([Constraint::Percentage(53), Constraint::Percentage(47)])
                .areas(observations);

        frame.render_widget(
            Paragraph::new(vec![
                overview_line(view),
                round_line(view),
                written_line(view),
            ])
            .block(panel(" 总览 ")),
            overview,
        );
        render_tasks(frame, tasks, view);
        render_progress(frame, progress, view, true);
        render_temperature(frame, temperature, view);
        render_events(frame, events, view);
        render_footer(frame, footer, view, stopping, true);
    } else {
        render_compact(frame, area, view, stopping);
    }
}

fn render_compact(frame: &mut Frame<'_>, area: Rect, view: &Snapshot, stopping: bool) {
    if area.height < 12 || area.width < 38 {
        let mut lines = vec![overview_line(view), round_line(view)];
        lines.extend(compact_progress(view));
        lines.push(temperature_line(view));
        lines.push(footer_line(view, stopping, false));
        frame.render_widget(Paragraph::new(lines), area);
        return;
    }

    let enough_for_details = area.height >= 19;
    let tasks_height = view.tasks.len().max(1) as u16;
    let [overview, work, footer] = Layout::vertical([
        Constraint::Length(if enough_for_details { 3 } else { 2 }),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    let [tasks, progress, temperature, events] = Layout::vertical([
        Constraint::Length(tasks_height),
        Constraint::Length(if enough_for_details { 4 } else { 2 }),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(work);

    frame.render_widget(
        Paragraph::new(vec![
            overview_line(view),
            round_line(view),
            written_line(view),
        ]),
        overview,
    );
    let rows = if view.tasks.is_empty() {
        vec![Line::from("等待轮次开始")]
    } else {
        view.tasks
            .iter()
            .map(|task| {
                Line::from(vec![
                    Span::styled(
                        format!("{}  {}", status_symbol(task.status), task_name(task.kind)),
                        status_style(task.status),
                    ),
                    Span::raw(format!(" · {}", status_label(task.status))),
                ])
            })
            .collect()
    };
    frame.render_widget(Paragraph::new(rows), tasks);
    render_progress(frame, progress, view, false);
    frame.render_widget(Paragraph::new(temperature_line(view)), temperature);
    if let Some((message, style)) = latest_event(view) {
        frame.render_widget(
            Paragraph::new(message)
                .style(style)
                .wrap(Wrap { trim: true }),
            events,
        );
    }
    render_footer(frame, footer, view, stopping, enough_for_details);
}

fn render_tasks(frame: &mut Frame<'_>, area: Rect, view: &Snapshot) {
    let rows: Vec<Row<'_>> = if view.tasks.is_empty() {
        vec![Row::new(["等待轮次开始", "", ""])]
    } else {
        view.tasks
            .iter()
            .map(|task| {
                let summary = match (task.status, task.progress) {
                    (StageStatus::Completed, Some(progress)) if progress.errors > 0 => {
                        format!("{} 个错误", progress.errors)
                    }
                    (StageStatus::Completed, Some(progress)) => {
                        format_rate(progress.bytes_per_second)
                    }
                    (StageStatus::Running, _) => task
                        .started
                        .map_or_else(String::new, |start| format_duration(start.elapsed())),
                    _ => task.duration.map_or_else(String::new, format_duration),
                };
                Row::new([
                    Cell::from(format!(
                        "{}  {}",
                        status_symbol(task.status),
                        task_name(task.kind)
                    )),
                    Cell::from(status_label(task.status)),
                    Cell::from(summary),
                ])
                .style(status_style(task.status))
            })
            .collect()
    };
    let heading = Row::new(["任务", "状态", "耗时 / 带宽 / 错误"])
        .style(Style::default().add_modifier(Modifier::BOLD));
    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Percentage(34),
                Constraint::Percentage(23),
                Constraint::Percentage(43),
            ],
        )
        .header(heading)
        .block(panel(" 当前轮次任务 ")),
        area,
    );
}

fn render_progress(frame: &mut Frame<'_>, area: Rect, view: &Snapshot, wide: bool) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    let mut content = area;
    if wide {
        let block = panel(" 进度与带宽 ");
        content = block.inner(area);
        frame.render_widget(block, area);
    }
    let mut lines = compact_progress(view);
    if wide {
        lines.push(Line::from(format!(
            "读取失败 {} · 定位错误 {}",
            view.read_errors, view.locate_errors
        )));
    }
    let task = current_task(&view.tasks);
    if let Some((target, completed)) = task
        .and_then(|task| {
            let target = task.target_bytes.filter(|target| *target > 0)?;
            task.progress
                .map(|progress| (target, progress.attempted_bytes))
        })
        .filter(|_| content.height >= 4)
    {
        let [gauge, metrics] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(content);
        let ratio = completed.min(target) as f64 / target as f64;
        frame.render_widget(
            Gauge::default()
                .ratio(ratio)
                .gauge_style(Style::default().fg(ACCENT))
                .label(format!("{:.1}%", ratio * 100.0)),
            gauge,
        );
        content = metrics;
    }
    frame.render_widget(Paragraph::new(lines), content);
}

fn compact_progress(view: &Snapshot) -> Vec<Line<'static>> {
    let Some(task) = current_task(&view.tasks) else {
        return vec![Line::from("等待任务开始")];
    };
    let elapsed = task
        .duration
        .or_else(|| task.started.map(|start| start.elapsed()));
    let title = format!("{} · {}", task_name(task.kind), status_label(task.status));
    let Some(progress) = task.progress else {
        let detail = match (task.status, task.target_bytes, elapsed) {
            (StageStatus::Running, Some(_), _) => " · 等待采样".to_owned(),
            (_, _, Some(duration)) => format!(" · {}", format_duration(duration)),
            _ => String::new(),
        };
        return vec![Line::styled(
            format!("{title}{detail}"),
            status_style(task.status),
        )];
    };

    let bytes = format_bytes(progress.attempted_bytes.into());
    let work = match task.target_bytes {
        Some(target) => format!("{bytes} / {}", format_bytes(target.into())),
        None => bytes,
    };
    let average = if task.status == StageStatus::Completed {
        progress.bytes_per_second
    } else {
        elapsed
            .filter(|duration| !duration.is_zero())
            .map(|duration| progress.successful_bytes as f64 / duration.as_secs_f64())
    };
    let bandwidth = if task.status == StageStatus::Completed {
        format!("平均 {}", format_rate(average))
    } else {
        format!(
            "近期 {} · 平均 {}",
            format_rate(progress.bytes_per_second),
            format_rate(average)
        )
    };
    let duration = elapsed.map_or_else(String::new, |duration| {
        format!(" · {}", format_duration(duration))
    });
    let mut lines = vec![
        Line::styled(format!("{title}  {work}"), status_style(task.status)),
        Line::from(format!("{bandwidth}{duration}")),
    ];
    if matches!(task.kind, StageKind::Read | StageKind::Locate) {
        let failed_bytes = progress
            .attempted_bytes
            .saturating_sub(progress.successful_bytes);
        lines.push(Line::from(format!(
            "成功 {} · 失败 {} · 错误 {}",
            format_bytes(progress.successful_bytes.into()),
            format_bytes(failed_bytes.into()),
            progress.errors
        )));
    }
    lines
}

fn current_task(tasks: &[Task]) -> Option<&Task> {
    tasks
        .iter()
        .find(|task| task.status == StageStatus::Running)
        .or_else(|| {
            tasks
                .iter()
                .rev()
                .find(|task| task.status != StageStatus::Pending)
        })
}

fn render_temperature(frame: &mut Frame<'_>, area: Rect, view: &Snapshot) {
    let block = panel(" 壳温 ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [latest, chart] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(inner);
    frame.render_widget(Paragraph::new(temperature_line(view)), latest);
    if chart.height < 4 || chart.width < 20 || view.temperature.recent.len() < 2 {
        return;
    }

    let start = view
        .temperature
        .recent
        .front()
        .map_or(0.0, |sample| sample.elapsed_seconds);
    let sample_points: Vec<_> = view
        .temperature
        .recent
        .iter()
        .filter_map(|sample| {
            let seconds = sample.elapsed_seconds - start;
            (seconds.is_finite() && sample.temperature_c.is_finite())
                .then_some((seconds, sample.temperature_c))
        })
        .collect();
    let Some((end, _)) = sample_points.last() else {
        return;
    };
    let (minimum, maximum) = sample_points
        .iter()
        .map(|(_, temperature)| *temperature)
        .fold(
            (f64::INFINITY, f64::NEG_INFINITY),
            |(minimum, maximum), value| (minimum.min(value), maximum.max(value)),
        );
    let lower = minimum.min(maximum - 0.5) - 0.25;
    let upper = maximum.max(minimum + 0.5) + 0.25;
    if sample_points.len() < 2 || *end <= 0.0 || lower >= upper {
        return;
    }
    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Line)
        .style(Style::default().fg(ACCENT))
        .data(&sample_points);
    let chart_widget = Chart::new(vec![dataset])
        .x_axis(
            Axis::default()
                .bounds([0.0, *end])
                .labels(vec![Span::raw("0 s"), Span::raw(format!("{end:.0} s"))]),
        )
        .y_axis(Axis::default().bounds([lower, upper]).labels(vec![
            Span::raw(format!("{lower:.1}°")),
            Span::raw(format!("{upper:.1}°")),
        ]));
    frame.render_widget(chart_widget, chart);
}

fn temperature_line(view: &Snapshot) -> Line<'static> {
    let Some(latest) = &view.temperature.latest else {
        return Line::from("壳温 · 等待采样");
    };
    match &latest.value {
        Ok(value) => Line::from(vec![
            Span::styled(format!("壳温 {value:.2} °C"), Style::default().fg(ACCENT)),
            Span::raw(format!(
                " · {} · {} 前",
                terminal_text(&latest.query_time),
                format_duration(latest.observed_at.elapsed())
            )),
        ]),
        Err(_) => {
            let previous = view.temperature.recent.back().map(|sample| {
                format!(
                    " · 最近有效 {:.2} °C（{}）",
                    sample.temperature_c,
                    terminal_text(&sample.query_time)
                )
            });
            Line::styled(
                format!("壳温查询失败{}", previous.unwrap_or_default()),
                Style::default().fg(Color::LightYellow),
            )
        }
    }
}

fn render_events(frame: &mut Frame<'_>, area: Rect, view: &Snapshot) {
    let mut lines = Vec::new();
    if let Some(message) = &view.message {
        lines.push(Line::from(terminal_text(message)));
    }
    if let Some(error) = &view.last_error {
        lines.push(Line::styled(
            terminal_text(error),
            Style::default().fg(Color::LightRed),
        ));
    }
    if let Some(latest) = &view.temperature.latest {
        if let Err(error) = &latest.value {
            lines.push(Line::styled(
                format!(
                    "壳温 {} · {}",
                    terminal_text(&latest.query_time),
                    terminal_text(error)
                ),
                Style::default().fg(Color::LightYellow),
            ));
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: true })
            .block(panel(" 事件 ")),
        area,
    );
}

fn latest_event(view: &Snapshot) -> Option<(String, Style)> {
    if let Some(error) = &view.last_error {
        return Some((terminal_text(error), Style::default().fg(Color::LightRed)));
    }
    if let Some(latest) = &view.temperature.latest {
        if let Err(error) = &latest.value {
            return Some((
                terminal_text(error),
                Style::default().fg(Color::LightYellow),
            ));
        }
    }
    view.message
        .as_deref()
        .map(|message| (terminal_text(message), Style::default()))
}

fn overview_line(view: &Snapshot) -> Line<'static> {
    let mode = match view.info.mode {
        Mode::Run => "正式运行",
        Mode::Pretest => "预测试",
    };
    let phase = match view.phase {
        Phase::Preparing => "准备中",
        Phase::Wear => "磨损",
        Phase::Retention => "retention 准备",
    };
    Line::from(vec![
        Span::styled(
            format!("{mode}  {phase}"),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(
            " · SN {} · {}",
            terminal_text(&view.info.serial),
            terminal_text(&view.info.device_path.to_string_lossy())
        )),
    ])
}

fn round_line(view: &Snapshot) -> Line<'static> {
    let round = match (view.phase, view.round) {
        (Phase::Wear, Some(round)) => format!("第 {round} 轮 · "),
        _ => String::new(),
    };
    let scope = match view.info.total_wear_rounds {
        Some(total) => format!(" / {total} 轮"),
        None => " 轮（容量估算至少需 2 轮）".to_owned(),
    };
    Line::from(format!(
        "{round}已完成 {}{scope} · 运行 {}",
        view.completed_rounds,
        format_duration(view.info.started.elapsed())
    ))
}

fn written_line(view: &Snapshot) -> Line<'static> {
    let budget = view
        .info
        .total_write_budget_bytes
        .map_or_else(String::new, |budget| {
            format!(" / {} 总预算", format_bytes(budget))
        });
    Line::from(format!(
        "累计写入 {}{budget}",
        format_bytes(view.cumulative_written_bytes)
    ))
}

fn render_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    view: &Snapshot,
    stopping: bool,
    show_path: bool,
) {
    frame.render_widget(Paragraph::new(footer_line(view, stopping, show_path)), area);
}

fn footer_line(view: &Snapshot, stopping: bool, show_path: bool) -> Line<'static> {
    let control = if stopping {
        Span::styled(
            "正在停止",
            Style::default()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("Ctrl+C 停止", Style::default().fg(ACCENT))
    };
    if show_path {
        Line::from(vec![
            control,
            Span::raw(format!(
                " · 日志 {}",
                terminal_text(&view.info.log_dir.to_string_lossy())
            )),
        ])
    } else {
        Line::from(control)
    }
}

fn panel(title: &'static str) -> Block<'static> {
    Block::bordered()
        .title(title)
        .border_style(Style::default().fg(Color::Gray))
}

fn task_name(kind: StageKind) -> &'static str {
    match kind {
        StageKind::Trim => "T TRIM",
        StageKind::Write => "W 写入",
        StageKind::Flush => "flush",
        StageKind::Read => "R 读取",
        StageKind::Locate => "错误定位",
        StageKind::Handoff => "断电交接",
    }
}

fn status_label(status: StageStatus) -> &'static str {
    match status {
        StageStatus::Pending => "待执行",
        StageStatus::Running => "执行中",
        StageStatus::Completed => "完成",
        StageStatus::Failed => "失败",
        StageStatus::Interrupted => "已中断",
    }
}

fn status_symbol(status: StageStatus) -> &'static str {
    match status {
        StageStatus::Pending => "·",
        StageStatus::Running => "▶",
        StageStatus::Completed => "✓",
        StageStatus::Failed => "×",
        StageStatus::Interrupted => "■",
    }
}

fn status_style(status: StageStatus) -> Style {
    let color = match status {
        StageStatus::Pending => Color::Gray,
        StageStatus::Running => ACCENT,
        StageStatus::Completed => Color::LightGreen,
        StageStatus::Failed => Color::LightRed,
        StageStatus::Interrupted => Color::LightYellow,
    };
    Style::default().fg(color)
}

fn format_bytes(bytes: u128) -> String {
    format_scaled_bytes(bytes as f64)
}

fn format_rate(rate: Option<f64>) -> String {
    rate.filter(|rate| rate.is_finite() && *rate >= 0.0)
        .map_or_else(
            || "—".to_owned(),
            |rate| format!("{}/s", format_scaled_bytes(rate)),
        )
}

fn format_scaled_bytes(bytes: f64) -> String {
    const BYTES_PER_MB: f64 = 1_000_000.0;
    const BYTES_PER_GB: f64 = 1_000_000_000.0;
    if bytes >= BYTES_PER_GB {
        return format!("{:.1} GB", bytes / BYTES_PER_GB);
    }
    let megabytes = bytes / BYTES_PER_MB;
    if megabytes >= 1.0 {
        return format!("{megabytes:.1} MB");
    }
    if bytes > 0.0 && bytes < 1.0 {
        return "<0.000001 MB".into();
    }
    let value = format!("{megabytes:.6}");
    format!("{} MB", value.trim_end_matches('0').trim_end_matches('.'))
}

fn format_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
#[path = "../tests/dashboard/mod.rs"]
mod tests;
