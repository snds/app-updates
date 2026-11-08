//! The local channel's tests: which staged build is an update, the files' schema, the atomic
//! writes, and the install's swap and rollback (with a plain folder copy, on every platform).

use std::cell::Cell;
use std::path::{Path, PathBuf};

use super::*;

const A: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
const B: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn build(commit: &str, time: &str) -> LocalBuild {
    LocalBuild {
        commit: commit.into(),
        commit_time: time.into(),
    }
}

fn staged_json(schema: u32, commit: &str, time: &str) -> String {
    serde_json::json!({
        "schema": schema,
        "app": "Synapse",
        "bundle": "/Users/snds/Projects/synapse/target/release/bundle/macos/Synapse.app",
        "commit": commit,
        "commit_time": time,
        "built_at": "2026-11-08T01:20:00Z",
        "version": "0.1.0",
    })
    .to_string()
}

fn staged(commit: &str, time: &str) -> Staged {
    Staged::parse(&staged_json(1, commit, time)).unwrap()
}

/// A fresh folder for one test, removed when it's dropped.
struct Temp(PathBuf);

impl Temp {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "app-updates-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A small bundle: `Contents/Info.plist` holding `marker`, and `Contents/MacOS/app`.
fn bundle(at: &Path, marker: &str) {
    std::fs::create_dir_all(at.join("Contents/MacOS")).unwrap();
    std::fs::write(at.join("Contents/Info.plist"), marker).unwrap();
    std::fs::write(at.join("Contents/MacOS/app"), b"binary").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("MacOS/app", at.join("Contents/link")).unwrap();
}

fn marker(at: &Path) -> String {
    std::fs::read_to_string(at.join("Contents/Info.plist")).unwrap()
}

fn leftovers(folder: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(folder)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.contains(".new-") || n.contains(".old-") || n.contains(".tmp-"))
        .collect();
    names.sort();
    names
}

fn plain_copy(from: &Path, to: &Path) -> Result<(), String> {
    copy_dir(from, to).map_err(|e| e.to_string())
}

fn no_check(_: &Path, _: &Path) -> Result<(), String> {
    Ok(())
}

#[test]
fn a_staged_build_is_newer_only_with_another_commit_and_a_later_time() {
    let running = build(A, "2026-11-08T01:00:00Z");
    assert!(is_newer_staged(
        &running,
        &staged(B, "2026-11-08T01:00:01Z")
    ));
    assert!(!is_newer_staged(
        &running,
        &staged(A, "2026-11-09T00:00:00Z")
    ));
    assert!(
        !is_newer_staged(&running, &staged(&A.to_uppercase(), "2026-11-09T00:00:00Z")),
        "the same commit in capitals is the same build"
    );
    assert!(!is_newer_staged(
        &running,
        &staged(B, "2026-11-08T01:00:00Z")
    ));
    assert!(!is_newer_staged(
        &running,
        &staged(B, "2026-11-07T23:59:59Z")
    ));
    assert!(is_newer_staged(
        &running,
        &staged(B, "2027-01-01T00:00:00Z")
    ));
    assert!(is_newer_staged(
        &build(A, "2026-12-31T23:59:59Z"),
        &staged(B, "2027-01-01T00:00:00Z")
    ));
    assert!(is_newer_staged(
        &build(A, "2026-11-08T01:00:00Z"),
        &staged(B, "2026-11-08T01:00:00.5Z")
    ));
    assert!(is_newer_staged(
        &build(A, "2026-11-08T01:00:00+00:00"),
        &staged(B, "2026-11-08T01:00:01+00:00")
    ));
    assert!(
        !is_newer_staged(&build(A, "yesterday"), &staged(B, "2026-11-08T01:00:00Z")),
        "a running time that can't be read is never older"
    );
}

#[test]
fn only_utc_times_in_rfc_3339_are_read() {
    for ok in [
        "2026-11-08T01:00:00Z",
        "2026-11-08t01:00:00z",
        "2026-11-08T01:00:00.123456789Z",
        "2026-11-08T01:00:00+00:00",
        "2028-02-29T00:00:00Z",
        " 2026-11-08T01:00:00Z\n",
    ] {
        assert!(utc_time(ok).is_some(), "{ok}");
    }
    for bad in [
        "",
        "2026-11-08",
        "2026-11-08T01:00:00",
        "2026-11-08T01:00:00+01:00",
        "2026-11-08T01:00:00-05:00",
        "2026-13-08T01:00:00Z",
        "2026-02-29T00:00:00Z",
        "2026-11-31T00:00:00Z",
        "2026-11-08T24:00:00Z",
        "2026-11-08T01:60:00Z",
        "2026-11-08T01:00:00.Z",
        "+026-11-08T01:00:00Z",
        "2026-11-08T01:00:00ZZ",
    ] {
        assert!(utc_time(bad).is_none(), "{bad} was read");
    }
}

