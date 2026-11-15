//! Bounded background NetworkManager queries for the optional bar icon.
//!
//! Only one query may be outstanding, including across config changes. Workers
//! report through a channel/ping; the compositor never reads or waits on a child.

use std::{
    ffi::OsStr,
    io::Read,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    sync::mpsc::{self, Sender},
    thread,
    time::{Duration, Instant},
};

use anvil::config::BarNetwork;
use anyhow::{Context, Result};
use rustix::process::{Pid, Signal, kill_process_group};
use smithay::reexports::calloop::{
    EventLoop,
    ping::{Ping, make_ping},
};

use crate::{CalloopData, bar::NetworkLink};

const MAX_OUTPUT_BYTES: u64 = 64 * 1024;
const QUERY_TIMEOUT: Duration = Duration::from_secs(2);

struct Completion {
    generation: u64,
    result: std::result::Result<NetworkLink, String>,
}

/// Main-loop state only: dropping it never joins a worker or waits for a child.
pub(crate) struct NetworkJobs {
    enabled: bool,
    interval: Duration,
    generation: u64,
    running: Option<u64>,
    last_started: Option<Instant>,
    visible: NetworkLink,
    sender: Option<Sender<Completion>>,
    ping: Option<Ping>,
}

impl NetworkJobs {
    pub(crate) fn new(config: &BarNetwork) -> Self {
        Self {
            enabled: config.enabled,
            interval: Duration::from_millis(config.refresh_interval_ms),
            generation: 0,
            running: None,
            last_started: None,
            visible: NetworkLink::Offline,
            sender: None,
            ping: None,
        }
    }

    fn connect(&mut self, sender: Sender<Completion>, ping: Ping) {
        self.sender = Some(sender);
        self.ping = Some(ping);
    }

    pub(crate) fn configure(&mut self, config: &BarNetwork) {
        let interval = Duration::from_millis(config.refresh_interval_ms);
        if (self.enabled, self.interval) != (config.enabled, interval) {
            self.enabled = config.enabled;
            self.interval = interval;
            self.generation = self.generation.wrapping_add(1);
            self.last_started = None;
            // Keep the in-flight slot occupied until even an obsolete worker
            // completes. Repeated reloads cannot create overlapping queries.
        }
    }

    pub(crate) fn visible(&self) -> NetworkLink {
        self.visible
    }

    pub(crate) fn refresh(&mut self, config: &BarNetwork) {
        self.configure(config);
        self.try_start(Instant::now(), || {
            query_nmcli(OsStr::new("nmcli"), QUERY_TIMEOUT).map_err(|error| error.to_string())
        });
    }

    fn try_start<F>(&mut self, now: Instant, query: F) -> bool
    where
        F: FnOnce() -> std::result::Result<NetworkLink, String> + Send + 'static,
    {
        if !self.enabled
            || self.running.is_some()
            || self
                .last_started
                .is_some_and(|last| now.duration_since(last) < self.interval)
        {
            return false;
        }
        let (Some(sender), Some(ping)) = (&self.sender, &self.ping) else {
            return false;
        };
        let generation = self.generation;
        let sender = sender.clone();
        let ping = ping.clone();
        self.running = Some(generation);
        self.last_started = Some(now);
        if let Err(error) = thread::Builder::new()
            .name("anvil-network".into())
            .spawn(move || {
                let result = query();
                if sender.send(Completion { generation, result }).is_ok() {
                    ping.ping();
                }
            })
        {
            self.running = None;
            tracing::warn!(%error, "cannot start network status worker");
            return false;
        }
        true
    }

    fn complete(&mut self, completed: Completion) -> bool {
        if self.running != Some(completed.generation) {
            return false;
        }
        self.running = None;
        if completed.generation != self.generation || !self.enabled {
            return false;
        }
        let network = match completed.result {
            Ok(network) => network,
            Err(error) => {
                tracing::warn!(%error, "network status query failed");
                NetworkLink::Offline
            }
        };
        let changed = self.visible != network;
        self.visible = network;
        changed
    }
}

/// GNU timeout bounds a hung nmcli/D-Bus call. Only a worker reads stdout/waits;
/// a separate process group also permits complete cleanup on output overflow.
fn query_nmcli(program: &OsStr, timeout: Duration) -> Result<NetworkLink> {
    let mut child = Command::new("timeout")
        .envs(crate::session::ENVIRONMENT)
        .args(["--signal=TERM", "--kill-after=1s"])
        .arg(format!("{}s", timeout.as_secs_f64()))
        .arg(program)
        .args(["-t", "-f", "TYPE,STATE", "device", "status"])
        .env("LC_ALL", "C")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("cannot start network status query")?;
    let mut bytes = Vec::new();
    let read = child
        .stdout
        .take()
        .context("missing nmcli stdout")?
        .take(MAX_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes);
    if read.is_err() || bytes.len() as u64 > MAX_OUTPUT_BYTES {
        let _ = kill_process_group(Pid::from_child(&child), Signal::KILL);
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("network status output exceeded limit or could not be read");
    }
    let status = child
        .wait()
        .context("cannot wait for network status query")?;
    if !status.success() {
        anyhow::bail!("network status query returned {status}");
    }
    Ok(NetworkLink::from_nmcli(&String::from_utf8_lossy(&bytes)))
}

