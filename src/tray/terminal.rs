// SPDX-License-Identifier: Apache-2.0

//! Choosing a terminal for the TUI, and asking it for a window the TUI actually fits in.
//!
//! Both halves come from the same hand test failing. The tray used to try
//! `x-terminal-emulator` first, which is a *distribution* alternative rather than a user
//! preference: here it resolves to `gnome-terminal.wrapper` at priority 40 while kitty, the
//! author's terminal, is registered at 20. And it asked for no size at all, so the TUI opened
//! below its 78x30 minimum and showed "terminal too small" instead of the pairing QR.
//!
//! The user's real preference is recorded, in the freedesktop Default Terminal Execution place:
//! `~/.config/xdg-terminals.list`. This module reads it. It does not shell out to
//! `xdg-terminal-exec`, which resolves the same file correctly but takes the command directly and
//! offers no way to ask for a window size — and the size is half the bug.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};

/// How long to wait for a terminal to reject its own arguments. A terminal that starts and then
/// exits does so at once; anything still alive after this is a real window.
const SETTLE: Duration = Duration::from_millis(400);
const POLL: Duration = Duration::from_millis(20);

/// Words that mean "everything after this is the command". If `$TERMINAL` ends in one, the user
/// has written the invocation and wadb only appends the executable.
const EXEC_SEPARATORS: [&str; 4] = ["-e", "-x", "--command", "--"];

/// Terminals worth trying when nothing has expressed a preference, in order.
const KNOWN: [&str; 7] = [
    "kitty",
    "alacritty",
    "foot",
    "xfce4-terminal",
    "konsole",
    "gnome-terminal",
    "xterm",
];

/// Field codes a desktop entry's `Exec` may carry; they expand to filenames wadb is not passing.
const FIELD_CODES: [&str; 13] = [
    "%f", "%F", "%u", "%U", "%i", "%c", "%k", "%d", "%D", "%n", "%N", "%v", "%m",
];

/// The window the TUI needs, plus a little slack, from the TUI's own constants — so if those move,
/// this moves with them.
pub fn size() -> (u16, u16) {
    (crate::ui::MIN_WIDTH + 2, crate::ui::MIN_HEIGHT + 2)
}

/// The program name a table row is keyed on.
///
/// A Debian `*.wrapper` is deliberately **not** unwrapped to its target. Those scripts implement
/// the xterm convention (`-geometry WxH`, `-e cmd`) rather than the convention of the terminal
/// they exec, so driving `gnome-terminal.wrapper` with gnome-terminal's own flags would have it
/// silently drop every argument, including the command, and open a bare shell — exit 0, no TUI,
/// and nothing for the settle check below to notice. Left alone, a wrapper matches no row, so it
/// gets no size arguments and the `-e` it does understand.
pub fn basename(program: &str) -> &str {
    Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
}

/// How to ask this terminal for a `cols` by `rows` window. An empty list means "this terminal
/// cannot be sized from the command line", which is a fact about the terminal, not a gap here.
///
/// Verified on this machine: kitty and xfce4-terminal. The rest are from each terminal's own
/// documentation, which is why a rejected argument list is retried without it rather than trusted.
pub fn size_args(program: &str, cols: u16, rows: u16) -> Vec<String> {
    let arg = |s: String| s;
    match basename(program) {
        // remember_window_size defaults to yes and then *overrides* initial_window_*, so without
        // it kitty reopens at whatever size it was last dragged to.
        "kitty" => vec![
            "-o".into(),
            "remember_window_size=no".into(),
            "-o".into(),
            arg(format!("initial_window_width={cols}c")),
            "-o".into(),
            arg(format!("initial_window_height={rows}c")),
        ],
        "alacritty" => vec![
            "-o".into(),
            arg(format!("window.dimensions.columns={cols}")),
            "-o".into(),
            arg(format!("window.dimensions.lines={rows}")),
        ],
        "foot" => vec![arg(format!("--window-size-chars={cols}x{rows}"))],
        "xfce4-terminal" => vec![arg(format!("--geometry={cols}x{rows}"))],
        "konsole" => vec![
            "-p".into(),
            arg(format!("TerminalColumns={cols}")),
            "-p".into(),
            arg(format!("TerminalRows={rows}")),
        ],
        "xterm" => vec!["-geometry".into(), arg(format!("{cols}x{rows}"))],
        // GNOME Terminal is in this list on purpose, with nothing in it. --geometry has been
        // deprecated since 3.28 and is ignored by 3.56; worse, the client hands off to the server
        // and exits 0 at once, so a bad size would produce a small window, no error, and
        // "terminal too small" — the very defect this module exists to fix, surviving its own fix.
        _ => Vec::new(),
    }
}

