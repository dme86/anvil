//! Cheap cumulative counters; Linux resource files are read only for an explicit IPC snapshot.

use std::time::Duration;

#[derive(Default)]
pub(crate) struct Diagnostics {
    pub requested_repaints: u64,
    pub render_attempts: u64,
    pub rendered_frames: u64,
    pub render_failures: u64,
    pub dmabuf_imports: u64,
    pub dmabuf_import_failures: u64,
    render_time: Duration,
}

impl Diagnostics {
    /// One backend/output attempt, including damage-free frames and failed submissions.
    pub fn record_render(&mut self, elapsed: Duration, submitted: bool, failed: bool) {
        self.render_attempts = self.render_attempts.saturating_add(1);
        self.render_time = self.render_time.saturating_add(elapsed);
        self.rendered_frames = self.rendered_frames.saturating_add(u64::from(submitted));
        self.render_failures = self.render_failures.saturating_add(u64::from(failed));
    }

    #[cfg(feature = "anvilctl")]
    fn average_render_time_ms(&self) -> Option<f64> {
        (self.render_attempts != 0)
            .then(|| self.render_time.as_secs_f64() * 1000.0 / self.render_attempts as f64)
    }
}

#[cfg(feature = "anvilctl")]
impl crate::Anvil {
    pub(crate) fn runtime_stats(&self) -> anvil::ipc::RuntimeStats {
        let mut connected_clients = 0;
        self.display_handle
            .backend_handle()
            .with_all_clients(|_| connected_clients += 1);
        let counters = &self.diagnostics;
        anvil::ipc::RuntimeStats {
            uptime_seconds: self.start_time.elapsed().as_secs_f64(),
            connected_clients,
            managed_windows: self.windows.len(),
            outputs: self.outputs.len(),
            session_locked: self.session_locked(),
            requested_repaints: counters.requested_repaints,
            render_attempts: counters.render_attempts,
            rendered_frames: counters.rendered_frames,
            render_failures: counters.render_failures,
            average_render_time_ms: counters.average_render_time_ms(),
            dmabuf_imports: counters.dmabuf_imports,
            dmabuf_import_failures: counters.dmabuf_import_failures,
            // Count successful directory entries only, excluding the temporary read_dir fd.
            open_file_descriptors: std::fs::read_dir("/proc/self/fd")
                .ok()
                .map(|entries| entries.filter_map(Result::ok).count().saturating_sub(1)),
            rss_bytes: std::fs::read_to_string("/proc/self/status")
                .ok()
                .and_then(|status| parse_rss_bytes(&status)),
        }
    }
}

#[cfg(feature = "anvilctl")]
fn parse_rss_bytes(status: &str) -> Option<u64> {
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let mut fields = line.split_whitespace();
    fields.next()?;
    let kib = fields.next()?.parse::<u64>().ok()?;
    (fields.next()? == "kB").then_some(())?;
    kib.checked_mul(1024)
}

#[cfg(all(test, feature = "anvilctl"))]
mod tests {
    use super::*;

    #[test]
    fn resource_data_must_have_the_expected_units_and_fit() {
        assert_eq!(
            parse_rss_bytes("Name: anvil\nVmRSS:\t123 kB\n"),
            Some(125952)
        );
        assert_eq!(parse_rss_bytes("VmRSS: 123 MB"), None);
        assert_eq!(parse_rss_bytes("VmRSS: nope kB"), None);
        assert_eq!(parse_rss_bytes("VmRSS: 18446744073709551615 kB"), None);
        assert_eq!(parse_rss_bytes("Name: anvil"), None);
    }

    #[test]
    fn render_counters_distinguish_no_damage_submission_and_failure() {
        let mut counters = Diagnostics::default();
        assert_eq!(counters.average_render_time_ms(), None);
        counters.record_render(Duration::from_millis(2), false, false);
        counters.record_render(Duration::from_millis(4), true, false);
        counters.record_render(Duration::from_millis(6), false, true);
        assert_eq!(counters.render_attempts, 3);
        assert_eq!(counters.rendered_frames, 1);
        assert_eq!(counters.render_failures, 1);
        assert_eq!(counters.average_render_time_ms(), Some(4.0));
    }
}