pub(crate) fn install(
    event_loop: &mut EventLoop<CalloopData>,
    data: &mut CalloopData,
) -> Result<()> {
    let (sender, receiver) = mpsc::channel();
    let (ping, source) = make_ping().context("cannot initialize network status notifications")?;
    data.state.bar.network_jobs.connect(sender, ping);
    event_loop
        .handle()
        .insert_source(source, move |(), _, data| {
            let jobs = &mut data.state.bar.network_jobs;
            // Validate the current config even if a completion arrives before the
            // next periodic refresh following a reload.
            jobs.configure(&data.state.config.bar.network);
            let mut changed = false;
            while let Ok(completed) = receiver.try_recv() {
                changed |= jobs.complete(completed);
            }
            jobs.refresh(&data.state.config.bar.network);
            if changed && data.state.bar_visible {
                data.state.request_repaint();
            }
        })
        .context("cannot register network status notifications")?;
    data.state
        .bar
        .network_jobs
        .refresh(&data.state.config.bar.network);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier},
    };

    fn connected(enabled: bool) -> (NetworkJobs, mpsc::Receiver<Completion>) {
        let config = BarNetwork {
            enabled,
            ..BarNetwork::default()
        };
        let mut jobs = NetworkJobs::new(&config);
        let (sender, receiver) = mpsc::channel();
        let (ping, _source) = make_ping().unwrap();
        jobs.connect(sender, ping);
        (jobs, receiver)
    }

    #[test]
    fn disabled_widget_starts_no_worker() {
        let (mut jobs, _) = connected(false);
        assert!(!jobs.try_start(Instant::now(), || panic!("disabled worker ran")));
        assert_eq!(jobs.running, None);
    }

    #[test]
    fn reloads_keep_one_worker_and_ignore_its_obsolete_result() {
        let (mut jobs, receiver) = connected(true);
        let gate = Arc::new(Barrier::new(2));
        let worker_gate = gate.clone();
        assert!(jobs.try_start(Instant::now(), move || {
            worker_gate.wait();
            Ok(NetworkLink::Wifi)
        }));
        let disabled = BarNetwork {
            enabled: false,
            ..BarNetwork::default()
        };
        jobs.configure(&disabled);
        let enabled = BarNetwork {
            enabled: true,
            ..disabled
        };
        jobs.configure(&enabled);
        assert!(!jobs.try_start(Instant::now(), || panic!("overlapping worker")));
        assert_eq!(jobs.visible(), NetworkLink::Offline);
        gate.wait();
        assert!(!jobs.complete(receiver.recv_timeout(Duration::from_secs(3)).unwrap()));
        assert_eq!(jobs.running, None);
        assert_eq!(jobs.visible(), NetworkLink::Offline);
        assert!(jobs.try_start(Instant::now(), || Ok(NetworkLink::Ethernet)));
        assert!(jobs.complete(receiver.recv_timeout(Duration::from_secs(3)).unwrap()));
        assert_eq!(jobs.visible(), NetworkLink::Ethernet);
        assert!(!jobs.try_start(Instant::now(), || panic!("interval ignored")));
    }

    #[test]
    fn delayed_worker_preserves_last_icon_and_failure_returns_offline() {
        let (mut jobs, receiver) = connected(true);
        jobs.visible = NetworkLink::Wifi;
        let gate = Arc::new(Barrier::new(2));
        let worker_gate = gate.clone();
        assert!(jobs.try_start(Instant::now(), move || {
            worker_gate.wait();
            Err("query timed out".into())
        }));
        assert_eq!(jobs.visible(), NetworkLink::Wifi);
        assert!(!jobs.try_start(Instant::now(), || panic!("overlapping worker")));
        gate.wait();
        assert!(jobs.complete(receiver.recv_timeout(Duration::from_secs(3)).unwrap()));
        assert_eq!(jobs.visible(), NetworkLink::Offline);
    }

    fn fake_nmcli(body: &str, suffix: &str) -> std::path::PathBuf {
        let path =
            std::env::temp_dir().join(format!("anvil-network-{}-{suffix}.sh", std::process::id()));
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }

    #[test]
    fn query_parses_success_and_bounds_time_and_output() {
        let path = fake_nmcli("printf 'wifi:connected\\nethernet:connected\\n'", "success");
        assert_eq!(
            query_nmcli(path.as_os_str(), QUERY_TIMEOUT).unwrap(),
            NetworkLink::Ethernet
        );
        fs::remove_file(path).unwrap();
        let path = fake_nmcli("sleep 10", "timeout");
        let start = Instant::now();
        assert!(query_nmcli(path.as_os_str(), Duration::from_millis(50)).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        fs::remove_file(path).unwrap();
        let path = fake_nmcli("head -c 70000 /dev/zero; sleep 10", "overflow");
        let start = Instant::now();
        assert!(query_nmcli(path.as_os_str(), QUERY_TIMEOUT).is_err());
        assert!(start.elapsed() < Duration::from_secs(3));
        fs::remove_file(path).unwrap();
    }
}