/// The word that introduces the command, or `None` when the command is simply the last argument.
/// `-e` is not universal, and assuming it was is how an earlier version planned to hand
/// gnome-terminal a deprecated flag.
pub fn separator(program: &str) -> Option<&'static str> {
    match basename(program) {
        "foot" => None,
        "xfce4-terminal" => Some("-x"),
        "gnome-terminal" | "ptyxis" => Some("--"),
        _ => Some("-e"),
    }
}

/// Where the terminal lists and the desktop entries live: the config directories first, then the
/// application directories. The defaults matter as much as the variables — a session that exports
/// none of them must still find `~/.config/xdg-terminals.list`, or the search falls through to
/// the known list and picks the wrong terminal again.
#[cfg(test)]
pub fn search_dirs() -> Vec<PathBuf> {
    let mut dirs = config_dirs();
    dirs.extend(application_dirs());
    dirs
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn dirs_from(var: &str, default: &str) -> Vec<PathBuf> {
    let raw = std::env::var(var).unwrap_or_default();
    let raw = if raw.trim().is_empty() {
        default.to_string()
    } else {
        raw
    };
    raw.split(':')
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .collect()
}

fn config_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_CONFIG_HOME") {
        Some(v) if !v.is_empty() => dirs.push(PathBuf::from(v)),
        _ => dirs.push(home().join(".config")),
    }
    dirs.extend(dirs_from("XDG_CONFIG_DIRS", "/etc/xdg"));
    dirs
}

fn application_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    match std::env::var_os("XDG_DATA_HOME") {
        Some(v) if !v.is_empty() => dirs.push(PathBuf::from(v).join("applications")),
        _ => dirs.push(home().join(".local/share/applications")),
    }
    dirs.extend(
        dirs_from("XDG_DATA_DIRS", "/usr/local/share:/usr/share")
            .into_iter()
            .map(|d| d.join("applications")),
    );
    dirs
}

/// The terminal lists, in the order the spec asks for them: within each config directory, one per
/// entry of `$XDG_CURRENT_DESKTOP`, then the unprefixed one.
fn terminal_list_files() -> Vec<PathBuf> {
    let desktops: Vec<String> = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_lowercase())
        .collect();
    let mut files = Vec::new();
    for dir in config_dirs() {
        for desktop in &desktops {
            files.push(dir.join(format!("{desktop}-xdg-terminals.list")));
        }
        files.push(dir.join("xdg-terminals.list"));
    }
    files
}

/// Desktop-file ids, one per line. Blank lines and `#` comments are not ids.
pub fn parse_terminal_list(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| l.to_string())
        .collect()
}

/// What wadb needs out of a desktop entry.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DesktopEntry {
    /// `Exec`'s first field — the program that actually runs. `None` when `Exec` is missing, or
    /// when it carries arguments other than field codes.
    pub program: Option<String>,
    /// The probe key. Present means "check this exists before showing the entry".
    pub try_exec: Option<String>,
    /// The terminal's own exec separator. Present and empty means "no separator".
    pub exec_arg: Option<String>,
}

/// Read the `[Desktop Entry]` group, and only that group: a `[Desktop Action …]` further down
/// carries an `Exec` of its own that runs something else entirely.
pub fn parse_desktop_entry(contents: &str) -> DesktopEntry {
    let mut entry = DesktopEntry::default();
    let mut in_entry = false;
    for line in contents.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "Exec" => entry.program = exec_program(value.trim()),
            "TryExec" => entry.try_exec = Some(value.trim().to_string()),
            "X-ExecArg" => entry.exec_arg = Some(value.trim().to_string()),
            _ => {}
        }
    }
    entry
}

