//! Application launcher: discovers XDG desktop entries and turns them into
//! spawnable commands, plus a simple fuzzy filter over their names.

use std::{
    collections::HashSet,
    env, fs,
    path::{Path, PathBuf},
    process::{Child, Command},
};

use tracing::{error, info};

#[derive(Debug, Clone)]
pub struct DesktopEntry {
    /// Desktop file id, e.g. "org.mozilla.firefox.desktop". Used for de-duplication.
    pub id: String,
    pub name: String,
    pub exec: String,
    pub terminal: bool,
}

/// Directories to look for `applications/*.desktop` in, in priority order
/// (matches the XDG Base Directory specification).
fn data_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    let data_home = env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")));
    if let Some(dir) = data_home {
        dirs.push(dir);
    }

    let data_dirs = env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
    dirs.extend(data_dirs.split(':').filter(|s| !s.is_empty()).map(PathBuf::from));

    dirs
}

/// Scans all XDG application directories and returns the visible, launchable
/// desktop entries, sorted by name. The first entry found for a given id wins
/// (matching XDG precedence), and hidden/non-application entries are skipped.
pub fn scan_desktop_entries() -> Vec<DesktopEntry> {
    let mut seen_ids = HashSet::new();
    let mut entries = Vec::new();

    for dir in data_dirs() {
        let apps_dir = dir.join("applications");
        let Ok(read_dir) = fs::read_dir(&apps_dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("desktop") {
                continue;
            }
            let Some(id) = path.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            if !seen_ids.insert(id.clone()) {
                continue;
            }
            if let Some(entry) = parse_desktop_file(&path, id) {
                entries.push(entry);
            }
        }
    }

    entries.sort_by_key(|a| a.name.to_lowercase());
    entries
}

/// Whether the desktop entry id `entry_id` (as stored, `foo.desktop`) is the one
/// a client asked for as `wanted`, which may or may not carry the `.desktop`
/// suffix (the shell's desktop-entry type omits it).
fn entry_id_matches(entry_id: &str, wanted: &str) -> bool {
    let strip = |id: &str| id.strip_suffix(".desktop").map(str::to_string).unwrap_or_else(|| id.to_string());
    !wanted.is_empty() && strip(entry_id) == strip(wanted)
}

/// Finds the installed, visible application whose desktop entry id is `id`
/// (with or without `.desktop`).
pub fn find_entry(id: &str) -> Option<DesktopEntry> {
    scan_desktop_entries()
        .into_iter()
        .find(|entry| entry_id_matches(&entry.id, id))
}

fn parse_desktop_file(path: &Path, id: String) -> Option<DesktopEntry> {
    let contents = fs::read_to_string(path).ok()?;

    let mut in_main_section = false;
    let mut name = None;
    let mut exec = None;
    let mut terminal = false;
    let mut is_application = true;
    let mut no_display = false;
    let mut hidden = false;

    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            in_main_section = section == "Desktop Entry";
            continue;
        }
        if !in_main_section {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        match key {
            "Name" => name = Some(value.to_string()),
            "Exec" => exec = Some(value.to_string()),
            "Terminal" => terminal = value.eq_ignore_ascii_case("true"),
            "Type" => is_application = value == "Application",
            "NoDisplay" => no_display = value.eq_ignore_ascii_case("true"),
            "Hidden" => hidden = value.eq_ignore_ascii_case("true"),
            _ => {}
        }
    }

    if !is_application || no_display || hidden {
        return None;
    }

    Some(DesktopEntry {
        id,
        name: name?,
        exec: exec?,
        terminal,
    })
}

/// Splits an `Exec=` value into a program and its arguments, stripping the
/// desktop-entry-spec field codes (`%f`, `%F`, `%u`, `%U`, `%i`, `%k`, `%c`, `%%`)
/// since the launcher never passes files/URIs to the started application.
fn parse_exec(exec: &str, name: &str) -> Option<(String, Vec<String>)> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut chars = exec.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '"' => in_quotes = !in_quotes,
            c if c.is_whitespace() && !in_quotes => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            '%' => match chars.next() {
                Some('%') => current.push('%'),
                Some('c') => current.push_str(name),
                // File/URL/icon/desktop-file placeholders: drop them, we never
                // launch with an associated file.
                Some('f' | 'F' | 'u' | 'U' | 'i' | 'k' | 'd' | 'D' | 'n' | 'N' | 'v' | 'm') => {}
                Some(other) => current.push(other),
                None => {}
            },
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }

    let mut iter = tokens.into_iter();
    let program = iter.next()?;
    Some((program, iter.collect()))
}

