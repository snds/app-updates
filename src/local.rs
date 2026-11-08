//! The local channel: a build made on this Mac by its own rebuild, offered to the running app
//! (the unified updates proposal, U4). Until an app has a paid Apple membership, its update
//! channel is the Mac's own rebuild. The rebuild writes `staged.json` in the app's updates folder
//! once a build is made and signed; the running app reads it, checks the build's signature, and
//! offers "Restart to update". The app writes `running.json` there at launch and removes it on a
//! clean quit, so the rebuild knows the app is listening and doesn't quit and reopen it.
//!
//! This part needs no window and is tested on every platform:
//!
//! - [`LocalBuild`]: the running build's identity (its commit and commit time, built in).
//! - [`Staged`]: `staged.json`, schema 1. [`is_newer_staged`]: whether it is an update.
//! - [`Running`]: `running.json`, written and removed atomically.
//! - [`verify_staged`]: the staged bundle's signature, and its team, on macOS.
//! - [`install_staged`]: copies the staged bundle over the installed one, so that a crash at any
//!   step leaves a working app.
//!
//! The window's half is in [`crate::shell`] (`Config::local`).

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// The rebuild's file: a newer build is ready to install.
pub const STAGED_FILE: &str = "staged.json";

/// The app's file: the app is running and listens for staged builds.
pub const RUNNING_FILE: &str = "running.json";

/// The one schema of both files this kit reads and writes.
pub const SCHEMA: u32 = 1;

/// How often a running app re-reads `staged.json`, with the automatic check on. The rebuild runs
/// every five minutes, so the app looks as often.
pub const LOCAL_CHECK_EVERY: Duration = Duration::from_secs(5 * 60);

/// The updates folder for the app with bundle identifier `app_id`, under the home folder `home`:
/// `~/Library/Application Support/snds/updates/<app-id>/`.
pub fn updates_dir(home: &Path, app_id: &str) -> PathBuf {
    home.join("Library")
        .join("Application Support")
        .join("snds")
        .join("updates")
        .join(app_id)
}

/// The running build's identity, built into the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalBuild {
    /// The commit the build was made from (hex).
    pub commit: String,
    /// That commit's committer time, in UTC (`YYYY-MM-DDTHH:MM:SSZ`).
    pub commit_time: String,
}

/// `staged.json`: a build the rebuild made and signed, ready to install.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Staged {
    pub schema: u32,
    /// The app's name, as a person reads it.
    pub app: String,
    /// The built bundle (`…/target/release/bundle/macos/<App>.app`).
    pub bundle: PathBuf,
    /// The commit it was built from (hex).
    pub commit: String,
    /// That commit's committer time, in UTC.
    pub commit_time: String,
    /// When the build finished, in UTC.
    pub built_at: String,
    /// The app's version.
    pub version: String,
}

impl Staged {
    /// Reads `staged.json` text. Only schema 1 is read; a commit that isn't hex or a time that
    /// isn't RFC 3339 in UTC is refused.
    pub fn parse(text: &str) -> Result<Self, String> {
        let staged: Self = serde_json::from_str(text)
            .map_err(|_| "The staged build's description couldn't be read.".to_string())?;
        if staged.schema != SCHEMA {
            return Err(format!(
                "The staged build's description is in a form this app doesn't read (schema {}).",
                staged.schema
            ));
        }
        if !commit_is_valid(&staged.commit) {
            return Err("The staged build's description names no commit.".into());
        }
        if utc_time(&staged.commit_time).is_none() {
            return Err("The staged build's description has no readable commit time.".into());
        }
        Ok(staged)
    }

    /// Reads `<dir>/staged.json`; `None` when there isn't one (nothing staged).
    pub fn read(dir: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(dir.join(STAGED_FILE)) {
            Ok(text) => Self::parse(&text).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!(
                "The staged build's description couldn't be read: {e}."
            )),
        }
    }

    /// A short name for the build, as the page shows it: the commit's first seven characters and
    /// its time (`"1a2b3c4 (2026-11-08 01:00 UTC)"`).
    pub fn label(&self) -> String {
        let short: String = self.commit.chars().take(7).collect();
        match utc_time(&self.commit_time) {
            Some(t) => format!(
                "{short} ({:04}-{:02}-{:02} {:02}:{:02} UTC)",
                t.0[0], t.0[1], t.0[2], t.0[3], t.0[4]
            ),
            None => short,
        }
    }
}

/// Whether the staged build is an update for the running one: the commits differ and the staged
/// commit is later. A time that can't be read is never later.
pub fn is_newer_staged(running: &LocalBuild, staged: &Staged) -> bool {
    if running
        .commit
        .trim()
        .eq_ignore_ascii_case(staged.commit.trim())
    {
        return false;
    }
    match (
        utc_time(&running.commit_time),
        utc_time(&staged.commit_time),
    ) {
        (Some(now), Some(then)) => then > now,
        _ => false,
    }
}

