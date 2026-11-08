//! The updater in the window, for any Tauri 2 app (the `tauri` feature): Tauri's updater plugin
//! driven from Rust only, so no updater permission reaches the webview. Extracted from Lockdown's
//! `updater/shell.rs` (slice 59), with everything Lockdown-only behind [`Host`].
//!
//! - **Check** ([`check`]): the channel's manifest, then, when it names a newer build, the bundle
//!   for this platform, downloaded in the background and checked against the app's key (the
//!   plugin's own minisign check, with the signed version required to match the manifest's).
//!   Every request and every redirect must stay on the plan's hosts; nothing sent names this
//!   person, this device or this build.
//! - **Restart to update** ([`restart`]): the host gets ready ([`Host::before_restart`]; Lockdown
//!   locks its vault), the bundle installs, the app relaunches.
//! - **Reload app** ([`reload`]): the app closes and opens again. A downloaded update installs on
//!   the way, so a reload is never a way to keep the old build by accident.
//! - **On quit** ([`on_exit`]): a downloaded update not yet taken installs as the app closes,
//!   without relaunching it.
//! - **Automatically**: at launch and every six hours, while the host's switch is on.
//! - **macOS**: "Check for Updates…" in the app menu does what the button does.
//!
//! An app wires it in four places: `.plugin(shell::plugin())` on the builder, [`start`] in
//! `setup`, [`on_exit`] on `RunEvent::Exit`, and its commands. The kit's [`commands`] are ready to
//! register; an app with its own error type wraps [`status`], [`check_now`], [`restart`],
//! [`set_auto_check`] and [`reload`] in its own commands instead.

use std::sync::{Arc, Mutex};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_updater::{Update, UpdaterExt};

use crate::{
    CHECK_EVERY, Feed, Install, NETWORK_TIMEOUT, Off, Source, TestSource, UpdateState,
    UpdateStatus, allowed, is_newer, percent, plan,
};

/// What an app tells the kit, and what the kit asks of it. Everything has a default except the
/// "Check automatically" switch, which the app keeps in its own settings.
pub trait Host: Send + Sync + 'static {
    /// Whether "Check automatically" is on (on by default in every app).
    fn auto_check(&self) -> bool;

    /// Saves the switch. Runs off the main thread.
    fn set_auto_check(&self, on: bool) -> Result<(), String>;

    /// Gets ready for the app to close and open again, before an update installs or the app
    /// reloads: Lockdown locks its vault; Synapse stops its background service.
    fn before_restart(&self, _handle: &AppHandle) {}

    /// Why checking is held right now, if it is (Lockdown's demo home), in the words after
    /// "<App> didn't check for updates: ".
    fn network_held(&self) -> Option<String> {
        None
    }

    /// Every change of status, after it's sent to the page (an end-to-end run's report).
    fn on_status(&self, _handle: &AppHandle, _status: &UpdateStatus) {}

    /// The status at launch, and whether this copy can replace itself (an end-to-end run's
    /// report).
    fn on_started(&self, _handle: &AppHandle, _status: &UpdateStatus, _install: Install) {}
}

/// How an app's updater is set up.
pub struct Config {
    /// Where the app's builds are published.
    pub feed: Feed,
    /// The app's public key (base64 of `tauri signer generate`'s `.pub`), built in from its repo;
    /// `None` when the build was made without it.
    pub key: Option<&'static str>,
    /// A test-only source (`update-test` builds only; `None` otherwise).
    pub test: Option<TestSource>,
    /// The event every change of [`UpdateStatus`] is sent on (`"<app>://update"`).
    pub event: &'static str,
    /// The macOS app menu's "Check for Updates…" item ID.
    pub menu_id: &'static str,
}

/// The plugin, configured from Rust. The app's `plugins.updater` block in `tauri.conf.json` holds
/// only what the plugin can't take from code: `"requireSignedVersion": true` and, on Windows,
/// `"installMode": "passive"`.
pub fn plugin() -> tauri::plugin::TauriPlugin<tauri::Wry, tauri_plugin_updater::Config> {
    tauri_plugin_updater::Builder::new().build()
}

/// What a check found.
enum Found {
    UpToDate,
    /// Downloaded and checked, waiting for a restart or the quit.
    Ready(Box<Update>, Vec<u8>),
    /// Newer, but this copy can't replace itself.
    Available(String),
}

struct Inner {
    status: UpdateStatus,
    /// The downloaded update and its checked bytes.
    ready: Option<(Box<Update>, Vec<u8>)>,
    /// A check or a download is running.
    busy: bool,
}

/// The updater's state, managed by the app.
pub struct Updates {
    app: &'static str,
    event: &'static str,
    source: Result<Source, Off>,
    install: Install,
    host: Arc<dyn Host>,
    inner: Mutex<Inner>,
}

