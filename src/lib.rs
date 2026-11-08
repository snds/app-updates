//! The shared update kit for snds desktop apps (the unified updates proposal, U2, 2026-11-08).
//!
//! Every app publishes signed builds to a feed: a channel's `latest.json` manifest and its bundles,
//! on GitHub Releases (`snds/releases` for private apps, U1). The app follows its feed: it checks
//! at launch and every six hours (or when asked), downloads a newer build in the background,
//! checks its minisign signature against the app's own public key (one key per app, U3), and
//! offers "Restart to update". An update not taken installs when the app quits. Every app also
//! offers "Check for updates" and "Reload app".
//!
//! Extracted from Lockdown's updater (`apps/desktop/src-tauri/src/updater.rs`, slice 59), with
//! Lockdown's addresses, hosts and name made into a [`Feed`]. This file is the part that needs no
//! window, tested on every platform:
//!
//! - [`Channel`]: which feed a build follows, read from its own version (`x.y.z-main.<commit
//!   time>` follows "main"; a plain version follows "stable").
//! - [`plan`]: whether this build checks at all, and where: the channel's endpoint, the key, and
//!   the hosts a download may come from.
//! - [`allowed`]: a manifest or bundle URL must be `https` on the feed's hosts, with no user
//!   name, password or other port.
//! - [`is_newer`]: only a strictly higher version is an update.
//! - [`UpdateStatus`]: what Settings and the menu show.
//!
//! The local channel (builds made on this Mac by its own rebuild, U4) is in [`local`]: also no
//! window, tested on every platform.
//!
//! The window's half (the plugin, the commands, the background check, the menu item, installing
//! on quit, reloading) is in [`shell`], behind the `tauri` feature.

use std::time::Duration;

use semver::Version;
use serde::Serialize;

/// The hosts a GitHub release download comes from: the manifest and the release page on
/// `github.com`, and the asset hosts a release download is redirected to. Every redirect is
/// checked against the same list.
pub const GITHUB_HOSTS: [&str; 4] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

/// How often a running app checks, with the automatic check on.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// How long a check or a download may take before it counts as failed.
pub const NETWORK_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// Where one app's builds are published, and what it calls itself in the words it shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Feed {
    /// The app's name, as a person reads it ("Lockdown").
    pub app: &'static str,
    /// The "main" channel's manifest (every merge to `main`).
    pub main_endpoint: &'static str,
    /// The page to send people to for a main build.
    pub main_page: &'static str,
    /// The "stable" channel's manifest, once tagged releases are built.
    pub stable_endpoint: Option<&'static str>,
    pub stable_page: Option<&'static str>,
    /// The hosts the manifest and the bundles may come from.
    pub hosts: &'static [&'static str],
}

/// The feed a build follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Channel {
    /// Every merge to `main`: versions `x.y.z-main.<YYYYMMDDHHMMSS>`.
    Main,
    /// Tagged releases (`v*`).
    Stable,
    /// Builds made on this computer by its own rebuild ([`local`]), offered through
    /// `staged.json` rather than a web feed.
    Local,
}

impl Channel {
    /// The web channel a build of `version` follows. (A build on the local channel says so in
    /// its [`shell`] config instead; its version doesn't.)
    pub fn of(version: &Version) -> Self {
        let pre = version.pre.as_str();
        if pre == "main" || pre.starts_with("main.") {
            Self::Main
        } else {
            Self::Stable
        }
    }

    /// Where the channel's manifest is; `None` while the channel isn't built.
    pub fn endpoint(self, feed: &Feed) -> Option<&'static str> {
        match self {
            Self::Main => Some(feed.main_endpoint),
            Self::Stable => feed.stable_endpoint,
            Self::Local => None,
        }
    }

    /// The release page for the channel.
    pub fn page(self, feed: &Feed) -> Option<&'static str> {
        match self {
            Self::Main => Some(feed.main_page),
            Self::Stable => feed.stable_page,
            Self::Local => None,
        }
    }
}

/// Whether `offered` is an update for a build at `current`: strictly higher in semver order,
/// where a pre-release's numeric fields compare as numbers (`main.20261107093000` <
/// `main.20261107120000`) and a release sorts after its own pre-releases (`0.2.0-main.…` <
/// `0.2.0`). The same version, an older one, or a garbled one is never an update.
pub fn is_newer(current: &Version, offered: &Version) -> bool {
    offered > current
}

/// Whether `key` is a minisign public key in the form the updater reads: base64 of the two-line
/// text `tauri signer generate` writes.
pub fn key_is_valid(key: &str) -> bool {
    let Ok(text) = data_encoding::BASE64.decode(key.trim().as_bytes()) else {
        return false;
    };
    let Ok(text) = String::from_utf8(text) else {
        return false;
    };
    minisign_verify::PublicKey::decode(&text).is_ok()
}