/// `Exec`'s first field, but only when wadb could run it faithfully.
///
/// `TryExec` is deliberately not used for this. The Desktop Entry spec uses it to decide whether
/// an entry should be *shown*; `Exec` is what runs, and the two can name different programs —
/// `ulauncher.desktop` on this machine has `TryExec=/usr/bin/ulauncher` against
/// `Exec=env GDK_BACKEND=x11 /usr/bin/ulauncher --hide-window`.
///
/// An `Exec` carrying anything but field codes means the first field is a wrapper (`env …`,
/// `flatpak run …`, `sh -c …`) and running it alone would launch something different, or nothing.
/// Such an entry is skipped, not truncated.
fn exec_program(exec: &str) -> Option<String> {
    // Split on whitespace only: Desktop Entry quoting is not implemented. A quoted path with a
    // space in it therefore yields a program that is neither on PATH nor a file, so the entry is
    // skipped and the search carries on — the same safe outcome as any other unusable entry.
    let mut words = exec.split_whitespace();
    let program = words.next()?.to_string();
    if words.any(|w| !FIELD_CODES.contains(&w)) {
        return None;
    }
    Some(program)
}

/// `foo-bar.desktop` may live at `foo-bar.desktop` or at `foo/bar.desktop`, per the spec.
fn id_paths(id: &str) -> Vec<PathBuf> {
    let mut out = vec![PathBuf::from(id)];
    let mut current = id.to_string();
    while let Some(i) = current.find('-') {
        current.replace_range(i..i + 1, "/");
        out.push(PathBuf::from(&current));
    }
    out
}

/// Is this program runnable? An absolute name is a file on disk; a bare name is looked up on
/// `PATH`.
fn resolve(program: &str) -> Option<PathBuf> {
    if program.contains('/') {
        let p = PathBuf::from(program);
        return p.is_file().then_some(p);
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join(program))
        .find(|p| p.is_file())
}

/// A terminal wadb might open, and what it knows about driving it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub program: String,
    /// Extra words the user put in `$TERMINAL`, kept in front of ours.
    pub extra: Vec<String>,
    /// `$TERMINAL` already ended in an exec separator, so wadb appends nothing but the command.
    pub user_separator: bool,
    /// `X-ExecArg` from the desktop entry, which beats the table.
    pub exec_arg: Option<String>,
    /// This came from `$TERMINAL`, so a failure is worth naming rather than swallowing.
    pub explicit: bool,
}

impl Candidate {
    fn plain(program: &str) -> Self {
        Self {
            program: program.to_string(),
            extra: Vec::new(),
            user_separator: false,
            exec_arg: None,
            explicit: false,
        }
    }
}

/// `$TERMINAL`, split on whitespace. Empty or whitespace-only counts as unset.
pub fn from_env(spec: &str) -> Option<Candidate> {
    let mut words = spec.split_whitespace().map(str::to_string);
    let program = words.next()?;
    let extra: Vec<String> = words.collect();
    // Only the *last* word decides. Scanning them all would take the `--` in a perfectly ordinary
    // `TERMINAL="kitty -o foo=--"` for a separator and hand kitty the executable as a bare
    // argument.
    let user_separator = extra
        .last()
        .is_some_and(|w| EXEC_SEPARATORS.contains(&w.as_str()));
    Some(Candidate {
        program,
        extra,
        user_separator,
        exec_arg: None,
        explicit: true,
    })
}

/// The terminals to try, best first.
pub fn candidates() -> Vec<Candidate> {
    let mut out = Vec::new();

    // 1. What the user said, explicitly.
    if let Some(c) = std::env::var("TERMINAL").ok().as_deref().and_then(from_env) {
        out.push(c);
    }

    // 2. What the user's desktop recorded.
    out.extend(from_terminal_lists());

    // 3. The distribution alternative, but only when it resolves to something wadb can size:
    //    that was the whole reason to rank it below the known list, and following the symlink
    //    removes the reason. It must be found on PATH first — canonicalising a bare name resolves
    //    it against the working directory, not PATH, so this step would never run at all.
    let (cols, rows) = size();
    if let Some(target) = resolve("x-terminal-emulator")
        .and_then(|p| std::fs::canonicalize(p).ok())
        .and_then(|p| p.to_str().map(str::to_string))
    {
        if !size_args(&target, cols, rows).is_empty() {
            out.push(Candidate::plain(&target));
        }
    }

    // 4. Terminals wadb knows how to size.
    for name in KNOWN {
        if resolve(name).is_some() {
            out.push(Candidate::plain(name));
        }
    }

    // 5. Last resort, unsized.
    if resolve("x-terminal-emulator").is_some() {
        out.push(Candidate::plain("x-terminal-emulator"));
    }

    out.dedup_by(|a, b| a.program == b.program && !a.explicit && !b.explicit);
    out
}

