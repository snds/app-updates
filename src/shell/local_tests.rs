//! The local channel's check in the shell (`find_local`): what `staged.json` says, before the
//! signature check (which needs a Mac).

use std::path::PathBuf;

use super::{LocalSource, find_local};
use crate::local::{LocalBuild, STAGED_FILE};

const RUNNING: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";
const NEWER: &str = "fedcba9876543210fedcba9876543210fedcba98";

fn source(name: &str) -> LocalSource {
    let dir = std::env::temp_dir().join(format!("app-updates-shell-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    LocalSource {
        updates_dir: dir.clone(),
        build: LocalBuild {
            commit: RUNNING.into(),
            commit_time: "2026-11-08T01:00:00Z".into(),
        },
        installed_bundle: PathBuf::from("/Users/snds/Applications/Synapse.app"),
    }
}

fn stage(local: &LocalSource, app: &str, commit: &str, time: &str) {
    let text = serde_json::json!({
        "schema": 1, "app": app, "bundle": local.updates_dir.join("missing.app"),
        "commit": commit, "commit_time": time,
        "built_at": "2026-11-08T02:00:00Z", "version": "0.1.0",
    });
    std::fs::write(local.updates_dir.join(STAGED_FILE), text.to_string()).unwrap();
}

#[test]
fn nothing_staged_the_same_build_or_an_older_one_is_up_to_date() {
    let local = source("uptodate");
    assert_eq!(find_local("Synapse", &local), Ok(None));
    stage(&local, "Synapse", RUNNING, "2026-11-09T00:00:00Z");
    assert_eq!(find_local("Synapse", &local), Ok(None));
    stage(&local, "Synapse", NEWER, "2026-11-07T00:00:00Z");
    assert_eq!(find_local("Synapse", &local), Ok(None));
    let _ = std::fs::remove_dir_all(&local.updates_dir);
}

#[test]
fn a_newer_build_must_be_for_this_app_and_must_check_out() {
    let local = source("newer");
    stage(&local, "Lockdown", NEWER, "2026-11-08T01:30:00Z");
    let e = find_local("Synapse", &local).unwrap_err();
    assert_eq!(e, "The staged build is for Lockdown, not Synapse.");
    stage(&local, "Synapse", NEWER, "2026-11-08T01:30:00Z");
    // The staged bundle isn't there: refused on a Mac, and off a Mac nothing is checked at all.
    assert!(find_local("Synapse", &local).is_err());
    std::fs::write(local.updates_dir.join(STAGED_FILE), r#"{"schema": 2}"#).unwrap();
    assert!(find_local("Synapse", &local).is_err());
    let _ = std::fs::remove_dir_all(&local.updates_dir);
}