/// Applies compositor-provided environment overrides to `cmd`. A `None` value
/// unsets the variable instead of setting it (used to strip an inherited
/// `DISPLAY` so X11-capable toolkits can't fall back to the host's X server).
fn apply_envs<'a>(cmd: &mut Command, envs: impl IntoIterator<Item = &'a (&'static str, Option<String>)>) {
    for (key, value) in envs {
        match value {
            Some(value) => {
                cmd.env(key, value);
            }
            None => {
                cmd.env_remove(key);
            }
        }
    }
}

/// Spawns the given desktop entry, respecting `Terminal=true` by wrapping the
/// command in the user's terminal emulator. `envs` should set `WAYLAND_DISPLAY`
/// (and unset `DISPLAY` unless XWayland is running) to the compositor's own
/// sockets, so the launched app connects to this compositor instead of
/// whatever session it was started from.
pub fn launch<'a>(
    entry: &DesktopEntry,
    envs: impl IntoIterator<Item = &'a (&'static str, Option<String>)>,
) -> std::io::Result<Child> {
    let (program, args) = parse_exec(&entry.exec, &entry.name)
        .ok_or_else(|| std::io::Error::other(format!("empty Exec in {}", entry.id)))?;

    info!(id = entry.id, program, "Launching application");

    if entry.terminal {
        let terminal = env::var("TERMINAL").unwrap_or_else(|_| "weston-terminal".into());
        let mut cmd = Command::new(terminal);
        cmd.arg("-e").arg(&program).args(&args);
        apply_envs(&mut cmd, envs);
        cmd.spawn()
    } else {
        let mut cmd = Command::new(&program);
        cmd.args(&args);
        apply_envs(&mut cmd, envs);
        cmd.spawn()
    }
}

/// The command line to run inside a VM guest for `entry`: its `Exec=`
/// program and arguments (field codes stripped, see [`parse_exec`]), wrapped
/// in `<terminal> -e ...` for `Terminal=true` entries just like [`launch`] does
/// on the host. `terminal` is the guest's terminal emulator command and may
/// carry its own arguments (`"foot --app-id vm"`). The command is resolved by
/// the guest, so the program must be installed there.
pub fn guest_command(entry: &DesktopEntry, terminal: &str) -> Option<Vec<String>> {
    let (program, args) = parse_exec(&entry.exec, &entry.name)?;
    let mut command = Vec::new();
    if entry.terminal {
        command.extend(terminal.split_whitespace().map(String::from));
        if command.is_empty() {
            return None;
        }
        command.push("-e".to_string());
    }
    command.push(program);
    command.extend(args);
    Some(command)
}

pub fn launch_and_log<'a>(entry: &DesktopEntry, envs: impl IntoIterator<Item = &'a (&'static str, Option<String>)>) {
    if let Err(e) = launch(entry, envs) {
        error!(id = entry.id, err = %e, "Failed to launch application");
    }
}