fn from_terminal_lists() -> Vec<Candidate> {
    let app_dirs = application_dirs();
    let mut out = Vec::new();
    for list in terminal_list_files() {
        let Ok(contents) = std::fs::read_to_string(&list) else {
            continue;
        };
        for id in parse_terminal_list(&contents) {
            if let Some(c) = candidate_for_id(&id, &app_dirs) {
                out.push(c);
            }
        }
    }
    out
}

fn candidate_for_id(id: &str, app_dirs: &[PathBuf]) -> Option<Candidate> {
    for dir in app_dirs {
        for rel in id_paths(id) {
            let Ok(contents) = std::fs::read_to_string(dir.join(rel)) else {
                continue;
            };
            let entry = parse_desktop_entry(&contents);
            // TryExec is the probe, and only when it is there. An entry with no TryExec key is
            // not skipped: the key is optional, and treating its absence as a failure would drop
            // every valid Exec-only desktop file.
            if let Some(probe) = &entry.try_exec {
                resolve(probe)?;
            }
            let program = entry.program?;
            resolve(&program)?;
            return Some(Candidate {
                program,
                extra: Vec::new(),
                user_separator: false,
                exec_arg: entry.exec_arg,
                explicit: false,
            });
        }
    }
    None
}

/// The full command line. Size arguments go before the separator, which every terminal that has
/// one requires.
pub fn argv(candidate: &Candidate, exe: &Path, size_args: &[String]) -> Vec<String> {
    let mut argv = vec![candidate.program.clone()];
    argv.extend(candidate.extra.clone());
    if candidate.user_separator {
        // The user wrote the invocation; wadb only finishes it. Adding size arguments here would
        // put them *after* something like alacritty's `--command`, which takes every following
        // word as the program to run — so the terminal would try to execute `-o`.
        argv.push(exe.to_string_lossy().into_owned());
        return argv;
    }
    argv.extend(size_args.iter().cloned());
    // X-ExecArg is authoritative where the table is a guess; present and empty means the command
    // simply goes last.
    match &candidate.exec_arg {
        Some(arg) if arg.is_empty() => {}
        Some(arg) => argv.push(arg.clone()),
        None => {
            if let Some(sep) = separator(&candidate.program) {
                argv.push(sep.to_string());
            }
        }
    }
    argv.push(exe.to_string_lossy().into_owned());
    argv
}

/// Start it, and give it long enough to reject its own arguments.
///
/// `Command::spawn` returns `Ok` the moment the binary is exec'd, so a terminal that dislikes a
/// flag starts, complains and exits *afterwards*. Checking the spawn alone is how Pair reports
/// success with no window on screen.
fn spawn_checked(argv: &[String]) -> Result<()> {
    let mut child = Command::new(&argv[0]).args(&argv[1..]).spawn()?;
    let deadline = Instant::now() + SETTLE;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => return Ok(()),
            Some(status) => return Err(anyhow!("{} exited with {status}", argv[0])),
            None if Instant::now() >= deadline => break,
            None => std::thread::sleep(POLL),
        }
    }
    // Still alive, so it is a real window. Reap it whenever it closes, or a closed terminal
    // leaves a zombie for the life of the tray. Its exit status says nothing: a terminal the user
    // closes can exit non-zero too.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn try_candidate(candidate: &Candidate, exe: &Path, cols: u16, rows: u16) -> Result<()> {
    let sized = if candidate.user_separator {
        Vec::new()
    } else {
        size_args(&candidate.program, cols, rows)
    };
    let first = spawn_checked(&argv(candidate, exe, &sized));
    match first {
        Ok(()) => Ok(()),
        // Nothing to drop, so a retry would be the same command twice and would learn nothing.
        Err(e) if sized.is_empty() => Err(e),
        Err(_) => spawn_checked(&argv(candidate, exe, &[])),
    }
}

