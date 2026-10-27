//! Non-blocking shell status blocks for the built-in bar.
//!
//! Only the calloop thread owns visible status state. Bounded child workers report completed
//! results through a channel and a calloop ping; no shell command may stall input or KMS.
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

use anvil::config::{Bar, BarBlock};
use anyhow::{Context, Result};
use smithay::reexports::calloop::{
    EventLoop,
    ping::{Ping, make_ping},
};

use crate::CalloopData;

const MAX_OUTPUT_BYTES: u64 = 4096;
const MAX_VISIBLE_CHARS: usize = 256;

struct Job {
    command: String,
    interval: Duration,
    timeout: Duration,
    last_started: Option<Instant>,
    running: bool,
    output: String,
}

struct Completion {
    generation: u64,
    index: usize,
    result: std::result::Result<String, String>,
}

/// A completed command only changes compositor state after the event loop receives its ping.
pub struct BarJobs {
    jobs: Vec<Job>,
    generation: u64,
    sender: Option<Sender<Completion>>,
    ping: Option<Ping>,
    visible: String,
}

impl BarJobs {
    pub fn new(config: &Bar) -> Self {
        Self {
            jobs: create_jobs(config),
            generation: 0,
            sender: None,
            ping: None,
            visible: String::new(),
        }
    }

    fn connect(&mut self, sender: Sender<Completion>, ping: Ping) {
        self.sender = Some(sender);
        self.ping = Some(ping);
    }

    pub fn reconfigure(&mut self, config: &Bar) -> bool {
        self.generation = self.generation.wrapping_add(1);
        self.jobs = create_jobs(config);
        let changed = !self.visible.is_empty();
        self.visible.clear();
        changed
    }

    pub fn visible(&self) -> &str {
        &self.visible
    }

    pub fn refresh(&mut self) {
        let (Some(sender), Some(ping)) = (&self.sender, &self.ping) else {
            return;
        };
        let now = Instant::now();
        for (index, job) in self.jobs.iter_mut().enumerate() {
            if job.running
                || job
                    .last_started
                    .is_some_and(|started| now.duration_since(started) < job.interval)
            {
                continue;
            }
            job.running = true;
            job.last_started = Some(now);
            let command = job.command.clone();
            let timeout = job.timeout;
            let generation = self.generation;
            let sender = Sender::clone(sender);
            let ping = Ping::clone(ping);
            let spawn = thread::Builder::new()
                .name(format!("anvil-bar-{index}"))
                .spawn(move || {
                    let result = run_command(&command, timeout).map_err(|error| error.to_string());
                    if sender
                        .send(Completion {
                            generation,
                            index,
                            result,
                        })
                        .is_ok()
                    {
                        ping.ping();
                    }
                });
            if let Err(error) = spawn {
                job.running = false;
                tracing::warn!(index, %error, "cannot spawn bar block worker");
            }
        }
    }

    fn complete(&mut self, completed: Completion) -> bool {
        if completed.generation != self.generation {
            // A previous config generation is never permitted to overwrite reloaded blocks.
            return false;
        }
        let Some(job) = self.jobs.get_mut(completed.index) else {
            return false;
        };
        job.running = false;
        match completed.result {
            Ok(value) => job.output = value,
            Err(error) => {
                // Keep the last successful output instead of flickering on network errors.
                tracing::warn!(index = completed.index, %error, "bar block failed");
                return false;
            }
        }
        let visible = self
            .jobs
            .iter()
            .filter(|job| !job.output.is_empty())
            .map(|job| job.output.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        if self.visible == visible {
            return false;
        }
        self.visible = visible;
        true
    }
}

fn create_jobs(config: &Bar) -> Vec<Job> {
    let blocks: Vec<BarBlock> = if config.blocks.is_empty() {
        // Legacy status_commands remain supported, but now run outside the compositor too.
        config
            .status_commands
            .iter()
            .enumerate()
            .map(|(index, command)| BarBlock {
                name: format!("legacy-{index}"),
                command: command.clone(),
                interval: config.refresh_interval_ms.div_ceil(1000).max(1),
                timeout: 10,
            })
            .collect()
    } else {
        config.blocks.clone()
    };
    blocks
        .into_iter()
        .map(|block| Job {
            command: block.command,
            interval: Duration::from_secs(block.interval),
            timeout: Duration::from_secs(block.timeout),
            last_started: None,
            running: false,
            output: String::new(),
        })
        .collect()
}

/// GNU coreutils timeout gives scripts a bounded runtime without blocking the compositor.
/// stdout is capped before conversion to UTF-8; stderr is intentionally never shown in the bar.
fn run_command(command: &str, timeout: Duration) -> Result<String> {
    let mut child = Command::new("timeout")
        .arg("--signal=TERM")
        .arg("--kill-after=2s")
        .arg(format!("{}s", timeout.as_secs()))
        .arg("/bin/sh")
        .arg("-c")
        .arg(command)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot launch bar command: {command}"))?;
    let mut bytes = Vec::new();
    let read_result = child
        .stdout
        .take()
        .context("missing bar child stdout")?
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes);
    if read_result.is_err() || bytes.len() as u64 > MAX_OUTPUT_BYTES {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("bar command output exceeded limit or could not be read");
    }
    let status = child.wait().context("cannot wait for bar command")?;
    if !status.success() {
        anyhow::bail!("bar command returned {status}");
    }
    // Strip newlines, control sequences and excessive output; the bar is single-line text.
    let text = String::from_utf8_lossy(&bytes);
    Ok(text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_VISIBLE_CHARS)
        .collect())
}

pub fn install(event_loop: &mut EventLoop<CalloopData>, data: &mut CalloopData) -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let (ping, source) = make_ping().context("cannot initialize bar status notifications")?;
    data.state.bar.jobs.connect(sender, ping);
    event_loop
        .handle()
        .insert_source(source, move |(), _, data| {
            let mut changed = false;
            while let Ok(completed) = receiver.try_recv() {
                changed |= data.state.bar.jobs.complete(completed);
            }
            if changed {
                data.state.request_repaint();
            }
        })
        .context("cannot register bar status notifications")?;
    // Start initial jobs now rather than blocking first paint on shell execution.
    data.state.bar.jobs.refresh();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unchanged_result_does_not_trigger_another_repaint() {
        let config = Bar {
            blocks: vec![BarBlock {
                name: "clock".into(),
                command: "date".into(),
                interval: 60,
                timeout: 5,
            }],
            ..Bar::default()
        };
        let mut jobs = BarJobs::new(&config);
        assert!(jobs.complete(Completion {
            generation: 0,
            index: 0,
            result: Ok("12:30".into()),
        }));
        assert!(!jobs.complete(Completion {
            generation: 0,
            index: 0,
            result: Ok("12:30".into()),
        }));
        assert_eq!(jobs.visible(), "12:30");
    }

    #[test]
    fn stale_results_cannot_override_reloaded_config() {
        let config = Bar::default();
        let mut jobs = BarJobs::new(&config);
        jobs.reconfigure(&config);
        assert!(!jobs.complete(Completion {
            generation: 0,
            index: 0,
            result: Ok("stale".into()),
        }));
        assert!(jobs.visible().is_empty());
    }

    #[test]
    fn command_captures_one_line_with_bounded_output() {
        assert_eq!(
            run_command("printf 'hello\\nworld'", Duration::from_secs(2)).unwrap(),
            "hello world"
        );
        assert!(run_command("exit 1", Duration::from_secs(2)).is_err());
    }
}
