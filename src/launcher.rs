//! Minimal XDG desktop-application launcher with Nucleo fuzzy ranking.

use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
};

use nucleo_matcher::{
    Config, Matcher,
    pattern::{CaseMatching, Normalization, Pattern},
};

const MAX_RESULTS: usize = 6;

#[derive(Clone, Debug)]
struct DesktopEntry {
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

#[derive(Clone, Debug)]
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
}

impl LauncherState {
    pub fn new() -> Self {
        let entries = desktop_entries();
        let matches = (0..entries.len().min(MAX_RESULTS)).collect();
        Self {
            active: false,
            query: String::new(),
            selected: 0,
            entries,
            matches,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub const fn active(&self) -> bool {
        self.active
    }

    pub fn open(&mut self) {
        // Rescan on every opening so newly installed applications appear without restarting the
        // compositor. Matching remains cheap because desktop installations are relatively small.
        self.entries = desktop_entries();
        self.query.clear();
        self.selected = 0;
        self.active = true;
        self.refresh_matches();
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

fn desktop_entries() -> Vec<DesktopEntry> {
    let mut roots = Vec::new();
    if let Some(data_home) = env::var_os("XDG_DATA_HOME") {
        roots.push(PathBuf::from(data_home).join("applications"));
    } else if let Some(home) = env::var_os("HOME") {
        roots.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data_dirs =
        env::var_os("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    roots.extend(env::split_paths(&data_dirs).map(|path| path.join("applications")));

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

#[cfg(test)]
mod tests {
    use super::*;

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