/// Open the TUI in the user's terminal. `Ok(Some(note))` means it opened, with something the user
/// should be told.
pub fn open(exe: &Path) -> Result<Option<String>> {
    let (cols, rows) = size();
    let mut refused: Option<String> = None;
    for candidate in candidates() {
        match try_candidate(&candidate, exe, cols, rows) {
            Ok(()) => {
                // A user who set $TERMINAL deliberately must not be silently overridden.
                return Ok(refused.map(|failed| {
                    format!(
                        "$TERMINAL ({failed}) failed; opened {} instead",
                        candidate.program
                    )
                }));
            }
            Err(_) if candidate.explicit => refused = Some(candidate.program.clone()),
            Err(_) => {}
        }
    }
    Err(anyhow!("no terminal found; set $TERMINAL"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::EnvGuard;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch directory of this test's own. The crate has no `tempfile` dependency and the
    /// existing fixtures do it this way.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wadb-term-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn script(path: &Path, body: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn write(path: &Path, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn the_window_follows_the_tui_s_own_minimum() {
        // Not a hard-coded 80x32: if the TUI's guard moves, the request must move with it.
        assert_eq!(
            size(),
            (crate::ui::MIN_WIDTH + 2, crate::ui::MIN_HEIGHT + 2)
        );
    }

    #[test]
    fn a_terminal_list_is_ids_without_comments_or_blanks() {
        let list = "# a comment\n\nkitty.desktop\r\n  foot.desktop  \n\n#another\n";
        assert_eq!(
            parse_terminal_list(list),
            vec!["kitty.desktop".to_string(), "foot.desktop".to_string()]
        );
    }

    #[test]
    fn the_program_comes_from_exec_never_from_try_exec() {
        // The real shape of ulauncher.desktop on this machine. TryExec is a probe key: the spec
        // uses it to decide whether an entry is shown, and Exec is what runs. Taking TryExec as
        // the program would launch something the entry never asked for.
        let entry = parse_desktop_entry(
            "[Desktop Entry]\n\
             TryExec=/usr/bin/ulauncher\n\
             Exec=env GDK_BACKEND=x11 /usr/bin/ulauncher --hide-window\n",
        );
        assert_eq!(entry.try_exec.as_deref(), Some("/usr/bin/ulauncher"));
        assert_eq!(
            entry.program, None,
            "an Exec that is a wrapper invocation cannot be run by dropping its arguments"
        );
    }

    #[test]
    fn exec_keeps_field_codes_out_and_refuses_real_arguments() {
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nExec=kitty %F\n")
                .program
                .as_deref(),
            Some("kitty")
        );
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nExec=/usr/bin/kitty\n")
                .program
                .as_deref(),
            Some("/usr/bin/kitty")
        );
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nExec=flatpak run org.foo.Term\n").program,
            None,
            "dropping `run org.foo.Term` would launch flatpak itself"
        );
        assert_eq!(
            parse_desktop_entry("[Desktop Entry]\nName=x\n").program,
            None
        );
    }

    #[test]
    fn only_the_desktop_entry_group_is_read() {
        let entry = parse_desktop_entry(
            "[Desktop Entry]\n\
             Exec=kitty\n\
             X-ExecArg=--\n\
             [Desktop Action new-window]\n\
             Exec=kitty --single-instance\n\
             X-ExecArg=-e\n",
        );
        assert_eq!(entry.program.as_deref(), Some("kitty"));
        assert_eq!(entry.exec_arg.as_deref(), Some("--"));
    }

    #[test]
    fn rows_are_keyed_on_the_file_name_not_the_whole_path() {
        // A desktop entry gives an absolute Exec, and a match on the whole string would send no
        // size arguments down exactly the path that failed by hand.
        let (c, r) = (80, 32);
        assert_eq!(size_args("/usr/bin/kitty", c, r), size_args("kitty", c, r));
        assert_eq!(separator("/usr/bin/foot"), separator("foot"));
        assert!(size_args("/opt/weird/term", c, r).is_empty());
        assert_eq!(separator("/opt/weird/term"), Some("-e"));
    }

    #[test]
    fn kitty_is_told_not_to_remember_its_last_size() {
        // remember_window_size defaults to yes and overrides initial_window_*, so without it
        // kitty reopens at whatever size it was last dragged to - the original bug, one release
        // later. Spelled out as a whole vector: "the three size arguments" is a count, and a
        // count is what an argv bug hides behind.
        assert_eq!(
            argv(
                &Candidate::plain("kitty"),
                Path::new("/x/wadb"),
                &size_args("kitty", 80, 32)
            ),
            vec![
                "kitty",
                "-o",
                "remember_window_size=no",
                "-o",
                "initial_window_width=80c",
                "-o",
                "initial_window_height=32c",
                "-e",
                "/x/wadb",
            ]
        );
    }

    #[test]
    fn gnome_terminal_is_asked_for_no_size_at_all() {
        // --geometry has been deprecated since 3.28 and is ignored by 3.56. Worse, the client
        // hands off to the server and exits 0 at once, so a bad size would give a small window,
        // no error, and "terminal too small" - the defect this module exists to fix.
        assert!(size_args("gnome-terminal", 80, 32).is_empty());
        assert_eq!(separator("gnome-terminal"), Some("--"));
        assert_eq!(
            argv(
                &Candidate::plain("gnome-terminal"),
                Path::new("/x/wadb"),
                &[]
            ),
            vec!["gnome-terminal", "--", "/x/wadb"]
        );
    }

    #[test]
    fn a_debian_wrapper_is_driven_by_the_xterm_convention_it_implements() {
        // /usr/bin/{gnome,xfce4}-terminal.wrapper translate xterm's flags (-geometry, -e) into
        // their target's. Unwrapping the name and using the target's own flags would have the
        // wrapper silently drop every argument it did not recognise - including the command - and
        // open a bare shell: exit 0, no TUI, nothing for the settle check to notice. Left alone, a
        // wrapper matches no row, so it gets no size arguments and the -e it does understand.
        assert!(size_args("gnome-terminal.wrapper", 80, 32).is_empty());
        assert!(size_args("xfce4-terminal.wrapper", 80, 32).is_empty());
        assert_eq!(separator("xfce4-terminal.wrapper"), Some("-e"));
    }

    #[test]
    fn a_terminal_that_already_names_its_command_is_left_alone() {
        // The case an earlier revision got wrong. alacritty's -e *is* --command and takes every
        // following word as the program, so appending size arguments after it would have the
        // terminal try to execute "-o".
        let c = from_env("alacritty --command").unwrap();
        assert!(c.user_separator);
        assert_eq!(
            argv(&c, Path::new("/x/wadb"), &size_args("alacritty", 80, 32)),
            vec!["alacritty", "--command", "/x/wadb"]
        );
    }

    #[test]
    fn only_the_last_word_of_terminal_can_be_a_separator() {
        // TERMINAL="kitty -o foo=--" is an ordinary invocation. Scanning every word for "--"
        // would take that for a separator and hand kitty the executable as a bare argument.
        let c = from_env("kitty -o foo=--").unwrap();
        assert!(!c.user_separator);
        assert_eq!(
            argv(&c, Path::new("/x/wadb"), &[]),
            vec!["kitty", "-o", "foo=--", "-e", "/x/wadb"]
        );
        assert!(from_env("   ").is_none());
        assert!(from_env("").is_none());
    }

    #[test]
    fn x_exec_arg_beats_the_table() {
        // org.gnome.Terminal.desktop and org.gnome.Ptyxis.desktop both carry X-ExecArg=--. The
        // key exists precisely because -e is not universal, so it is authoritative where the
        // basename table is a guess.
        let mut c = Candidate::plain("weird-term");
        c.exec_arg = Some("--".into());
        assert_eq!(
            argv(&c, Path::new("/x/wadb"), &[]),
            vec!["weird-term", "--", "/x/wadb"]
        );
        c.exec_arg = Some(String::new());
        assert_eq!(
            argv(&c, Path::new("/x/wadb"), &[]),
            vec!["weird-term", "/x/wadb"],
            "present and empty means the command simply goes last"
        );
    }

    #[test]
    fn a_dashed_id_is_also_looked_for_in_a_subdirectory() {
        assert_eq!(
            id_paths("org-gnome-Terminal.desktop"),
            vec![
                PathBuf::from("org-gnome-Terminal.desktop"),
                PathBuf::from("org/gnome-Terminal.desktop"),
                PathBuf::from("org/gnome/Terminal.desktop"),
            ]
        );
    }

    #[test]
    fn the_defaults_are_used_when_the_variables_are_absent() {
        // The branch that runs on a session exporting none of these. Pointing them at temporary
        // directories, which every other test here does, never exercises it - and it is the
        // branch whose failure sends the search to the wrong terminal in silence.
        let mut env = EnvGuard::lock();
        let home = scratch("defaults");
        env.set("HOME", home.to_str().unwrap());
        for key in [
            "XDG_CONFIG_HOME",
            "XDG_CONFIG_DIRS",
            "XDG_DATA_HOME",
            "XDG_DATA_DIRS",
        ] {
            env.remove(key);
        }
        assert_eq!(
            search_dirs(),
            vec![
                home.join(".config"),
                PathBuf::from("/etc/xdg"),
                home.join(".local/share/applications"),
                PathBuf::from("/usr/local/share/applications"),
                PathBuf::from("/usr/share/applications"),
            ]
        );
    }

    /// A whole fixture desktop: config directories, application directories, and a PATH holding
    /// only what the test puts there.
    struct Fixture {
        root: PathBuf,
        bin: PathBuf,
    }

    impl Fixture {
        fn new(name: &str, env: &mut EnvGuard) -> Self {
            let root = scratch(name);
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            env.set("HOME", root.join("home").to_str().unwrap());
            env.set("XDG_CONFIG_HOME", root.join("config").to_str().unwrap());
            env.set("XDG_CONFIG_DIRS", root.join("etc").to_str().unwrap());
            env.set("XDG_DATA_HOME", root.join("data").to_str().unwrap());
            env.set("XDG_DATA_DIRS", root.join("sys").to_str().unwrap());
            env.set("XDG_CURRENT_DESKTOP", "XFCE");
            // PATH must be the fixture's own: kitty really is installed on this machine, so a
            // fixture naming it would otherwise resolve for the wrong reason and keep passing on
            // a machine where the parser is broken.
            env.set("PATH", bin.to_str().unwrap());
            env.remove("TERMINAL");
            Self { root, bin }
        }

        fn program(&self, name: &str) -> &Self {
            script(&self.bin.join(name), "exec /bin/sleep 5");
            self
        }

        fn user_list(&self, contents: &str) -> &Self {
            write(&self.root.join("config/xdg-terminals.list"), contents);
            self
        }

        fn desktop_list(&self, contents: &str) -> &Self {
            write(&self.root.join("config/xfce-xdg-terminals.list"), contents);
            self
        }

        fn user_entry(&self, rel: &str, contents: &str) -> &Self {
            write(&self.root.join("data/applications").join(rel), contents);
            self
        }

        fn system_entry(&self, rel: &str, contents: &str) -> &Self {
            write(&self.root.join("sys/applications").join(rel), contents);
            self
        }

        fn chosen(&self) -> Vec<String> {
            candidates().into_iter().map(|c| c.program).collect()
        }
    }

    #[test]
    fn the_desktop_prefixed_list_beats_the_plain_one() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("prefixed", &mut env);
        f.program("aaa").program("bbb");
        f.desktop_list("aaa.desktop\n").user_list("bbb.desktop\n");
        f.system_entry("aaa.desktop", "[Desktop Entry]\nExec=aaa\n");
        f.system_entry("bbb.desktop", "[Desktop Entry]\nExec=bbb\n");
        assert_eq!(f.chosen().first().map(String::as_str), Some("aaa"));
    }

    #[test]
    fn the_user_application_directory_beats_the_system_one() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("userdir", &mut env);
        f.program("mine").program("theirs");
        f.user_list("term.desktop\n");
        f.user_entry("term.desktop", "[Desktop Entry]\nExec=mine\n");
        f.system_entry("term.desktop", "[Desktop Entry]\nExec=theirs\n");
        assert_eq!(f.chosen().first().map(String::as_str), Some("mine"));
    }

    #[test]
    fn a_dashed_id_resolves_through_a_subdirectory() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("dashed", &mut env);
        f.program("sub");
        f.user_list("org-foo-Term.desktop\n");
        f.system_entry("org/foo/Term.desktop", "[Desktop Entry]\nExec=sub\n");
        assert_eq!(f.chosen().first().map(String::as_str), Some("sub"));
    }

    #[test]
    fn unusable_entries_are_skipped_rather_than_fatal() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("skips", &mut env);
        f.program("good");
        f.user_list("missing.desktop\nprobe.desktop\nwrapped.desktop\ngood.desktop\n");
        // A TryExec naming a binary that is not there: the entry should not be shown.
        f.system_entry(
            "probe.desktop",
            "[Desktop Entry]\nTryExec=/nowhere/at/all\nExec=good\n",
        );
        // An Exec that is a wrapper invocation: running its first field alone runs the wrong thing.
        f.system_entry("wrapped.desktop", "[Desktop Entry]\nExec=env FOO=1 good\n");
        f.system_entry("good.desktop", "[Desktop Entry]\nExec=good\n");
        assert_eq!(f.chosen().first().map(String::as_str), Some("good"));
    }

    #[test]
    fn an_entry_with_no_try_exec_key_is_not_skipped() {
        // TryExec is optional. Treating its absence as a failure would drop every valid
        // Exec-only desktop file and send the search to the known list again.
        let mut env = EnvGuard::lock();
        let f = Fixture::new("notryexec", &mut env);
        f.program("plain");
        f.user_list("plain.desktop\n");
        f.system_entry("plain.desktop", "[Desktop Entry]\nExec=plain\n");
        assert_eq!(f.chosen().first().map(String::as_str), Some("plain"));
    }

    #[test]
    fn with_no_lists_at_all_the_known_terminals_are_tried() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("nolists", &mut env);
        f.program("kitty").program("xterm");
        // kitty is first in the known list, so it wins over xterm.
        assert_eq!(f.chosen().first().map(String::as_str), Some("kitty"));
    }

    #[test]
    fn terminal_beats_everything_the_desktop_recorded() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("explicit", &mut env);
        f.program("chosen").program("kitty");
        f.user_list("kitty.desktop\n");
        f.system_entry("kitty.desktop", "[Desktop Entry]\nExec=kitty\n");
        env.set("TERMINAL", "chosen");
        assert_eq!(f.chosen().first().map(String::as_str), Some("chosen"));
    }

    #[test]
    fn a_terminal_that_dies_on_its_size_flags_is_retried_without_them() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("retry", &mut env);
        let log = f.root.join("argv.log");
        // Exits when handed a size flag, runs otherwise: a terminal whose documented flags this
        // machine cannot check, which is the case for four rows of the table.
        script(
            &f.bin.join("kitty"),
            &format!(
                "echo \"$*\" >> {log}\ncase \"$1\" in -o) exit 1 ;; esac\nexec /bin/sleep 5",
                log = log.display()
            ),
        );
        try_candidate(&Candidate::plain("kitty"), Path::new("/x/wadb"), 80, 32).unwrap();
        let calls: Vec<String> = std::fs::read_to_string(&log)
            .unwrap()
            .lines()
            .map(str::to_string)
            .collect();
        assert_eq!(calls.len(), 2, "sized once, then retried unsized");
        assert!(calls[0].contains("remember_window_size=no"));
        assert!(
            !calls[1].contains("-o"),
            "the retry must drop the size arguments: {}",
            calls[1]
        );
        assert!(calls[1].contains("-e /x/wadb"));
    }

    #[test]
    fn a_program_that_was_sent_no_size_arguments_is_not_retried() {
        // The retry argv would be identical to the first, so an unrelated failure - no DISPLAY,
        // say - would launch twice and learn nothing.
        let mut env = EnvGuard::lock();
        let f = Fixture::new("noretry", &mut env);
        let log = f.root.join("argv.log");
        script(
            &f.bin.join("weird-term"),
            &format!("echo x >> {log}\nexit 1", log = log.display()),
        );
        assert!(try_candidate(
            &Candidate::plain("weird-term"),
            Path::new("/x/wadb"),
            80,
            32
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(&log).unwrap().lines().count(), 1);
    }

    #[test]
    fn a_terminal_still_running_after_the_settle_window_is_a_success() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("settles", &mut env);
        f.program("kitty");
        assert!(try_candidate(&Candidate::plain("kitty"), Path::new("/x/wadb"), 80, 32).is_ok());
    }

    #[test]
    fn a_dead_child_is_never_reported_as_a_window() {
        // Command::spawn returns Ok the moment the binary is exec'd, so checking the spawn alone
        // is how Pair reports success with no window on screen. The search must move on, and an
        // explicit $TERMINAL that failed must be named rather than swallowed.
        let mut env = EnvGuard::lock();
        let f = Fixture::new("dead", &mut env);
        script(&f.bin.join("broken"), "exit 1");
        f.program("kitty");
        env.set("TERMINAL", "broken");
        let note = open(Path::new("/x/wadb")).unwrap();
        assert_eq!(
            note.as_deref(),
            Some("$TERMINAL (broken) failed; opened kitty instead")
        );
    }

    #[test]
    fn with_nothing_that_works_it_says_so_instead_of_claiming_success() {
        let mut env = EnvGuard::lock();
        let f = Fixture::new("none", &mut env);
        script(&f.bin.join("broken"), "exit 1");
        env.set("TERMINAL", "broken");
        assert!(open(Path::new("/x/wadb")).is_err());
    }
}
