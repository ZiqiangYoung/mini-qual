use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use serde::Serialize;
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

pub struct RunLog {
    pub dir: PathBuf,
    pub started: Instant,
    events: File,
}

impl RunLog {
    pub fn create(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("creating log root {}", root.display()))?;
        let root = root
            .canonicalize()
            .with_context(|| format!("resolving --log-dir {}", root.display()))?;
        let suffix = Uuid::new_v4();
        let timestamp = utc_now().replace(':', "");
        let dir = root.join(format!("{timestamp}-{suffix}"));
        fs::create_dir(&dir)
            .with_context(|| format!("creating trial log directory {}", dir.display()))?;
        let events_path = dir.join("events.jsonl");
        let events = File::create_new(&events_path)
            .with_context(|| format!("creating event log {}", events_path.display()))?;
        File::open(&root)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing log root {}", root.display()))?;
        Ok(Self {
            dir,
            started: Instant::now(),
            events,
        })
    }

    pub fn event(&mut self, event: &str, data: Value) -> Result<()> {
        write_json_line(
            &mut self.events,
            &json!({
                "event": event,
                "time": utc_now(),
                "elapsed_seconds": self.started.elapsed().as_secs_f64(),
                "data": data,
            }),
        )
        .with_context(|| {
            format!(
                "writing event {event} to {}",
                self.dir.join("events.jsonl").display()
            )
        })
    }

    pub fn report(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let path = self.dir.join(name);
        let mut report = File::create_new(&path)
            .with_context(|| format!("creating report {}", path.display()))?;
        serde_json::to_writer_pretty(&mut report, value)
            .with_context(|| format!("writing report {}", path.display()))?;
        writeln!(report).with_context(|| format!("finishing report {}", path.display()))?;
        report
            .sync_all()
            .with_context(|| format!("syncing report {}", path.display()))?;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        self.events.sync_all().with_context(|| {
            format!(
                "syncing event log {}",
                self.dir.join("events.jsonl").display()
            )
        })?;
        File::open(&self.dir)
            .and_then(|directory| directory.sync_all())
            .with_context(|| format!("syncing trial log directory {}", self.dir.display()))?;
        Ok(())
    }
}

pub fn write_json_line(file: &mut File, value: &impl Serialize) -> Result<()> {
    let mut line = serde_json::to_vec(value)?;
    line.push(b'\n');
    file.write_all(&line)?;
    Ok(())
}

pub fn utc_now() -> String {
    OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .expect("UTC clock can be represented in RFC 3339")
}
