//! `sled install` / `sled uninstall`: register or remove the hook in Claude Code settings.

use crate::paths::Paths;
use crate::quote;
use crate::sweep;
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

const EVENTS: [&str; 3] = ["SessionStart", "Stop", "SessionEnd"];
const RETENTION_KEY: &str = "cleanupPeriodDays";
const WANTED_RETENTION: u64 = 365;
const DEFAULT_RETENTION: u64 = 30;
/// Claude Code's per-hook timeout, in seconds.
const HOOK_TIMEOUT_SECS: u64 = 10;

pub enum Retention {
    Ask,
    Set(u32),
    Keep,
}

/// True when `command` runs a program named `sled` with the single argument `hook`, written in
/// one of the encodings sled itself has used: plain, single-quoted with `'\''` escapes, or
/// double-quoted (older installs). Anything else, such as `notify;/x/sled hook`, is not ours.
fn is_ours(command: &str) -> bool {
    if command.contains(['\n', '\r']) {
        return false; // a shell would run what follows a line break as another command
    }
    let command = command.trim_start_matches([' ', '\t']);
    let Some((program, rest)) = quote::split_first_word(command) else { return false };
    let raw = &command[..command.len() - rest.len()];
    (program == "sled" || program.ends_with("/sled"))
        && is_only_hook_argument(rest)
        && written_by_sled(raw, &program)
}

/// True when `rest` is one or more spaces or tabs, `hook`, then only spaces or tabs.
fn is_only_hook_argument(rest: &str) -> bool {
    let blank = [' ', '\t'];
    let arg = rest.trim_start_matches(blank);
    arg.len() < rest.len() && arg.trim_end_matches(blank) == "hook"
}

/// True when `raw` is exactly how sled would have written the program path `program`.
fn written_by_sled(raw: &str, program: &str) -> bool {
    let plain = program
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '/' | '~' | '-'));
    let double_quotable = !program.contains(['"', '$', '\\', '`']);
    raw == quote::shell_quote_path(program)
        || (plain && raw == program)
        || (double_quotable && raw == format!("\"{program}\""))
}

/// Ensures each of our three events carries our command. Returns true if anything changed.
pub fn add_hooks(settings: &mut Value, command: &str) -> bool {
    let Some(root) = settings.as_object_mut() else { return false };
    let hooks = root.entry("hooks").or_insert_with(|| Value::Object(Map::new()));
    let Some(hooks) = hooks.as_object_mut() else { return false };
    let mut changed = false;
    for event in EVENTS {
        let entries = hooks.entry(event).or_insert_with(|| Value::Array(Vec::new()));
        let Some(entries) = entries.as_array_mut() else { continue };
        let mut found = false;
        entries.retain_mut(|entry| {
            let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else { return true };
            let n = list.len();
            list.retain_mut(|hook| {
                let current = hook.get("command").and_then(Value::as_str);
                if !current.is_some_and(is_ours) {
                    return true;
                }
                if found {
                    return false; // a duplicate of ours: the first one is kept
                }
                found = true;
                if current != Some(command) {
                    hook["command"] = json!(command);
                    changed = true;
                }
                if let Some(h) = hook.as_object_mut().filter(|h| !h.contains_key("timeout")) {
                    h.insert("timeout".into(), json!(HOOK_TIMEOUT_SECS));
                    changed = true;
                }
                true
            });
            if list.len() == n {
                return true;
            }
            changed = true;
            !list.is_empty()
        });
        if !found {
            entries.push(json!({"hooks":[{"type":"command","command":command,"timeout":HOOK_TIMEOUT_SECS}]}));
            changed = true;
        }
    }
    changed
}

/// Removes only our commands, then prunes containers that this emptied. True if anything changed.
pub fn remove_hooks(settings: &mut Value) -> bool {
    let Some(hooks) = settings.get_mut("hooks").and_then(Value::as_object_mut) else { return false };
    let mut changed = false;
    let mut emptied_events = Vec::new();
    for (event, entries) in hooks.iter_mut() {
        let Some(entries) = entries.as_array_mut() else { continue };
        let before = entries.len();
        let mut touched = false;
        entries.retain_mut(|entry| {
            let Some(list) = entry.get_mut("hooks").and_then(Value::as_array_mut) else { return true };
            let n = list.len();
            list.retain(|h| !h.get("command").and_then(Value::as_str).is_some_and(is_ours));
            if list.len() == n {
                return true;
            }
            touched = true;
            !list.is_empty()
        });
        if touched || entries.len() != before {
            changed = true;
            if entries.is_empty() {
                emptied_events.push(event.clone());
            }
        }
    }
    for event in emptied_events {
        hooks.shift_remove(&event);
    }
    if changed && hooks.is_empty() {
        if let Some(root) = settings.as_object_mut() {
            root.shift_remove("hooks");
        }
    }
    changed
}