fn commit_is_valid(commit: &str) -> bool {
    (7..=64).contains(&commit.len()) && commit.bytes().all(|b| b.is_ascii_hexdigit())
}

/// A UTC time, comparable: year, month, day, hour, minute, second, then nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct UtcTime([u32; 7]);

/// Reads RFC 3339 in UTC: `YYYY-MM-DDTHH:MM:SS`, optional fractional seconds, then `Z` or
/// `+00:00`. Any other offset is refused: both files carry UTC.
fn utc_time(s: &str) -> Option<UtcTime> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    if b[13] != b':' || b[16] != b':' {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<u32> {
        let part = s.get(from..to)?;
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())?
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    let mut nanos = 0;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let kept = &frac[..digits.min(9)];
        nanos = kept.parse::<u32>().ok()? * 10u32.pow(9 - kept.len() as u32);
        rest = &frac[digits..];
    }
    if !matches!(rest, "Z" | "z" | "+00:00") {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if day == 0 || day > days || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some(UtcTime([year, month, day, hour, minute, second, nanos]))
}

/// `running.json`: this app is running, from `bundle`, and listens for staged builds.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Running {
    pub schema: u32,
    pub pid: u32,
    /// The bundle the app is running from.
    pub bundle: PathBuf,
    pub commit: String,
    pub commit_time: String,
    /// Whether the app picks up staged builds itself (always true from this kit). The rebuild
    /// treats the app as listening only when this is true and `pid` is alive.
    pub local_updates: bool,
}

impl Running {
    /// The record for this process, running `build` from `bundle`.
    pub fn new(build: &LocalBuild, bundle: &Path) -> Self {
        Self {
            schema: SCHEMA,
            pid: std::process::id(),
            bundle: bundle.to_path_buf(),
            commit: build.commit.clone(),
            commit_time: build.commit_time.clone(),
            local_updates: true,
        }
    }

    /// Writes `<dir>/running.json` atomically, making the folder if it isn't there.
    pub fn write(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("The updates folder couldn't be made: {e}."))?;
        let text = serde_json::to_vec_pretty(self)
            .map_err(|e| format!("The running record couldn't be written: {e}."))?;
        write_atomic(&dir.join(RUNNING_FILE), &text)
            .map_err(|e| format!("The running record couldn't be written: {e}."))
    }

    /// Reads `<dir>/running.json`; `None` when there isn't one.
    pub fn read(dir: &Path) -> Result<Option<Self>, String> {
        match std::fs::read_to_string(dir.join(RUNNING_FILE)) {
            Ok(text) => serde_json::from_str(&text)
                .map(Some)
                .map_err(|_| "The running record couldn't be read.".to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(format!("The running record couldn't be read: {e}.")),
        }
    }

    /// Removes `<dir>/running.json` if this process wrote it (its `pid` is `pid`), so a second
    /// copy of the app quitting doesn't remove the first one's record. A record that can't be read
    /// is removed too. Nothing there is fine.
    pub fn remove(dir: &Path, pid: u32) -> Result<(), String> {
        if let Ok(Some(other)) = Self::read(dir)
            && other.pid != pid
        {
            return Ok(());
        }
        match std::fs::remove_file(dir.join(RUNNING_FILE)) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("The running record couldn't be removed: {e}.")),
        }
    }
}

/// Writes `path` all at once: a temporary file in the same folder, flushed, then renamed over it.
/// A reader sees the old file or the new one, never part of one.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let folder = path.parent().unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = folder.join(format!(".{name}.tmp-{}", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    written
}

/// Checks the staged bundle before it's offered: on macOS, `codesign --verify --deep --strict`
/// passes on it, and it's signed by the same team as the running bundle. A bundle with no team
/// (ad hoc or self-signed) is refused. Elsewhere there's no local channel, and this says so.
pub fn verify_staged(staged_bundle: &Path, running_bundle: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        mac::verify(staged_bundle)?;
        let theirs = mac::team(staged_bundle)?;
        let ours = mac::team(running_bundle)?;
        if theirs != ours {
            return Err(format!(
                "The new build was refused: it's signed by team {theirs}, and this app by team {ours}."
            ));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (staged_bundle, running_bundle);
        Err("Local builds can be checked and installed only on a Mac.".into())
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::path::Path;
    use std::process::Command;

    pub fn verify(bundle: &Path) -> Result<(), String> {
        if !bundle.is_dir() {
            return Err(format!(
                "The new build isn't where its description says ({}).",
                bundle.display()
            ));
        }
        let out = Command::new("/usr/bin/codesign")
            .args(["--verify", "--deep", "--strict"])
            .arg(bundle)
            .output()
            .map_err(|e| format!("The new build's signature couldn't be checked: {e}."))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            let why = why.lines().last().unwrap_or("").trim();
            return Err(format!(
                "The new build was refused: its signature didn't check out ({why})."
            ));
        }
        Ok(())
    }

    pub fn team(bundle: &Path) -> Result<String, String> {
        let out = Command::new("/usr/bin/codesign")
            .args(["-dv", "--verbose=2"])
            .arg(bundle)
            .output()
            .map_err(|e| format!("The build's signature couldn't be read: {e}."))?;
        let info = String::from_utf8_lossy(&out.stderr);
        match super::team_from(&info) {
            Some(team) => Ok(team),
            None => Err(format!(
                "{} isn't signed by a team, so a new build can't be matched to it.",
                bundle.display()
            )),
        }
    }
}

