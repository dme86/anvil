//! Minimal XDG desktop-application launcher with Nucleo fuzzy ranking.

use std::{
    collections::HashSet,
    env,
    ffi::OsString,
    fs,
    path::{Path, PathBuf},
    sync::mpsc::{self, Sender},
    thread,
};

use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};

const MAX_RESULTS: usize = 6;

#[derive(Clone, Debug)]
struct DesktopEntry {
    id: OsString,
    name: String,
    searchable: String,
    argv: Vec<String>,
    terminal: bool,
}

#[derive(Clone, Debug)]
pub struct LaunchCommand {
    pub argv: Vec<String>,
    pub terminal: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct LauncherSnapshot {
    pub query: String,
    pub results: Vec<String>,
    pub selected: usize,
}

pub struct LauncherState {
    active: bool,
    query: String,
    selected: usize,
    entries: Vec<DesktopEntry>,
    matches: Vec<usize>,
    matcher: Matcher,
    scan_running: bool,
    scan_pending: bool,
    sender: Option<Sender<Vec<DesktopEntry>>>,
    ping: Option<smithay::reexports::calloop::ping::Ping>,
}

impl Default for LauncherState {
    fn default() -> Self {
        // Keep one canonical initialization path. Besides satisfying Rust's conventional
        // `Default` contract for a parameterless `new`, delegating here prevents the initially
        // visible desktop entries and the Nucleo matcher configuration from drifting apart if
        // launcher startup changes later.
        Self::new()
    }
}

impl LauncherState {
    pub fn new() -> Self {
        // Application directories are read only after installing the worker bridge.
        let entries = Vec::new();
        let matches = Vec::new();
        Self {
            active: false,
            query: String::new(),
            selected: 0,
            entries,
            matches,
            matcher: Matcher::new(Config::DEFAULT),
            scan_running: false,
            scan_pending: false,
            sender: None,
            ping: None,
        }
    }

    pub const fn active(&self) -> bool {
        self.active
    }

    pub fn open(&mut self) {
        // Display the current index immediately, and discover installations/removals
        // in the background. Repeated openings coalesce while a scan is running.
        self.request_refresh();
        self.query.clear();
        self.selected = 0;
        self.active = true;
        self.refresh_matches();
    }

    fn request_refresh(&mut self) {
        self.scan_pending = true;
        self.start_pending();
    }

    fn start_pending(&mut self) {
        if !self.scan_pending || self.scan_running {
            return;
        }
        let roots = application_roots();
        self.start_scan(move || desktop_entries(&roots));
    }

    fn start_scan<F>(&mut self, scan: F) -> bool
    where
        F: FnOnce() -> Vec<DesktopEntry> + Send + 'static,
    {
        if self.scan_running || !self.scan_pending {
            return false;
        }
        let (Some(sender), Some(ping)) = (&self.sender, &self.ping) else {
            return false;
        };
        let sender = sender.clone();
        let ping = ping.clone();
        self.scan_running = true;
        self.scan_pending = false;
        if let Err(error) = thread::Builder::new()
            .name("anvil-app-index".into())
            .spawn(move || {
                if sender.send(scan()).is_ok() {
                    ping.ping();
                }
            })
        {
            self.scan_running = false;
            self.scan_pending = true;
            tracing::warn!(%error, "cannot start application index worker");
            return false;
        }
        true
    }

    fn complete_scan(&mut self, entries: Vec<DesktopEntry>) -> bool {
        self.scan_running = false;
        let before = self.snapshot();
        let selected = self
            .matches
            .get(self.selected)
            .and_then(|&index| self.entries.get(index))
            .map(|entry| entry.id.clone());
        self.entries = entries;
        // Preserve input typed while scanning, and the selected application even
        // if another application changes its index through sorting/removal.
        self.refresh_matches();
        self.selected = selected
            .and_then(|id| {
                self.matches
                    .iter()
                    .position(|&index| self.entries[index].id == id)
            })
            .unwrap_or(0);
        self.snapshot() != before
    }

    pub fn close(&mut self) {
        self.active = false;
        self.query.clear();
    }

    pub fn insert(&mut self, character: char) {
        if !character.is_control() {
            self.query.push(character);
            self.selected = 0;
            self.refresh_matches();
        }
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.refresh_matches();
    }