impl Updates {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Where updates stand.
    pub fn status(&self) -> UpdateStatus {
        self.lock().status.clone()
    }

    /// Whether this copy can replace itself.
    pub fn install(&self) -> Install {
        self.install
    }
}

fn updates(handle: &AppHandle) -> Result<Arc<Updates>, String> {
    handle
        .try_state::<Arc<Updates>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| "the updater isn't running".to_string())
}

fn emit(handle: &AppHandle, updates: &Updates) {
    let status = updates.status();
    updates.host.on_status(handle, &status);
    let _ = handle.emit(updates.event, status);
}

/// Sets up the updater: the plan, the status, the macOS menu item, and the automatic check at
/// launch and every six hours.
pub fn start(handle: &AppHandle, config: Config, host: Arc<dyn Host>) {
    let version = handle.package_info().version.clone();
    let source = plan(&config.feed, &version, config.key, config.test);
    let updates = Arc::new(Updates {
        app: config.feed.app,
        event: config.event,
        install: install_mode(config.feed.app),
        inner: Mutex::new(Inner {
            status: UpdateStatus::at_start(&version, &source, host.auto_check()),
            ready: None,
            busy: false,
        }),
        source,
        host,
    });
    handle.manage(updates.clone());
    updates
        .host
        .on_started(handle, &updates.status(), updates.install);
    #[cfg(target_os = "macos")]
    if let Err(e) = install_menu(handle, config.menu_id) {
        log::warn!("the Check for Updates menu item couldn't be added: {e}");
    }
    #[cfg(not(target_os = "macos"))]
    let _ = config.menu_id;
    if updates.source.is_err() {
        return;
    }
    let handle = handle.clone();
    let _ = std::thread::Builder::new()
        .name(format!("{}-updates", updates.app.to_lowercase()))
        .spawn(move || {
            loop {
                if updates.host.auto_check() {
                    tauri::async_runtime::block_on(check(handle.clone(), false));
                }
                std::thread::sleep(CHECK_EVERY);
            }
        });
}

/// Checks the channel, and downloads a newer build in the background. `asked`: the button or the
/// menu, not the automatic check.
pub async fn check(handle: AppHandle, asked: bool) {
    let Ok(updates) = updates(&handle) else {
        return;
    };
    let Ok(source) = updates.source.clone() else {
        return;
    };
    {
        let mut inner = updates.lock();
        if inner.busy {
            return;
        }
        inner.status.asked = asked;
        if inner.ready.is_some() {
            // Already downloaded: say so again, and download nothing.
            drop(inner);
            emit(&handle, &updates);
            return;
        }
        inner.busy = true;
        inner.status.to(UpdateState::Checking);
    }
    emit(&handle, &updates);
    let found = match updates.host.network_held() {
        Some(reason) => Err(format!(
            "{} didn't check for updates: {reason}",
            updates.app
        )),
        None => find(&handle, &updates, &source).await,
    };
    {
        let mut inner = updates.lock();
        inner.busy = false;
        match found {
            Ok(Found::UpToDate) => inner.status.to(UpdateState::UpToDate),
            Ok(Found::Ready(update, bytes)) => {
                inner.status.to(UpdateState::Ready);
                inner.status.offered = Some(update.version.clone());
                inner.ready = Some((update, bytes));
            }
            Ok(Found::Available(version)) => {
                inner.status.to(UpdateState::Available);
                inner.status.offered = Some(version);
                inner.status.link = Some(source.page.clone());
                inner.status.link_why = updates.install.words(updates.app);
            }
            Err(words) => {
                inner.status.to(UpdateState::Failed);
                inner.status.error = Some(words);
            }
        }
    }
    emit(&handle, &updates);
}