/// The command written into settings: the binary path, shell-quoted when needed, then `hook`.
pub fn hook_command(exe: &Path) -> String {
    format!("{} hook", quote::shell_quote_path(&exe.display().to_string()))
}

/// Errors when `hooks` or one of our event values has a shape we cannot add to.
fn check_hook_shapes(settings: &Value, file: &Path) -> Result<()> {
    let Some(hooks) = settings.get("hooks") else { return Ok(()) };
    let Some(hooks) = hooks.as_object() else {
        return Err(anyhow!("{}: hooks is not an object; leaving it untouched", file.display()));
    };
    for event in EVENTS {
        if hooks.get(event).is_some_and(|v| !v.is_array()) {
            return Err(anyhow!("{}: hooks.{event} is not an array; leaving it untouched", file.display()));
        }
    }
    Ok(())
}

/// Loads settings.json: `{}` when missing, an error naming the file when it is not an object.
fn load(file: &Path) -> Result<(Value, bool)> {
    match std::fs::read_to_string(file) {
        Ok(text) => {
            let value: Value = serde_json::from_str(&text)
                .with_context(|| format!("{} is not valid JSON; leaving it untouched", file.display()))?;
            if !value.is_object() {
                return Err(anyhow!("{} is not a JSON object; leaving it untouched", file.display()));
            }
            Ok((value, true))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok((json!({}), false)),
        Err(e) => Err(e).with_context(|| format!("reading {}", file.display())),
    }
}

/// Follows `file` through any symlinks, even when the last target does not exist yet.
fn resolve_symlinks(file: &Path) -> Result<PathBuf> {
    let mut path = file.to_path_buf();
    for _ in 0..40 {
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.file_type().is_symlink() => {
                let next = std::fs::read_link(&path).with_context(|| format!("reading link {}", path.display()))?;
                path = match path.parent() {
                    Some(dir) if next.is_relative() => dir.join(next),
                    _ => next,
                };
            }
            _ => return Ok(path),
        }
    }
    Err(anyhow!("{}: too many levels of symbolic links", file.display()))
}

/// Copies `file` to a new `settings.json.sled-backup-<stamp>` in `dir`, adding `-2`, `-3`, …
/// when the name is taken. Never overwrites an existing backup. The copy keeps the file's mode.
fn backup(file: &Path, dir: &Path) -> Result<PathBuf> {
    let base = format!("settings.json.sled-backup-{}", chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let mode = std::fs::metadata(file)?.permissions();
    for n in 1u32.. {
        let path = if n == 1 { dir.join(&base) } else { dir.join(format!("{base}-{n}")) };
        let mut out = match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e).with_context(|| format!("writing backup {}", path.display())),
        };
        let copied = out
            .set_permissions(mode.clone())
            .and_then(|()| std::io::copy(&mut std::fs::File::open(file)?, &mut out))
            .and_then(|_| out.sync_all());
        return match copied {
            Ok(()) => Ok(path),
            Err(e) => {
                let _ = std::fs::remove_file(&path);
                Err(e).with_context(|| format!("writing backup {}", path.display()))
            }
        };
    }
    unreachable!("ran out of backup names")
}

/// Backs up the existing file (if any), then writes the new content through a temp file, synced
/// to disk, and a rename. A symlinked settings file stays a link: its target is written.
fn save(file: &Path, settings: &Value, existed: bool) -> Result<Option<PathBuf>> {
    let target = resolve_symlinks(file)?;
    let dir = target.parent().ok_or_else(|| anyhow!("{} has no parent folder", target.display()))?;
    if target != file && !dir.is_dir() {
        return Err(anyhow!(
            "{} is a symlink to {}, whose folder does not exist; leaving it untouched",
            file.display(),
            target.display()
        ));
    }
    std::fs::create_dir_all(dir)?;
    let file = target.as_path();
    let backup = if existed { Some(backup(file, dir)?) } else { None };
    let mut text = serde_json::to_string_pretty(settings)?;
    text.push('\n');
    let tmp = dir.join(format!("settings.json.sled-tmp-{}", std::process::id()));
    let mode = if existed { file_mode(file)? } else { NEW_FILE_MODE };
    let written = write_synced(&tmp, text.as_bytes(), mode)
        .and_then(|()| {
            if existed {
                std::fs::set_permissions(&tmp, std::fs::metadata(file)?.permissions())
            } else {
                Ok(())
            }
        })
        .and_then(|()| std::fs::rename(&tmp, file));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| format!("writing {}", file.display()));
    }
    // Best effort: make the rename itself durable.
    #[cfg(unix)]
    let _ = std::fs::File::open(dir).and_then(|d| d.sync_all());
    Ok(backup)
}

/// Mode for a settings file that did not exist: it can hold secrets such as API keys.
const NEW_FILE_MODE: u32 = 0o600;