/// The key's minisign ID, as `minisign` and `tauri signer` print it (sixteen hex digits), read
/// from the key itself rather than its comment.
pub fn key_id(key: &str) -> Option<String> {
    let text = data_encoding::BASE64.decode(key.trim().as_bytes()).ok()?;
    let text = String::from_utf8(text).ok()?;
    minisign_verify::PublicKey::decode(&text).ok()?;
    let raw = data_encoding::BASE64
        .decode(text.lines().nth(1)?.trim().as_bytes())
        .ok()?;
    // Two bytes of algorithm, eight of key ID (little-endian), 32 of key.
    let id = raw.get(2..10)?;
    Some(id.iter().rev().map(|b| format!("{b:02X}")).collect())
}

/// Checks that `url` is somewhere an update may come from: `https` on one of `hosts` with the
/// default port and no user name or password. A test source (the `update-test` feature) may also
/// be plain `http` on this computer's loopback address.
pub fn allowed(url: &str, hosts: &[String]) -> Result<(), String> {
    let parsed = url::Url::parse(url).map_err(|_| "the update address isn't a web address")?;
    let host = parsed
        .host_str()
        .ok_or("the update address has no host")?
        .to_ascii_lowercase();
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err("the update address carries a user name".into());
    }
    if !hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Err(format!("updates don't come from {host}"));
    }
    let loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]");
    match parsed.scheme() {
        "https" if parsed.port().is_none() || loopback => Ok(()),
        "http" if loopback && cfg!(any(test, feature = "update-test")) => Ok(()),
        "https" => Err(format!("updates from {host} use its usual port")),
        _ => Err("updates come only over https".into()),
    }
}

/// Why a build doesn't check for updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Off {
    /// Built without its update key.
    NoKey,
    /// The key was there but isn't a minisign public key.
    BadKey,
    /// A tagged release, and the app's stable channel isn't built yet.
    StableNotBuilt,
}

impl Off {
    /// The one line Settings shows.
    pub fn words(self) -> &'static str {
        match self {
            Self::NoKey => "Updates are off in this build: it was made without the update key.",
            Self::BadKey => "Updates are off in this build: its update key isn't valid.",
            Self::StableNotBuilt => {
                "Updates are off in this build: automatic updates for tagged releases come later."
            }
        }
    }
}

/// Where a build checks, and what it trusts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// The manifest.
    pub endpoint: String,
    /// The minisign public key (base64), as the updater reads it.
    pub pubkey: String,
    /// The hosts the manifest and the bundle may come from.
    pub hosts: Vec<String>,
    /// The page to send people to where the app can't replace itself.
    pub page: String,
}

/// The test-only endpoint and key (`update-test` builds): an end-to-end run serves build B from
/// this computer, signed with a throwaway key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestSource {
    pub endpoint: String,
    pub pubkey: String,
}

impl TestSource {
    /// `<PREFIX>_UPDATE_TEST_ENDPOINT` and `<PREFIX>_UPDATE_TEST_PUBKEY`, both set, in a build with
    /// the kit's `update-test` feature; never otherwise. An app keeps its own test marker beside the
    /// call, so its release check can find the marker and refuse a build that has the override.
    pub fn from_env(prefix: &str) -> Option<Self> {
        #[cfg(feature = "update-test")]
        {
            let endpoint = std::env::var(format!("{prefix}_UPDATE_TEST_ENDPOINT")).ok()?;
            let pubkey = std::env::var(format!("{prefix}_UPDATE_TEST_PUBKEY")).ok()?;
            Some(Self { endpoint, pubkey })
        }
        #[cfg(not(feature = "update-test"))]
        {
            let _ = prefix;
            None
        }
    }
}

/// Whether a build of `version` checks for updates, and where. A test source (only ever `Some` in
/// an `update-test` build) replaces the channel's endpoint, its key and its hosts.
pub fn plan(
    feed: &Feed,
    version: &Version,
    key: Option<&str>,
    test: Option<TestSource>,
) -> Result<Source, Off> {
    if let Some(test) = test {
        let host = url::Url::parse(&test.endpoint)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        if !key_is_valid(&test.pubkey) {
            return Err(Off::BadKey);
        }
        return Ok(Source {
            page: test.endpoint.clone(),
            endpoint: test.endpoint,
            pubkey: test.pubkey,
            hosts: vec![host],
        });
    }
    let key = key.ok_or(Off::NoKey)?;
    if !key_is_valid(key) {
        return Err(Off::BadKey);
    }
    let channel = Channel::of(version);
    let endpoint = channel.endpoint(feed).ok_or(Off::StableNotBuilt)?;
    Ok(Source {
        endpoint: endpoint.to_string(),
        pubkey: key.to_string(),
        hosts: feed.hosts.iter().map(|h| h.to_string()).collect(),
        page: channel.page(feed).unwrap_or(feed.main_page).to_string(),
    })
}