    pub fn select_relative(&mut self, delta: isize) {
        if !self.matches.is_empty() {
            self.selected =
                (self.selected as isize + delta).rem_euclid(self.matches.len() as isize) as usize;
        }
    }

    pub fn accept(&mut self) -> Option<LaunchCommand> {
        let entry = self
            .matches
            .get(self.selected)
            .and_then(|&index| self.entries.get(index))?
            .clone();
        self.close();
        Some(LaunchCommand {
            argv: entry.argv,
            terminal: entry.terminal,
        })
    }

    pub fn snapshot(&self) -> Option<LauncherSnapshot> {
        self.active.then(|| LauncherSnapshot {
            query: self.query.clone(),
            results: self
                .matches
                .iter()
                .map(|&index| self.entries[index].name.clone())
                .collect(),
            selected: self.selected,
        })
    }

    fn refresh_matches(&mut self) {
        if self.query.is_empty() {
            self.matches = (0..self.entries.len().min(MAX_RESULTS)).collect();
            return;
        }
        let pattern = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let candidates = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| Candidate {
                index,
                text: entry.searchable.as_str(),
            });
        self.matches = pattern
            .match_list(candidates, &mut self.matcher)
            .into_iter()
            .take(MAX_RESULTS)
            .map(|(candidate, _)| candidate.index)
            .collect();
    }
}

#[derive(Clone, Copy)]
struct Candidate<'a> {
    index: usize,
    text: &'a str,
}

impl AsRef<str> for Candidate<'_> {
    fn as_ref(&self) -> &str {
        self.text
    }
}

fn application_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data_home).join("applications"));
    } else if let Some(home) = env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data_dirs =
        env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    roots.extend(env::split_paths(&data_dirs).map(|path| path.join("applications")));

    roots
}

fn desktop_entries(roots: &[PathBuf]) -> Vec<DesktopEntry> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for root in roots {
        let Ok(files) = fs::read_dir(root) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "desktop")
                && seen.insert(file.file_name())
                && path.metadata().is_ok_and(|metadata| metadata.is_file())
            {
                if let Some(entry) = parse_desktop_entry(&path) {
                    entries.push(entry);
                }
            }
        }
    }
    entries.sort_by_key(|entry| entry.name.to_lowercase());
    entries
}

fn parse_desktop_entry(path: &Path) -> Option<DesktopEntry> {
    let text = fs::read_to_string(path).ok()?;
    let mut in_entry = false;
    let mut name = None;
    let mut generic_name = None;
    let mut exec = None;
    let mut terminal = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry || line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "Type" if value != "Application" => return None,
            "Hidden" | "NoDisplay" if value.eq_ignore_ascii_case("true") => return None,
            "Name" => name = Some(value.to_owned()),
            "GenericName" => generic_name = Some(value.to_owned()),
            "Exec" => exec = Some(value.to_owned()),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }
    let name = name?;
    let argv = desktop_exec_argv(&exec?, &name, path)?;
    let searchable =
        generic_name.map_or_else(|| name.clone(), |generic| format!("{name} {generic}"));
    Some(DesktopEntry {
        id: path.file_name()?.to_owned(),
        name,
        searchable,
        argv,
        terminal,
    })
}

fn desktop_exec_argv(exec: &str, name: &str, path: &Path) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    for token in shlex::split(exec)? {
        if matches!(token.as_str(), "%f" | "%F" | "%u" | "%U" | "%i") {
            continue;
        }
        let expanded = token
            .replace("%%", "%")
            .replace("%c", name)
            .replace("%k", &path.display().to_string());
        let cleaned = ["%f", "%F", "%u", "%U", "%i"]
            .into_iter()
            .fold(expanded, |value, code| value.replace(code, ""));
        if !cleaned.is_empty() {
            argv.push(cleaned);
        }
    }
    (!argv.is_empty()).then_some(argv)
}