#[test]
fn staged_json_is_read_only_in_schema_1() {
    let s = staged(A, "2026-11-08T01:00:00Z");
    assert_eq!(s.app, "Synapse");
    assert_eq!(s.version, "0.1.0");
    assert!(s.bundle.ends_with("Synapse.app"));
    assert_eq!(s.label(), "1a2b3c4 (2026-11-08 01:00 UTC)");
    let e = Staged::parse(&staged_json(2, A, "2026-11-08T01:00:00Z")).unwrap_err();
    assert!(e.contains("schema 2"), "{e}");
    assert!(Staged::parse(&staged_json(0, A, "2026-11-08T01:00:00Z")).is_err());
    assert!(Staged::parse(&staged_json(1, "main", "2026-11-08T01:00:00Z")).is_err());
    assert!(Staged::parse(&staged_json(1, "", "2026-11-08T01:00:00Z")).is_err());
    assert!(Staged::parse(&staged_json(1, A, "8 Nov 2026")).is_err());
    assert!(Staged::parse("{").is_err());
    assert!(Staged::parse(r#"{"schema": 1}"#).is_err());
    // A later rebuild may add fields; schema 1 still reads.
    let mut extra: serde_json::Value =
        serde_json::from_str(&staged_json(1, A, "2026-11-08T01:00:00Z")).unwrap();
    extra["signed_by"] = "x".into();
    assert!(Staged::parse(&extra.to_string()).is_ok());
}

#[test]
fn a_missing_staged_file_means_nothing_is_staged() {
    let dir = Temp::new("staged");
    assert_eq!(Staged::read(&dir.0), Ok(None));
    std::fs::write(
        dir.0.join(STAGED_FILE),
        staged_json(1, B, "2026-11-08T01:00:00Z"),
    )
    .unwrap();
    assert_eq!(Staged::read(&dir.0).unwrap().unwrap().commit, B);
    std::fs::write(dir.0.join(STAGED_FILE), "not json").unwrap();
    assert!(Staged::read(&dir.0).is_err());
}

#[test]
fn the_running_record_is_written_whole_and_removed_only_by_its_writer() {
    let root = Temp::new("running");
    let dir = root.0.join("snds/updates/design.snds.sy");
    let record = Running::new(
        &build(A, "2026-11-08T01:00:00Z"),
        Path::new("/Users/snds/Applications/Synapse.app"),
    );
    record.write(&dir).unwrap();
    assert_eq!(Running::read(&dir).unwrap().as_ref(), Some(&record));
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join(RUNNING_FILE)).unwrap()).unwrap();
    assert_eq!(json["schema"], 1);
    assert_eq!(json["pid"], std::process::id());
    assert_eq!(json["bundle"], "/Users/snds/Applications/Synapse.app");
    assert_eq!(json["commit"], A);
    assert_eq!(json["commit_time"], "2026-11-08T01:00:00Z");
    assert_eq!(json["local_updates"], true);
    assert!(leftovers(&dir).is_empty(), "the temporary file was left");
    // Written again over the old one, still whole.
    record.write(&dir).unwrap();
    assert!(leftovers(&dir).is_empty());

    Running::remove(&dir, record.pid + 1).unwrap();
    assert!(
        dir.join(RUNNING_FILE).exists(),
        "another copy's quit removed it"
    );
    Running::remove(&dir, record.pid).unwrap();
    assert!(!dir.join(RUNNING_FILE).exists());
    Running::remove(&dir, record.pid).unwrap();
    assert_eq!(Running::read(&dir), Ok(None));
}

#[test]
fn an_atomic_write_replaces_the_file_in_one_step() {
    let dir = Temp::new("atomic");
    let path = dir.0.join("staged.json");
    std::fs::write(&path, "old").unwrap();
    write_atomic(&path, b"new").unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
    assert!(leftovers(&dir.0).is_empty());
    assert!(write_atomic(&dir.0.join("missing/x.json"), b"x").is_err());
    assert!(leftovers(&dir.0).is_empty());
}

#[test]
fn the_team_is_read_from_codesign_and_none_is_no_team() {
    let info = "Executable=/x\nIdentifier=design.snds.sy\nTeamIdentifier=AB12CD34EF\nSealed Resources version=2\n";
    assert_eq!(team_from(info).as_deref(), Some("AB12CD34EF"));
    assert_eq!(team_from("TeamIdentifier=not set\n"), None);
    assert_eq!(team_from("Identifier=x\n"), None);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn verifying_says_plainly_it_needs_a_mac() {
    let e = verify_staged(Path::new("/a.app"), Path::new("/b.app")).unwrap_err();
    assert_eq!(
        e,
        "Local builds can be checked and installed only on a Mac."
    );
}

#[test]
fn the_install_swaps_the_new_build_in_and_removes_the_old() {
    let dir = Temp::new("swap");
    let staged = dir.0.join("build/Synapse.app");
    let apps = dir.0.join("Applications");
    let installed = apps.join("Synapse.app");
    bundle(&staged, "new");
    bundle(&installed, "old");
    std::fs::write(installed.join("Contents/stale"), b"only in the old one").unwrap();
    install_with(
        &staged,
        &installed,
        &Steps {
            copy: &plain_copy,
            check: &no_check,
            rename: &|a, b| std::fs::rename(a, b),
        },
    )
    .unwrap();
    assert_eq!(marker(&installed), "new");
    assert!(!installed.join("Contents/stale").exists());
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(installed.join("Contents/link")).unwrap(),
        Path::new("MacOS/app")
    );
    assert_eq!(marker(&staged), "new", "the staged build is still there");
    assert!(leftovers(&apps).is_empty(), "{:?}", leftovers(&apps));
}