async fn find(
    handle: &AppHandle,
    updates: &Arc<Updates>,
    source: &Source,
) -> Result<Found, String> {
    let app = updates.app;
    allowed(&source.endpoint, &source.hosts)?;
    let endpoint = source
        .endpoint
        .parse()
        .map_err(|_| "the update address isn't a web address".to_string())?;
    // A redirect off the plan's hosts stops the request; the host is kept to say which.
    let refused: Arc<Mutex<Option<String>>> = Arc::default();
    let hosts = source.hosts.clone();
    let seen = refused.clone();
    let updater = handle
        .updater_builder()
        .pubkey(source.pubkey.clone())
        .endpoints(vec![endpoint])
        .map_err(|e| words(app, &e, None))?
        .version_comparator(|current, release| is_newer(&current, &release.version))
        .timeout(NETWORK_TIMEOUT)
        .configure_client(move |client| {
            let hosts = hosts.clone();
            let seen = seen.clone();
            client.redirect(reqwest::redirect::Policy::custom(move |attempt| {
                if attempt.previous().len() >= 10 {
                    return attempt.error("too many redirects");
                }
                match allowed(attempt.url().as_str(), &hosts) {
                    Ok(()) => attempt.follow(),
                    Err(why) => {
                        if let Ok(mut s) = seen.lock() {
                            *s = Some(why.clone());
                        }
                        attempt.error(why)
                    }
                }
            }))
        })
        .build()
        .map_err(|e| words(app, &e, None))?;
    let refused_words = || refused.lock().ok().and_then(|s| s.clone());
    let Some(update) = updater
        .check()
        .await
        .map_err(|e| words(app, &e, refused_words()))?
    else {
        return Ok(Found::UpToDate);
    };
    allowed(update.download_url.as_str(), &source.hosts)?;
    if updates.install != Install::InPlace {
        return Ok(Found::Available(update.version.clone()));
    }
    {
        let mut inner = updates.lock();
        inner.status.to(UpdateState::Downloading);
        inner.status.offered = Some(update.version.clone());
        inner.status.percent = Some(0);
    }
    emit(handle, updates);
    let mut received = 0u64;
    let mut shown = Some(0u8);
    let bytes = update
        .download(
            |chunk, total| {
                received += chunk as u64;
                let now = percent(received, total);
                if now != shown {
                    shown = now;
                    updates.lock().status.percent = now;
                    emit(handle, updates);
                }
            },
            || {},
        )
        .await
        .map_err(|e| words(app, &e, refused_words()))?;
    Ok(Found::Ready(Box::new(update), bytes))
}

/// The plugin's error in plain words. Nothing here can carry a secret: the requests hold none.
fn words(app: &str, e: &tauri_plugin_updater::Error, refused: Option<String>) -> String {
    use tauri_plugin_updater::Error as E;
    if let Some(why) = refused {
        return format!("The update was refused: {why}.");
    }
    match e {
        E::Minisign(_)
        | E::Base64(_)
        | E::SignatureUtf8(_)
        | E::SignedVersionMismatch { .. }
        | E::MissingSignedVersion => {
            format!("The update was refused: its signature didn't match {app}'s key.")
        }
        E::Reqwest(_) | E::Network(_) => {
            format!("{app} couldn't reach GitHub. Check your connection and try again.")
        }
        E::ReleaseNotFound | E::TargetNotFound(_) | E::TargetsNotFound(_) => {
            "The newest build has nothing for this computer yet.".into()
        }
        E::Serialization(_) | E::Semver(_) | E::InvalidUpdaterFormat => {
            "The update's description couldn't be read.".into()
        }
        E::InsecureTransportProtocol => "Updates come only over https.".into(),
        other => format!("The update didn't work: {other}."),
    }
}

/// Whether this copy can replace itself.
fn install_mode(app: &str) -> Install {
    #[cfg(target_os = "linux")]
    {
        let _ = app;
        Install::LinuxPackage
    }
    #[cfg(windows)]
    {
        let _ = app;
        let exe = std::env::current_exe().ok();
        let local = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
        match exe {
            Some(exe) if crate::windows_per_user(&exe, local.as_deref()) => Install::InPlace,
            _ => Install::MachineWide,
        }
    }
    #[cfg(target_os = "macos")]
    {
        let bundle = std::env::current_exe()
            .ok()
            .and_then(|exe| tauri_plugin_updater::extract_path_from_executable(&exe).ok());
        match bundle {
            Some(bundle) if mac_can_replace(app, &bundle) => Install::InPlace,
            _ => Install::NotWritable,
        }
    }
    #[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
    {
        let _ = app;
        Install::LinuxPackage
    }
}

