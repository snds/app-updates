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
| The release workflow (stamp the version, sign, write `latest.json`, publish) | `.github/workflows/release-main.yml`, `release/` | — |
| The restart helper for servers on a Docker host (one-click Update and Reload, U5), and the client the servers call it with | `host-helper/` (its own crate) | — |

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

## Releasing

Every push to an app's `main` can become its newest test build. The kit's reusable workflow
builds the app, signs the update bundles with the app's own key, writes `latest.json`, and puts
them on the app's rolling pre-release `<app>-main` in [snds/releases](https://github.com/snds/releases).
The app's feed points there (`main_endpoint` and `main_page` above).

The version is the app's own (`tauri.conf.json`) plus the commit time in UTC:
`0.1.0-main.20261108010203`. Each later merge gets a higher number. The workflow refuses to
publish a version that isn't newer than the one already there, so a late run never takes the
app back.

### Calling it

Add a job to the app's CI. Pin the workflow and `kit-ref` to the same revision:

```yaml
on:
  push:
    branches: [main]

jobs:
  release-main:
    permissions:
      contents: read
    uses: snds/app-updates/.github/workflows/release-main.yml@<kit commit>
    with:
      app: vale                        # the release is vale-main
      tauri-dir: apps/web/src-tauri    # the folder with tauri.conf.json
      platforms: '["macos-universal", "windows-x64"]'   # add "linux-x64" to build the deb
      package-manager: pnpm            # or npm
      kit-ref: <kit commit>
    secrets:
      TAURI_SIGNING_PRIVATE_KEY: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY }}
      TAURI_SIGNING_PRIVATE_KEY_PASSWORD: ${{ secrets.TAURI_SIGNING_PRIVATE_KEY_PASSWORD }}
      RELEASES_TOKEN: ${{ secrets.RELEASES_TOKEN }}
```

Other inputs, all optional: `working-directory` (where the install and the Tauri CLI run,
default the repository root), `install-command` (default `pnpm install --frozen-lockfile` or
`npm ci`), `tauri-command` (default `pnpm tauri` or `npx tauri`), `build-args` (extra
`tauri build` arguments), `node-version` (default 22), `linux-packages` (extra apt packages),
and `publish: false` to make every run a dry run.

It publishes only on a push to the default branch, with all three secrets set and the app's
`updater.pub` committed. Anything else is a **dry run**: the same builds, signed with a
throwaway key, and the would-be `latest.json` kept as the run's `latest-json` artifact. The run's
summary says what is missing. A dry run never fails a push for a missing secret, so the job can
go in before the keys exist.

macOS builds are signed ad hoc (there's no Developer ID yet), so macOS asks before a first open
of a download. Updates are protected by the updater's own signature, not by Apple's.

### What to set up once per app

1. **Make the key pair.** In the app's folder: `npx tauri signer generate -w ~/.tauri/<app>.key`. Give
   it a password, or press Enter for none. Keep the private key (and its password, if any) in your
   password manager.
2. **Commit the public key** as `<tauri dir>/updater.pub` (the contents of
   `~/.tauri/<app>.key.pub`). The app's `build.rs` builds it in. Without it, updates are off in
   that build.
3. **Add two secrets on the app's repository** (Settings → Secrets and variables → Actions):
   `TAURI_SIGNING_PRIVATE_KEY` (the contents of `~/.tauri/<app>.key`) and, only if the key has a
   password, `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`. A key made without a password needs no
   password secret; if one is set anyway, it must be empty or signing fails with "incorrect updater
   private key password".
4. **Add `RELEASES_TOKEN`** on the app's repository too. It is a fine-grained personal access
   token (GitHub → Settings → Developer settings → Fine-grained tokens) with access to
   **snds/releases only** and the permission **Contents: Read and write**. Nothing else. One
   token can serve every app; store it as a secret on each app repository that publishes.

snds/releases needs at least one commit (a README is enough) before its first release can be
made.

The scripts behind the workflow are in `release/`: `stamp-version.py` (the version into
`tauri.conf.json`, the app crate and `Cargo.lock`, and the front end's `package.json`) and
`write-manifest.py` (`latest.json`, with its signature and never-backwards checks). Their tests:
`python3 release/test_release.py`.

## Keys

Each app has its own minisign key (U3). Make it once with `tauri signer generate`, keep the secret
half in your password manager and the app's CI secrets, and commit only the public half.

## Test-only source

The `update-test` feature lets an end-to-end run point an app at a local feed through
`<PREFIX>_UPDATE_TEST_ENDPOINT` and `<PREFIX>_UPDATE_TEST_PUBKEY`. An app turns it on only in its
CI update jobs, and refuses it in a release build.

## Servers: the host helper

Servers (the Lockdown server, the Vale gateway, later Warpcore) can't replace their own image,
and must never hold the Docker socket. `host-helper/` is a small service for the Docker host
that does it for them: a server signs a request with its own key, and the helper pulls an
allowed image at an allowed tag and recreates the allowed services, or restarts them, and
nothing else. It ships as `ghcr.io/snds/app-updates-host-helper`, with a client the servers use
(`default-features = false`). What it does, its threat model and the Unraid install steps:
[host-helper/README.md](host-helper/README.md).

## Checks

```sh
python3 release/test_release.py
cargo fmt --all --check
cargo clippy --all-targets -- -D warnings
cargo test
cargo test --features tauri,update-test,test-runtime
```

The host helper's own checks are in [host-helper/README.md](host-helper/README.md#checks).