/// Main-loop bridge only: index workers never own or mutate compositor state.
pub(crate) fn install(
    event_loop: &mut smithay::reexports::calloop::EventLoop<crate::CalloopData>,
    data: &mut crate::CalloopData,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use smithay::reexports::calloop::ping::make_ping;
    let (sender, receiver) = mpsc::channel();
    let (ping, source) = make_ping().context("cannot create launcher notifications")?;
    data.state.launcher.sender = Some(sender);
    data.state.launcher.ping = Some(ping);
    event_loop
        .handle()
        .insert_source(source, move |(), _, data| {
            let mut changed = false;
            while let Ok(entries) = receiver.try_recv() {
                changed |= data.state.launcher.complete_scan(entries);
            }
            data.state.launcher.start_pending();
            if changed {
                data.state.request_repaint();
            }
        })
        .context("cannot register launcher notifications")?;
    data.state.launcher.request_refresh();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, name: &str) -> DesktopEntry {
        DesktopEntry {
            id: id.into(),
            name: name.into(),
            searchable: name.into(),
            argv: vec![id.into()],
            terminal: false,
        }
    }
    #[test]
    fn rescans_preserve_xdg_precedence_and_discover_installation_and_removal() {
        let root = std::env::temp_dir().join(format!("anvil-index-test-{}", std::process::id()));
        let first = root.join("first");
        let second = root.join("second");
        fs::create_dir_all(&first).unwrap();
        fs::create_dir_all(&second).unwrap();
        let desktop = "[Desktop Entry]\nName=Visible\nExec=/bin/true\n";
        fs::write(
            first.join("masked.desktop"),
            "[Desktop Entry]\nHidden=true\n",
        )
        .unwrap();
        fs::write(second.join("masked.desktop"), desktop).unwrap();
        fs::write(first.join("first.desktop"), desktop).unwrap();
        let roots = [first.clone(), second.clone()];
        let entries = desktop_entries(&roots);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "first.desktop");
        fs::write(second.join("new.desktop"), desktop).unwrap();
        assert_eq!(desktop_entries(&roots).len(), 2);
        fs::remove_file(first.join("first.desktop")).unwrap();
        let entries = desktop_entries(&roots);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, "new.desktop");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completed_index_preserves_live_query_and_selection() {
        let mut launcher = LauncherState::new();
        launcher.complete_scan(vec![entry("a", "App Alpha"), entry("b", "App Beta")]);
        launcher.open();
        launcher.insert('A');
        launcher.insert('p');
        launcher.insert('p');
        launcher.select_relative(1);
        let selected = launcher.accept().unwrap().argv;
        // Restore query/selection and insert another app before it in the index.
        launcher.open();
        for c in "App".chars() {
            launcher.insert(c);
        }
        launcher.select_relative(1);
        assert!(launcher.complete_scan(vec![
            entry("new", "App Added"),
            entry("a", "App Alpha"),
            entry("b", "App Beta")
        ]));
        assert_eq!(launcher.snapshot().unwrap().query, "App");
        assert_eq!(launcher.accept().unwrap().argv, selected);
    }
    #[test]
    fn slow_scan_does_not_block_open_and_repeated_refreshes_coalesce() {
        use std::{
            sync::{Arc, Barrier},
            time::Duration,
        };
        let mut launcher = LauncherState::new();
        launcher.complete_scan(vec![entry("cached", "Cached")]);
        let (sender, receiver) = mpsc::channel();
        let (ping, _source) = smithay::reexports::calloop::ping::make_ping().unwrap();
        launcher.sender = Some(sender);
        launcher.ping = Some(ping);
        launcher.scan_pending = true;
        let barrier = Arc::new(Barrier::new(2));
        let worker_gate = barrier.clone();
        assert!(launcher.start_scan(move || {
            worker_gate.wait();
            vec![entry("new", "New")]
        }));
        for _ in 0..100 {
            launcher.open();
            launcher.close();
        }
        launcher.open();
        assert_eq!(launcher.snapshot().unwrap().results, vec!["Cached"]);
        assert!(launcher.scan_pending && launcher.scan_running);
        assert!(!launcher.start_scan(|| panic!("overlapping scan")));
        barrier.wait();
        assert!(launcher.complete_scan(receiver.recv_timeout(Duration::from_secs(3)).unwrap()));
        assert_eq!(launcher.snapshot().unwrap().results, vec!["New"]);
        assert!(launcher.scan_pending);
    }

    #[test]
    fn exec_fields_do_not_become_shell_syntax() {
        assert_eq!(
            desktop_exec_argv(
                "firefox --name '%c' %U",
                "Web Browser",
                Path::new("app.desktop")
            ),
            Some(vec![
                "firefox".into(),
                "--name".into(),
                "Web Browser".into()
            ])
        );
    }
}
