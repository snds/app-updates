# app-updates

The shared update kit for snds desktop apps. Every app that uses it behaves the same way:

- It checks for a newer build at launch and every six hours. A **Check automatically** switch turns
  this off.
- **Check for updates** checks now (Settings, the command palette, and on macOS the app menu).
- A newer build downloads quietly. Its signature is checked against the app's own key.
- **Restart to update** installs it and opens the app again. An update you don't take installs when
  you quit.
- **Reload app** closes the app and opens it again.
- Where the app can't replace itself (a Linux package, a Windows install for every user, a Mac
  folder you can't write to), it links to the download instead.

The kit was extracted from Lockdown's updater (slice 59). The plan behind it is the unified updates
proposal, approved 2026-11-08 (U1–U6).

## What's in it

| Part | Where | Needs a window |
|---|---|---|
| The rules: channels, version order, allowed hosts, keys, the status the page shows | `src/lib.rs` | No |
| The local channel: `staged.json` and `running.json`, which build is newer, the signature check, the install | `src/local.rs` | No |
| The Tauri glue: the plugin, the background check, the commands, the menu item, install on quit, reload | `src/shell.rs` (`tauri` feature) | Yes |
| The release workflow (stamp the version, sign, write `latest.json`, publish) | to come (U6 step 3) | — |

## Using it in an app

```toml
[dependencies]
snds-app-updates = { git = "https://github.com/snds/app-updates", tag = "v0.2.0", features = ["tauri"] }
```

1. Describe the app's feed:

   ```rust
   const FEED: app_updates::Feed = app_updates::Feed {
       app: "Vale",
       main_endpoint: "https://github.com/snds/releases/releases/download/vale-main/latest.json",
       main_page: "https://github.com/snds/releases/releases/tag/vale-main",
       stable_endpoint: None,
       stable_page: None,
       hosts: &app_updates::GITHUB_HOSTS,
   };
   ```

2. Implement `shell::Host`: where the **Check automatically** switch is saved, and anything the app
   must do before it closes (lock a vault, stop a background service).
3. Wire four places: `.plugin(shell::plugin())`, `shell::start(handle, config, host)` in `setup`,
   `shell::on_exit(handle)` on `RunEvent::Exit`, and the commands: either the kit's
   `shell::commands::*` (errors as plain strings) or your own wrappers around `shell::status`,
   `check_now`, `restart`, `set_auto_check` and `reload`.
4. In `tauri.conf.json`, set `plugins.updater.requireSignedVersion` to `true` and, for Windows,
   `installMode` to `passive`. Don't grant any updater permission to the page: the kit drives the
   plugin from Rust.

The page hears every change on the app's event (`"<app>://update"`) as an `UpdateStatus`.

## Local channel

Until an app has a paid Apple membership, its updates come from the Mac's own rebuild instead of a
web feed (the unified updates proposal, U4). The rebuild builds and signs the app, then writes
`staged.json` in the app's updates folder,
`~/Library/Application Support/snds/updates/<bundle id>/` (`local::updates_dir`). The running app
reads it and offers **Restart to update**. The person chooses when.

Give `Config` a `local` source to use it:

```rust
local: Some(app_updates::shell::LocalSource {
    updates_dir: app_updates::local::updates_dir(&home, "design.snds.sy"),
    // Built in by the app's build.rs: the commit and its committer time in UTC.
    build: app_updates::local::LocalBuild { commit: COMMIT.into(), commit_time: COMMIT_TIME.into() },
    installed_bundle: home.join("Applications/Synapse.app"),
}),
```

With it:

- **At launch** the app writes `running.json` (its pid, the bundle it runs from, its commit, and
  `local_updates: true`). The rebuild sees it and leaves the app running. The app removes it on a
  clean quit.
- **A check** reads `staged.json`. No file means up to date. A build is newer only when its commit
  differs and its commit time is later. A newer build must pass `codesign --verify --deep --strict`
  and be signed by the same team as the running app; then it's ready at once, with nothing to
  download. The status shows `channel: "local"` and the build as `1a2b3c4 (2026-11-08 01:00 UTC)`.
  Anything wrong shows as a failed check, in plain words.
- **Automatically** the app checks at launch and then every five minutes, the rebuild's own timer,
  while **Check automatically** is on. **Check for updates** reads the file at once.
- **Restart to update**, **Reload app** with a build ready, and quitting all install it with
  `local::install_staged`: `ditto` copies the build beside the installed app, the copy's signature
  is checked again, the old app is renamed aside, the copy is renamed into place, and the old one is
  removed. If the copy can't go in, the old app is put back. Quitting installs without reopening.
- Off a Mac the check fails with "Local builds can be checked and installed only on a Mac."

Without `local`, the web feed works exactly as before.

## Keys

Each app has its own minisign key (U3). Make it once with `tauri signer generate`, keep the secret
half in your password manager and the app's CI secrets, and commit only the public half.

## Test-only source

The `update-test` feature lets an end-to-end run point an app at a local feed through
`<PREFIX>_UPDATE_TEST_ENDPOINT` and `<PREFIX>_UPDATE_TEST_PUBKEY`. An app turns it on only in its
CI update jobs, and refuses it in a release build.

## Checks

```sh
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --features tauri,update-test,test-runtime
```