#[cfg(not(target_os = "macos"))]
#[test]
fn the_kits_own_install_copies_a_folder_off_a_mac() {
    let dir = Temp::new("own");
    let staged = dir.0.join("Synapse-built.app");
    let installed = dir.0.join("Synapse.app");
    bundle(&staged, "new");
    install_staged(&staged, &installed).unwrap();
    assert_eq!(marker(&installed), "new");
    bundle(&dir.0.join("Synapse-next.app"), "next");
    install_staged(&dir.0.join("Synapse-next.app"), &installed).unwrap();
    assert_eq!(marker(&installed), "next");
    assert!(leftovers(&dir.0).is_empty());
}

#[test]
fn a_failed_swap_puts_the_old_app_back() {
    let dir = Temp::new("rollback");
    let staged = dir.0.join("build/Synapse.app");
    let installed = dir.0.join("Synapse.app");
    bundle(&staged, "new");
    bundle(&installed, "old");
    // The first rename (old aside) works, the second (new in) fails, the third (old back) works.
    let calls = Cell::new(0);
    let rename = |a: &Path, b: &Path| {
        calls.set(calls.get() + 1);
        if calls.get() == 2 {
            Err(std::io::Error::other("the disk said no"))
        } else {
            std::fs::rename(a, b)
        }
    };
    let e = install_with(
        &staged,
        &installed,
        &Steps {
            copy: &plain_copy,
            check: &no_check,
            rename: &rename,
        },
    )
    .unwrap_err();
    assert!(e.contains("left as it was"), "{e}");
    assert_eq!(calls.get(), 3);
    assert_eq!(marker(&installed), "old");
    assert!(leftovers(&dir.0).is_empty(), "{:?}", leftovers(&dir.0));
}

#[test]
fn a_copy_that_fails_or_doesnt_check_out_leaves_the_app_untouched() {
    let dir = Temp::new("refuse");
    let staged = dir.0.join("build/Synapse.app");
    let installed = dir.0.join("Synapse.app");
    bundle(&staged, "new");
    bundle(&installed, "old");
    let half_copy = |from: &Path, to: &Path| {
        plain_copy(from, to)?;
        Err("ditto stopped: no space left.".to_string())
    };
    let rename = |a: &Path, b: &Path| std::fs::rename(a, b);
    let e = install_with(
        &staged,
        &installed,
        &Steps {
            copy: &half_copy,
            check: &no_check,
            rename: &rename,
        },
    )
    .unwrap_err();
    assert!(e.starts_with("The new build couldn't be copied"), "{e}");
    let refuse = |_: &Path, _: &Path| Err("The new build was refused.".to_string());
    let e = install_with(
        &staged,
        &installed,
        &Steps {
            copy: &plain_copy,
            check: &refuse,
            rename: &rename,
        },
    )
    .unwrap_err();
    assert_eq!(e, "The new build was refused.");
    assert_eq!(marker(&installed), "old");
    assert!(leftovers(&dir.0).is_empty(), "{:?}", leftovers(&dir.0));
}

#[test]
fn a_first_install_needs_no_old_app_and_a_missing_build_is_refused() {
    let dir = Temp::new("first");
    let staged = dir.0.join("build/Synapse.app");
    let installed = dir.0.join("Synapse.app");
    let steps = Steps {
        copy: &plain_copy,
        check: &no_check,
        rename: &|a, b| std::fs::rename(a, b),
    };
    let e = install_with(&staged, &installed, &steps).unwrap_err();
    assert!(e.starts_with("The new build isn't where"), "{e}");
    bundle(&staged, "new");
    install_with(&staged, &installed, &steps).unwrap();
    assert_eq!(marker(&installed), "new");
    assert!(install_with(&staged, &dir.0.join("nowhere/Synapse.app"), &steps).is_err());
}

#[test]
fn the_updates_folder_is_under_application_support() {
    assert_eq!(
        updates_dir(Path::new("/Users/snds"), "design.snds.sy"),
        Path::new("/Users/snds/Library/Application Support/snds/updates/design.snds.sy")
    );
}