#[cfg(unix)]
fn file_mode(file: &Path) -> std::io::Result<u32> {
    use std::os::unix::fs::PermissionsExt;
    Ok(std::fs::metadata(file)?.permissions().mode() & 0o7777)
}

#[cfg(not(unix))]
fn file_mode(_file: &Path) -> std::io::Result<u32> {
    Ok(NEW_FILE_MODE)
}

/// Writes `bytes` to a fresh file at `path` and syncs it. On unix the file is created with
/// `mode` (narrowed by the umask), so its content is never readable more widely than that;
/// a stale file left at `path` is removed first so it cannot keep a looser mode.
fn write_synced(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, mode);
    #[cfg(not(unix))]
    let _ = mode;
    let mut f = options.open(path)?;
    f.write_all(bytes)?;
    f.sync_all()
}

/// Applies the retention choice; retention is only ever raised. True if the settings changed.
fn apply_retention(settings: &mut Value, retention: Retention) -> bool {
    let current = match settings.get(RETENTION_KEY) {
        None => None,
        Some(v) => match v.as_u64() {
            Some(n) => Some(n),
            None => return false, // not a number: not ours to touch
        },
    };
    let effective = current.unwrap_or(DEFAULT_RETENTION);
    let wanted = match retention {
        Retention::Keep => return false,
        Retention::Set(n) => u64::from(n),
        Retention::Ask => {
            if effective >= WANTED_RETENTION {
                return false;
            }
            let stdin = std::io::stdin();
            if !stdin.is_terminal() || !confirm_retention(current, &mut std::io::stdout(), &mut stdin.lock()) {
                return false;
            }
            WANTED_RETENTION
        }
    };
    if wanted <= effective {
        println!("Retention left at {} days.", shown_retention(current));
        return false;
    }
    settings[RETENTION_KEY] = json!(wanted);
    true
}

fn shown_retention(current: Option<u64>) -> String {
    current.map_or("30 (default)".to_string(), |n| n.to_string())
}

/// Asks whether to raise retention to 365 days. Only `y` or `yes` (any case) accepts.
pub fn confirm_retention(current: Option<u64>, out: &mut dyn Write, input: &mut dyn BufRead) -> bool {
    let shown = shown_retention(current);
    let _ = write!(
        out,
        "Claude Code deletes session files after {shown} days, after which a session cannot be resumed. \
         Keep them for 365 days instead? This uses more disk. [y/N] "
    );
    let _ = out.flush();
    let mut answer = String::new();
    if input.read_line(&mut answer).is_err() {
        return false;
    }
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

pub fn run(paths: &Paths, exe: &Path, retention: Retention) -> Result<()> {
    let file = paths.settings_file();
    let (mut settings, existed) = load(&file)?;
    check_hook_shapes(&settings, &file)?;
    let mut changed = add_hooks(&mut settings, &hook_command(exe));
    changed |= apply_retention(&mut settings, retention);
    if changed {
        if let Some(backup) = save(&file, &settings, existed)? {
            println!("Backed up settings to {}", backup.display());
        }
    } else {
        println!("Hooks already installed in {}", file.display());
    }

    let report = std::fs::create_dir_all(&paths.data_dir)
        .map_err(anyhow::Error::from)
        .and_then(|()| sweep::run(paths))
        .with_context(|| {
            format!(
                "the hooks were installed in {}, but the first sweep failed; \
                 run \"sled sweep\" to index existing sessions",
                file.display()
            )
        })?;
    println!("Indexed {} sessions ({} new).", report.sessions, report.new);
    println!("Run \"sled serve\" to open the ledger.");
    Ok(())
}

pub fn uninstall(paths: &Paths) -> Result<()> {
    let file = paths.settings_file();
    let (mut settings, existed) = load(&file)?;
    if remove_hooks(&mut settings) {
        if let Some(backup) = save(&file, &settings, existed)? {
            println!("Backed up settings to {}", backup.display());
        }
        println!("Removed the sled hooks from {}", file.display());
    } else {
        println!("No sled hooks found in {}", file.display());
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::write_synced;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn temp_file_is_created_with_the_given_mode() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("t");
        write_synced(&tmp, b"{}", 0o600).unwrap();
        assert_eq!(mode(&tmp), 0o600);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"{}");
    }

    #[test]
    fn stale_temp_file_does_not_keep_its_looser_mode() {
        let dir = tempfile::tempdir().unwrap();
        let tmp = dir.path().join("t");
        std::fs::write(&tmp, "old secret text, longer than the new").unwrap();
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644)).unwrap();
        write_synced(&tmp, b"{}", 0o600).unwrap();
        assert_eq!(mode(&tmp), 0o600);
        assert_eq!(std::fs::read(&tmp).unwrap(), b"{}");
    }
}