/// The `TeamIdentifier=` line of `codesign -dv` output, when it names a team.
#[cfg_attr(not(any(test, target_os = "macos")), allow(dead_code))]
fn team_from(codesign_info: &str) -> Option<String> {
    codesign_info
        .lines()
        .find_map(|l| l.trim().strip_prefix("TeamIdentifier="))
        .map(str::trim)
        .filter(|t| !t.is_empty() && *t != "not set")
        .map(str::to_string)
}

/// The steps [`install_staged`] takes, so the swap and its rollback can be tested anywhere.
pub(crate) struct Steps<'a> {
    /// Copies a bundle to a new path (macOS: `ditto`).
    pub copy: &'a dyn Fn(&Path, &Path) -> Result<(), String>,
    /// Checks the copy before it goes in (macOS: [`verify_staged`] against the installed app).
    pub check: &'a dyn Fn(&Path, &Path) -> Result<(), String>,
    pub rename: &'a dyn Fn(&Path, &Path) -> std::io::Result<()>,
}

/// Installs the staged bundle over the installed one: copies it to `<installed>.new-<pid>`
/// (`ditto` on macOS), checks the copy's signature, renames the installed bundle to
/// `<installed>.old-<pid>`, renames the copy into place, then removes the old one. If the copy
/// can't go in, the old one is put back. At every step there is a whole app at `installed` or at
/// `<installed>.old-<pid>`.
pub fn install_staged(staged_bundle: &Path, installed_bundle: &Path) -> Result<(), String> {
    install_with(
        staged_bundle,
        installed_bundle,
        &Steps {
            copy: &copy_bundle,
            check: &check_copy,
            rename: &|from, to| std::fs::rename(from, to),
        },
    )
}

pub(crate) fn install_with(staged: &Path, installed: &Path, steps: &Steps) -> Result<(), String> {
    if !staged.is_dir() {
        return Err(format!(
            "The new build isn't where its description says ({}).",
            staged.display()
        ));
    }
    let folder = installed
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    if !folder.is_dir() {
        return Err(format!(
            "The app's folder isn't there ({}).",
            folder.display()
        ));
    }
    let sibling = |tag: &str| {
        let mut name = installed.file_name().unwrap_or_default().to_os_string();
        name.push(format!(".{tag}-{}", std::process::id()));
        folder.join(name)
    };
    let new = sibling("new");
    let old = sibling("old");
    let _ = remove_any(&new);
    if let Err(e) = (steps.copy)(staged, &new) {
        let _ = remove_any(&new);
        return Err(format!("The new build couldn't be copied: {e}"));
    }
    if let Err(e) = (steps.check)(&new, installed) {
        let _ = remove_any(&new);
        return Err(e);
    }
    let had_old = installed.symlink_metadata().is_ok();
    if had_old {
        let _ = remove_any(&old);
        if let Err(e) = (steps.rename)(installed, &old) {
            let _ = remove_any(&new);
            return Err(format!("The installed app couldn't be moved aside: {e}."));
        }
    }
    if let Err(e) = (steps.rename)(&new, installed) {
        let back = if had_old {
            (steps.rename)(&old, installed)
        } else {
            Ok(())
        };
        let _ = remove_any(&new);
        return Err(match back {
            Ok(()) => format!("The new build couldn't go in, so the app was left as it was: {e}."),
            Err(b) => format!(
                "The new build couldn't go in ({e}), and the old app couldn't be put back ({b}): it's at {}.",
                old.display()
            ),
        });
    }
    if had_old {
        let _ = remove_any(&old);
    }
    Ok(())
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Copies a bundle: `ditto` on macOS (it keeps signatures, symlinks and extended attributes),
/// a plain folder copy elsewhere.
fn copy_bundle(from: &Path, to: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("/usr/bin/ditto")
            .arg(from)
            .arg(to)
            .output()
            .map_err(|e| format!("ditto didn't run: {e}."))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "ditto stopped: {}.",
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        copy_dir(from, to).map_err(|e| format!("{e}."))
    }
}

/// On macOS the copy must verify and match the installed app's team before it replaces it, so a
/// bundle rewritten by a rebuild since the check never goes in half-made.
fn check_copy(copy: &Path, installed: &Path) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        if installed.is_dir() {
            verify_staged(copy, installed)
        } else {
            mac::verify(copy)
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (copy, installed);
        Ok(())
    }
}

/// A plain recursive copy that keeps symlinks as symlinks (on Unix).
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub(crate) fn copy_dir(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &target)?;
            #[cfg(not(unix))]
            std::fs::copy(entry.path(), &target).map(|_| ())?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