/// Whether this copy of the app can replace itself, or can only point to the newer build.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Install {
    /// The update installs in place and the app relaunches.
    InPlace,
    /// A Linux package (deb, rpm, Flatpak): the package manager owns it.
    LinuxPackage,
    /// Windows, installed for every user (Program Files): replacing it would need an
    /// administrator, so the app doesn't try.
    MachineWide,
    /// macOS, in a folder this account can't write to: replacing it would ask for an
    /// administrator's password, so the app doesn't try.
    NotWritable,
}

impl Install {
    /// Why the app points to the build instead, in one line, with the app's name.
    pub fn words(self, app: &str) -> Option<String> {
        match self {
            Self::InPlace => None,
            Self::LinuxPackage => Some(format!(
                "{app} can't update itself when installed from a Linux package."
            )),
            Self::MachineWide => Some(format!(
                "{app} is installed for everyone on this computer, so it can't update itself."
            )),
            Self::NotWritable => Some(format!(
                "{app} can't update itself from a folder your account can't change."
            )),
        }
    }
}

/// Whether a Windows install at `exe` is the per-user one the NSIS installer makes (under
/// `%LOCALAPPDATA%`), which updates without an administrator; anything else (Program Files, an
/// MSI) is machine-wide.
pub fn windows_per_user(exe: &std::path::Path, local_app_data: Option<&std::path::Path>) -> bool {
    let Some(base) = local_app_data else {
        return false;
    };
    let lower = |p: &std::path::Path| p.to_string_lossy().replace('/', "\\").to_lowercase();
    let exe = lower(exe);
    let base = lower(base);
    let base = base.trim_end_matches('\\');
    !base.is_empty() && exe.starts_with(&format!("{base}\\"))
}

/// Where an update stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UpdateState {
    /// No check since launch.
    Idle,
    Checking,
    UpToDate,
    Downloading,
    /// Downloaded and checked: "Restart to update", or it installs on quit.
    Ready,
    /// A newer build exists, but this copy can't replace itself.
    Available,
    Failed,
}

/// What Settings → About, the sidebar and the menu show. Plain data and plain words; nothing
/// here is secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    /// The running version.
    pub version: String,
    pub channel: Channel,
    /// Whether this build checks at all.
    pub enabled: bool,
    /// Why it doesn't, in one line (`enabled` false).
    pub off: Option<String>,
    /// The "Check automatically" switch (Settings, on by default).
    pub auto_check: bool,
    pub state: UpdateState,
    /// The download's progress, 0 to 100 (`downloading`).
    pub percent: Option<u8>,
    /// The newer version (`downloading`, `ready`, `available`).
    pub offered: Option<String>,
    /// What went wrong, in plain words (`failed`).
    pub error: Option<String>,
    /// The release page (`available`).
    pub link: Option<String>,
    /// Why the app points there instead of updating itself (`available`).
    pub link_why: Option<String>,
    /// Whether the last check was asked for (the button or the menu), rather than the automatic
    /// one.
    pub asked: bool,
}

impl UpdateStatus {
    /// The status at launch, before any check.
    pub fn at_start(version: &Version, plan: &Result<Source, Off>, auto_check: bool) -> Self {
        Self {
            version: version.to_string(),
            channel: Channel::of(version),
            enabled: plan.is_ok(),
            off: plan.as_ref().err().map(|o| o.words().to_string()),
            auto_check,
            state: UpdateState::Idle,
            percent: None,
            offered: None,
            error: None,
            link: None,
            link_why: None,
            asked: false,
        }
    }

    /// The status at launch of a build on the local channel ([`local`]): it always checks (it
    /// needs no key), and has no web feed.
    pub fn at_start_local(version: &Version, auto_check: bool) -> Self {
        Self {
            channel: Channel::Local,
            enabled: true,
            off: None,
            ..Self::at_start(version, &Err(Off::NoKey), auto_check)
        }
    }

    /// Moves to `state`, clearing what belonged to the one before.
    pub fn to(&mut self, state: UpdateState) {
        self.state = state;
        if state != UpdateState::Downloading {
            self.percent = None;
        }
        if state != UpdateState::Failed {
            self.error = None;
        }
        if state != UpdateState::Available {
            self.link = None;
            self.link_why = None;
        }
        if matches!(
            state,
            UpdateState::Idle | UpdateState::Checking | UpdateState::UpToDate
        ) {
            self.offered = None;
        }
    }
}

/// A download's progress as a whole percentage, never past 99 until the signature has been
/// checked (`ready` says the rest).
pub fn percent(received: u64, total: Option<u64>) -> Option<u8> {
    let total = total.filter(|t| *t > 0)?;
    Some(((received.min(total) * 100) / total).min(99) as u8)
}

pub mod local;

#[cfg(feature = "tauri")]
pub mod shell;

#[cfg(test)]
mod tests;