/// Returns the indices of entries matching `query` (case-insensitive substring
/// match), best matches first: prefix matches before other substring matches,
/// ties broken alphabetically. An empty query matches everything.
pub fn filter_entries(entries: &[DesktopEntry], query: &str) -> Vec<usize> {
    let query = query.to_lowercase();
    if query.is_empty() {
        return (0..entries.len()).collect();
    }

    let mut scored: Vec<(i32, usize)> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, entry)| {
            let name = entry.name.to_lowercase();
            if name.starts_with(&query) {
                Some((0, i))
            } else if name.split_whitespace().any(|w| w.starts_with(&query)) {
                Some((1, i))
            } else if name.contains(&query) {
                Some((2, i))
            } else {
                None
            }
        })
        .collect();

    scored.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| entries[a.1].name.to_lowercase().cmp(&entries[b.1].name.to_lowercase()))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str) -> DesktopEntry {
        DesktopEntry {
            id: format!("{name}.desktop"),
            name: name.to_string(),
            exec: String::new(),
            terminal: false,
        }
    }

    #[test]
    fn parse_exec_strips_field_codes() {
        let (program, args) = parse_exec("firefox %u --new-window", "Firefox").unwrap();
        assert_eq!(program, "firefox");
        assert_eq!(args, vec!["--new-window"]);
    }

    #[test]
    fn parse_exec_keeps_quoted_arguments_together() {
        let (program, args) = parse_exec(r#"sh -c "echo hello world""#, "Shell").unwrap();
        assert_eq!(program, "sh");
        assert_eq!(args, vec!["-c", "echo hello world"]);
    }

    #[test]
    fn parse_exec_expands_percent_c_to_name() {
        let (program, args) = parse_exec("launcher %c", "My App").unwrap();
        assert_eq!(program, "launcher");
        assert_eq!(args, vec!["My App"]);
    }

    #[test]
    fn parse_exec_rejects_empty_command() {
        assert!(parse_exec("", "Empty").is_none());
    }

    #[test]
    fn parse_desktop_file_skips_hidden_entries() {
        let dir = std::env::temp_dir().join(format!("launcher-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hidden.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Hidden App\nExec=hidden\nNoDisplay=true\n",
        )
        .unwrap();

        assert!(parse_desktop_file(&path, "hidden.desktop".into()).is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parse_desktop_file_reads_visible_entries() {
        let dir = std::env::temp_dir().join(format!("launcher-test-{}", std::process::id() + 1));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("visible.desktop");
        std::fs::write(
            &path,
            "[Desktop Entry]\nType=Application\nName=Visible App\nExec=visible %F\nTerminal=true\n\n[Desktop Action New]\nName=New Window\nExec=visible --new\n",
        )
        .unwrap();

        let parsed = parse_desktop_file(&path, "visible.desktop".into()).unwrap();
        assert_eq!(parsed.name, "Visible App");
        assert_eq!(parsed.exec, "visible %F");
        assert!(parsed.terminal);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn entry_ids_match_with_or_without_the_desktop_suffix() {
        assert!(entry_id_matches("org.mozilla.firefox.desktop", "org.mozilla.firefox"));
        assert!(entry_id_matches("org.mozilla.firefox.desktop", "org.mozilla.firefox.desktop"));
        assert!(entry_id_matches("foot", "foot.desktop"));
        assert!(!entry_id_matches("foot.desktop", "foot-server"));
        assert!(!entry_id_matches("foot.desktop", ""));
        assert!(!entry_id_matches(".desktop", ""));
    }

    #[test]
    fn guest_command_is_the_exec_line_split_into_words() {
        let mut e = entry("Firefox");
        e.exec = "firefox %u --new-window".into();
        assert_eq!(
            guest_command(&e, "foot").unwrap(),
            vec!["firefox", "--new-window"]
        );
    }

    #[test]
    fn guest_command_wraps_terminal_apps_in_the_guest_terminal() {
        let mut e = entry("htop");
        e.exec = "htop".into();
        e.terminal = true;
        assert_eq!(
            guest_command(&e, "foot --app-id vm").unwrap(),
            vec!["foot", "--app-id", "vm", "-e", "htop"]
        );
        assert!(guest_command(&e, "  ").is_none());
    }

    #[test]
    fn guest_command_rejects_an_empty_exec() {
        assert!(guest_command(&entry("Empty"), "foot").is_none());
    }

    #[test]
    fn filter_entries_ranks_prefix_matches_first() {
        let entries = vec![entry("GNOME Terminal"), entry("Terminator"), entry("Files")];
        let filtered = filter_entries(&entries, "term");
        let names: Vec<_> = filtered.iter().map(|&i| entries[i].name.as_str()).collect();
        assert_eq!(names, vec!["Terminator", "GNOME Terminal"]);
    }

    #[test]
    fn filter_entries_empty_query_returns_everything_in_order() {
        let entries = vec![entry("A"), entry("B")];
        assert_eq!(filter_entries(&entries, ""), vec![0, 1]);
    }
}
