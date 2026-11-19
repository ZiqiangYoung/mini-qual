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
        let root = root.canonicalize()?;
        let suffix = Uuid::new_v4();
        let timestamp = utc_now().replace(':', "");
        let dir = root.join(format!("{timestamp}-{suffix}"));
        fs::create_dir(&dir)?;
        let events = File::create_new(dir.join("events.jsonl"))?;
        File::open(&root)?.sync_all()?;
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
        .with_context(|| format!("recording {event}"))
    }

    pub fn report(&self, name: &str, value: &impl Serialize) -> Result<()> {
        let mut report = File::create_new(self.dir.join(name))?;
        serde_json::to_writer_pretty(&mut report, value)?;
        writeln!(report)?;
        report.sync_all()?;
        Ok(())
    }

    pub fn sync(&self) -> Result<()> {
        self.events.sync_all()?;
        File::open(&self.dir)?.sync_all()?;
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