/// The plugin replaces the bundle with renames, which need write access to the folder holding it
/// and to the bundle itself; without them it would ask for an administrator's password, which the
/// apps never do.
#[cfg(target_os = "macos")]
fn mac_can_replace(app: &str, bundle: &std::path::Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    if bundle.extension().is_none_or(|e| e != "app") {
        return false;
    }
    let Some(folder) = bundle.parent() else {
        return false;
    };
    let probe = folder.join(format!(
        ".{}-update-{}",
        app.to_lowercase(),
        std::process::id()
    ));
    let Ok(file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    else {
        return false;
    };
    let me = file.metadata().map(|m| m.uid()).ok();
    drop(file);
    let _ = std::fs::remove_file(&probe);
    std::fs::metadata(bundle).is_ok_and(|m| {
        let mode = m.mode();
        (Some(m.uid()) == me && mode & 0o200 != 0) || mode & 0o002 != 0
    })
}

/// Where updates stand.
pub fn status(handle: &AppHandle) -> Result<UpdateStatus, String> {
    Ok(updates(handle)?.status())
}

/// "Check for updates": checks now and starts the download; the outcome arrives on the event.
pub fn check_now(handle: &AppHandle) -> Result<UpdateStatus, String> {
    let updates = updates(handle)?;
    tauri::async_runtime::spawn(check(handle.clone(), true));
    Ok(updates.status())
}

/// "Restart to update": the host gets ready, the update installs, the app relaunches. On Windows
/// the passive installer closes the app and starts the new one itself.
pub fn restart(handle: &AppHandle) -> Result<(), String> {
    let updates = updates(handle).map_err(|_| "there's no update to install".to_string())?;
    let (update, bytes) = updates
        .lock()
        .ready
        .take()
        .ok_or_else(|| "there's no update to install yet".to_string())?;
    updates.host.before_restart(handle);
    if let Err(e) = update.install(&bytes) {
        let message = words(updates.app, &e, None);
        let mut inner = updates.lock();
        inner.status.to(UpdateState::Failed);
        inner.status.error = Some(message.clone());
        drop(inner);
        emit(handle, &updates);
        return Err(message);
    }
    handle.restart()
}

/// "Reload app": the app closes and opens again. With an update downloaded, this is "Restart to
/// update"; otherwise the host gets ready and the app relaunches as it is.
pub fn reload(handle: &AppHandle) -> Result<(), String> {
    if let Ok(updates) = updates(handle) {
        if updates.lock().ready.is_some() {
            return restart(handle);
        }
        updates.host.before_restart(handle);
    }
    handle.restart()
}

/// The "Check automatically" switch: the host saves it; turning it on checks at once when nothing
/// has been checked since launch.
pub async fn set_auto_check(handle: AppHandle, on: bool) -> Result<UpdateStatus, String> {
    let updates = updates(&handle)?;
    let host = updates.host.clone();
    tauri::async_runtime::spawn_blocking(move || host.set_auto_check(on))
        .await
        .map_err(|e| format!("the call didn't finish: {e}"))??;
    let idle = {
        let mut inner = updates.lock();
        inner.status.auto_check = on;
        inner.status.state == UpdateState::Idle
    };
    emit(&handle, &updates);
    if on && idle {
        tauri::async_runtime::spawn(check(handle.clone(), false));
    }
    Ok(updates.status())
}

/// The app is closing: a downloaded update installs now, without a relaunch.
pub fn on_exit(handle: &AppHandle) {
    let Ok(updates) = updates(handle) else {
        return;
    };
    let Some((update, bytes)) = updates.lock().ready.take() else {
        return;
    };
    updates.host.before_restart(handle);
    let _ = (*update).restart_after_install(false).install(&bytes);
}

#[cfg(target_os = "macos")]
fn install_menu(handle: &AppHandle, menu_id: &'static str) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, MenuItemKind};
    let menu = match handle.menu() {
        Some(menu) => menu,
        None => {
            let menu = Menu::default(handle)?;
            handle.set_menu(menu.clone())?;
            menu
        }
    };
    let item = MenuItem::with_id(handle, menu_id, "Check for Updates…", true, None::<&str>)?;
    // The app menu is the first; "Check for Updates…" goes under "About".
    if let Some(MenuItemKind::Submenu(app_menu)) = menu.items()?.into_iter().next() {
        app_menu.insert(&item, 1)?;
    }
    handle.on_menu_event(move |app, event| {
        if event.id() == menu_id {
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.set_focus();
            }
            tauri::async_runtime::spawn(check(app.clone(), true));
        }
    });
    Ok(())
}

/// Ready-made commands, for an app without its own error type: register them with
/// `tauri::generate_handler![app_updates::shell::commands::update_status, …]`. Errors reach the
/// page as plain strings.
pub mod commands {
    use tauri::AppHandle;

    use crate::UpdateStatus;

    /// Where updates stand.
    #[tauri::command]
    pub async fn update_status(handle: AppHandle) -> Result<UpdateStatus, String> {
        super::status(&handle)
    }

    /// "Check for updates".
    #[tauri::command]
    pub async fn update_check(handle: AppHandle) -> Result<UpdateStatus, String> {
        super::check_now(&handle)
    }

    /// "Restart to update".
    #[tauri::command]
    pub async fn update_restart(handle: AppHandle) -> Result<(), String> {
        tauri::async_runtime::spawn_blocking(move || super::restart(&handle))
            .await
            .map_err(|e| format!("the call didn't finish: {e}"))?
    }

    /// The "Check automatically" switch.
    #[tauri::command]
    pub async fn update_auto_set(handle: AppHandle, on: bool) -> Result<UpdateStatus, String> {
        super::set_auto_check(handle, on).await
    }

    /// "Reload app".
    #[tauri::command]
    pub async fn app_reload(handle: AppHandle) -> Result<(), String> {
        tauri::async_runtime::spawn_blocking(move || super::reload(&handle))
            .await
            .map_err(|e| format!("the call didn't finish: {e}"))?
    }
}

#[cfg(test)]
mod plugin_tests;
