//! The update engine for one product's server (docs/self-hosting-service-apps.md, section 2).
//!
//! Ported from Synapse's relay helper (`relay/src/updates/`, Stage L L5.12, LS18, X15e), for one
//! product per service app, with three things added:
//!
//! - **Ahead of time.** A check that finds a newer signed release pulls its images by digest at
//!   once, without touching a running container. Settings then says it is downloaded.
//! - **The person's choice** ([`Policy`]): accept each update by hand, or update automatically
//!   inside a time window or whenever nothing would be lost. A release that needs a step by hand,
//!   or whose by-hand flag isn't signed, is never installed automatically.
//! - **The product's own steps** ([`Hooks`]): what would be lost now, a backup before, a
//!   self-test after (a failure rolls everything back), and letting work start again.

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use semver::Version;
use serde::{Deserialize, Serialize};

use crate::docker::{Engine, Service};
use crate::release::{self, Release};

/// How often the channel is checked: every six hours, as the desktop kit.
pub const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);
/// How many past updates are kept for Settings.
pub const KEEP: usize = 10;

/// The product this engine updates, and where its releases come from.
#[derive(Debug, Clone)]
pub struct Feed {
    /// Its id: the value of its containers' `org.snds.update.app` label (`warpcore`).
    pub id: String,
    /// Its name as a person reads it (`Warpcore`).
    pub name: String,
    /// Its channel's `latest.json`.
    pub endpoint: String,
    /// Its minisign public key, base64, as `tauri signer generate` writes it, built into the
    /// service app's image. Empty: updates are off.
    pub key: String,
    /// The hosts its manifest may come from.
    pub hosts: Vec<String>,
    /// Services a release may carry that this server need not run (`artifacts` while its hostname
    /// doesn't exist).
    pub optional: Vec<String>,
    /// The service app's own service name (`server`). It is never swapped by the engine: a release
    /// entry for it is reported, and the service app updates itself apart.
    pub supervisor: Option<String>,
}

/// How the engine runs.
#[derive(Debug, Clone)]
pub struct Settings {
    pub feed: Feed,
    /// How long a new container has to become healthy.
    pub health_wait: Duration,
    /// How long a container with no health check must keep running to count as healthy.
    pub settle: Duration,
    /// How often an update accepted by hand asks again whether work is still running.
    pub wait_poll: Duration,
    /// How long an update accepted by hand waits for running work before it gives up.
    pub wait_max: Duration,
    /// How long the product's self-test may take.
    pub after_wait: Duration,
    /// Allow plain-HTTP channels on loopback (tests only).
    pub loopback_feeds: bool,
}

impl Settings {
    pub fn new(feed: Feed) -> Settings {
        Settings {
            feed,
            health_wait: Duration::from_secs(120),
            settle: Duration::from_secs(10),
            wait_poll: Duration::from_secs(15),
            wait_max: Duration::from_secs(6 * 60 * 60),
            after_wait: Duration::from_secs(10 * 60),
            loopback_feeds: false,
        }
    }
}

/// Where the update stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    /// Not checked yet.
    Idle,
    /// No container on this server carries the product's label.
    NotInstalled,
    /// Updates are off: see `off`.
    Off,
    UpToDate,
    /// A newer signed release is being downloaded ahead of time.
    Downloading,
    /// A newer signed release is offered; `downloaded` says whether its images are here.
    Available,
    /// Accepted, waiting for running work to finish: see `blockers`.
    Waiting,
    Updating,
    /// The update went in and is healthy.
    Updated,
    RollingBack,
    /// Back on the kept version, asked for or after a failed health check or self-test.
    RolledBack,
    /// Something went wrong: see `error`.
    Failed,
}

impl State {
    fn busy(self) -> bool {
        matches!(
            self,
            State::Downloading | State::Waiting | State::Updating | State::RollingBack
        )
    }
}

/// When an update goes in: the person's choice, in the product's own Settings → Updates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum Mode {
    /// Accept each update by hand: **Restart to update**.
    Manual,
    /// Update automatically between `from` and `to` (`HH:MM`, the server's time zone; a window
    /// may cross midnight), when nothing would be lost.
    Window { from: String, to: String },
    /// Update automatically as soon as nothing would be lost.
    NothingLost,
}

/// The person's update settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Policy {
    /// Check at start and every six hours.
    pub auto_check: bool,
    #[serde(flatten)]
    pub mode: Mode,
}

impl Default for Policy {
    /// Checking on, installing by hand: nothing restarts unless the person chooses.
    fn default() -> Policy {
        Policy {
            auto_check: true,
            mode: Mode::Manual,
        }
    }
}

/// Minutes since midnight from `HH:MM`.
pub fn minutes(hhmm: &str) -> Result<u16, String> {
    let bad = || format!("{hhmm}: not a time such as 03:00");
    let (h, m) = hhmm.trim().split_once(':').ok_or_else(bad)?;
    let (h, m): (u16, u16) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if h > 23 || m > 59 || hhmm.trim().len() != 5 {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

impl Policy {
    pub fn validate(&self) -> Result<(), String> {
        if let Mode::Window { from, to } = &self.mode
            && minutes(from)? == minutes(to)?
        {
            return Err("the window starts and ends at the same time".into());
        }
        Ok(())
    }

    /// Whether minute `now` of the day (server time) is a moment this policy installs by itself.
    pub fn allows(&self, now: u16) -> bool {
        match &self.mode {
            Mode::Manual => false,
            Mode::NothingLost => true,
            Mode::Window { from, to } => match (minutes(from), minutes(to)) {
                (Ok(f), Ok(t)) if f < t => (f..t).contains(&now),
                (Ok(f), Ok(t)) => now >= f || now < t,
                _ => false,
            },
        }
    }
}

/// One past update, for Settings: what it went to, how it ended, and the backup taken before it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    pub at: u64,
    pub from: Option<String>,
    pub to: String,
    /// `updated`, `rolled_back` (by itself, after a failure), `rollback` (asked for), `failed`.
    pub result: String,
    /// The backup taken before it, by name.
    pub backup: Option<String>,
    pub automatic: bool,
    pub error: Option<String>,
}

/// The product's update status, as its Settings and the status-only view show it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Status {
    pub app: String,
    pub name: String,
    pub state: State,
    /// The version running.
    pub version: Option<String>,
    /// The newer signed version on offer.
    pub offered: Option<String>,
    /// Whether every image of `offered` is already on this server.
    pub downloaded: bool,
    /// For `offered`: `true` needs a step by hand, `false` may install by itself, `null` wasn't
    /// signed either way (by hand only).
    pub by_hand: Option<bool>,
    pub notes: Option<String>,
    /// The version kept for rollback.
    pub previous: Option<String>,
    /// Its services (containers) on this server.
    pub services: Vec<String>,
    /// What the update is doing now.
    pub step: Option<String>,
    /// What an update is waiting for, named.
    pub blockers: Vec<String>,
    /// A newer build of the service app itself, which it installs apart.
    pub supervisor_offered: Option<String>,
    /// What went wrong, in plain words.
    pub error: Option<String>,
    /// Why updates are off.
    pub off: Option<String>,
    /// When it was last checked (Unix seconds).
    pub checked: Option<u64>,
    pub policy: Policy,
    /// The last updates, newest first.
    pub history: Vec<Record>,
}

/// What survives a restart of the service app: the person's choice and past updates.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Saved {
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub history: Vec<Record>,
}

/// Where [`Saved`] is kept. The shell keeps it in a file; tests keep it in memory.
pub trait Store: Send + Sync {
    fn load(&self) -> Saved;
    fn save(&self, saved: &Saved) -> Result<(), String>;
}

/// A [`Store`] in memory.
#[derive(Default)]
pub struct MemoryStore(pub Mutex<Saved>);

impl Store for MemoryStore {
    fn load(&self) -> Saved {
        lock(&self.0).clone()
    }
    fn save(&self, saved: &Saved) -> Result<(), String> {
        *lock(&self.0) = saved.clone();
        Ok(())
    }
}

/// The product's own steps around an update. Every method has a default that does nothing.
pub trait Hooks: Send + Sync {
    /// What would be lost by updating now (running tasks, named). Empty: a safe moment.
    fn blockers(&self) -> Result<Vec<String>, String> {
        Ok(Vec::new())
    }
    /// Before anything changes: stop new work starting and take a backup. An error means nothing
    /// changes. `Some(name)` is the backup taken, shown with the update.
    fn before(&self, _from: Option<&str>, _to: &str) -> Result<Option<String>, String> {
        Ok(None)
    }
    /// Once every new container is healthy: the product's self-test. An error rolls every service
    /// back.
    fn after(
        &self,
        _engine: &dyn Engine,
        _services: &[Service],
        _wait: Duration,
    ) -> Result<(), String> {
        Ok(())
    }
    /// When the update is over, however it ended: let work start again.
    fn done(&self) {}
}

/// No steps of the product's own.
pub struct NoHooks;
impl Hooks for NoHooks {}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

struct Inner {
    settings: Settings,
    engine: Box<dyn Engine>,
    hooks: Box<dyn Hooks>,
    store: Box<dyn Store>,
    status: Mutex<Status>,
    /// The release on offer, as last checked.
    offered: Mutex<Option<Release>>,
    /// Held while a download, update or rollback runs: one at a time.
    busy: Mutex<()>,
}

/// The engine. Cheap to clone; clones share one state.
#[derive(Clone)]
pub struct Updater {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Updater {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Updater")
            .field("feed", &self.inner.settings.feed)
            .finish_non_exhaustive()
    }
}

/// How an update was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// **Restart to update**: waits for running work, then goes in.
    ByHand,
    /// The person's automatic choice: goes in only if nothing would be lost right now.
    Automatic,
}

impl Updater {
    pub fn new(
        settings: Settings,
        engine: Box<dyn Engine>,
        hooks: Box<dyn Hooks>,
        store: Box<dyn Store>,
    ) -> Updater {
        let feed = &settings.feed;
        let off = (!release::key_ok(&feed.key)).then(|| {
            format!(
                "Updates for {} are off: this build has no key for its releases.",
                feed.name
            )
        });
        let saved = store.load();
        let status = Status {
            app: feed.id.clone(),
            name: feed.name.clone(),
            state: if off.is_some() {
                State::Off
            } else {
                State::Idle
            },
            version: None,
            offered: None,
            downloaded: false,
            by_hand: None,
            notes: None,
            previous: None,
            services: Vec::new(),
            step: None,
            blockers: Vec::new(),
            supervisor_offered: None,
            error: None,
            off,
            checked: None,
            policy: saved.policy,
            history: saved.history,
        };
        Updater {
            inner: Arc::new(Inner {
                settings,
                engine,
                hooks,
                store,
                status: Mutex::new(status),
                offered: Mutex::default(),
                busy: Mutex::default(),
            }),
        }
    }

    fn feed(&self) -> &Feed {
        &self.inner.settings.feed
    }

    fn set(&self, f: impl FnOnce(&mut Status)) -> Status {
        let mut st = lock(&self.inner.status);
        f(&mut st);
        st.clone()
    }

    fn get(&self) -> Status {
        lock(&self.inner.status).clone()
    }

    fn save(&self) {
        let st = self.get();
        let _ = self.inner.store.save(&Saved {
            policy: st.policy,
            history: st.history,
        });
    }

    fn is_supervisor(&self, svc: &str) -> bool {
        self.feed().supervisor.as_deref() == Some(svc)
    }

    /// The product's services the engine swaps: every labelled container but the service app's own.
    fn services(&self) -> Result<Vec<Service>, String> {
        Ok(self
            .inner
            .engine
            .services(&self.feed().id)?
            .into_iter()
            .filter(|s| !self.is_supervisor(&s.service))
            .collect())
    }

    /// Reads what runs now into the status.
    fn observe(&self) -> Result<Vec<Service>, String> {
        let svcs = self.services()?;
        self.set(|st| {
            st.services = svcs.iter().map(|s| s.service.clone()).collect();
            st.version = svcs.iter().find_map(|s| s.version.clone());
            st.previous = svcs
                .iter()
                .find_map(|s| s.previous.clone())
                .map(|v| v.unwrap_or_else(|| "unknown".into()));
            if svcs.is_empty() && st.state != State::Off {
                st.state = State::NotInstalled;
            }
        });
        Ok(svcs)
    }

    /// The status now, read fresh from the server (no check of the channel).
    pub fn status(&self) -> Status {
        if !self.get().state.busy() && self.inner.busy.try_lock().is_ok() {
            let _ = self.observe();
            self.refresh_downloaded();
        }
        self.get()
    }

    /// Whether the offered release's images are still all here (someone may have pruned them).
    fn refresh_downloaded(&self) {
        let Some(rel) = lock(&self.inner.offered).clone() else {
            return;
        };
        let here = self
            .wanted(&rel)
            .iter()
            .all(|i| self.inner.engine.has_image(i).unwrap_or(false));
        self.set(|st| st.downloaded = here);
    }

    /// The release on offer now, checked in full (LS18).
    fn latest(&self) -> Result<Release, String> {
        let feed = self.feed();
        if !release::key_ok(&feed.key) {
            return Err(format!(
                "Updates for {} are off: this build has no key for its releases.",
                feed.name
            ));
        }
        let m = release::fetch(
            &feed.endpoint,
            &feed.hosts,
            self.inner.settings.loopback_feeds,
        )?;
        release::check(&feed.id, &feed.key, &m)
    }

    fn running_version(svcs: &[Service]) -> Version {
        svcs.iter()
            .find_map(|s| s.version.as_deref())
            .and_then(|v| Version::parse(v.trim_start_matches('v')).ok())
            .unwrap_or_else(|| Version::new(0, 0, 0))
    }

    /// Each running service paired with its image in `rel`, or why the release doesn't fit this
    /// server: a service it has no image for, or one it adds that isn't optional.
    fn plan<'a>(
        &self,
        rel: &Release,
        svcs: &'a [Service],
    ) -> Result<Vec<(&'a Service, String)>, String> {
        let mut plan = Vec::new();
        for s in svcs {
            let image = rel
                .images
                .get(&s.service)
                .ok_or_else(|| format!("the release has no image for its {} service", s.service))?;
            plan.push((s, image.clone()));
        }
        let feed = self.feed();
        if let Some(extra) = rel.images.keys().find(|k| {
            !svcs.iter().any(|s| &s.service == *k)
                && !feed.optional.contains(k)
                && !self.is_supervisor(k)
        }) {
            return Err(format!(
                "the release adds a {extra} service this server doesn't run"
            ));
        }
        Ok(plan)
    }

    /// The images of `rel` this server would run.
    fn wanted(&self, rel: &Release) -> Vec<String> {
        let running = lock(&self.inner.status).services.clone();
        rel.images
            .iter()
            .filter(|(k, _)| running.contains(k))
            .map(|(_, v)| v.clone())
            .collect()
    }

    /// Pulls every image of `plan` that isn't here yet.
    fn download(&self, plan: &[(&Service, String)]) -> Result<(), String> {
        for (_, image) in plan {
            if !self.inner.engine.has_image(image)? {
                self.inner.engine.pull(image)?;
            }
        }
        Ok(())
    }

    /// **Check for updates.** A newer signed release is downloaded at once, ahead of time; nothing
    /// that runs is touched.
    pub fn check(&self) -> Result<Status, String> {
        if self.get().off.is_some() {
            return Ok(self.get());
        }
        let Ok(_busy) = self.inner.busy.try_lock() else {
            return Ok(self.get());
        };
        let fail = |e: String| {
            self.set(|st| {
                st.state = State::Failed;
                st.error = Some(e.clone());
                st.checked = Some(now());
                st.step = None;
            });
            e
        };
        let svcs = self.observe().map_err(fail)?;
        if svcs.is_empty() {
            return Ok(self.set(|st| st.checked = Some(now())));
        }
        let rel = self.latest().map_err(fail)?;
        let supervisor = self
            .feed()
            .supervisor
            .as_ref()
            .and_then(|s| rel.images.get(s))
            .map(|_| rel.version.to_string());
        if rel.version <= Self::running_version(&svcs) {
            *lock(&self.inner.offered) = None;
            return Ok(self.set(|st| {
                st.checked = Some(now());
                st.error = None;
                st.offered = None;
                st.downloaded = false;
                st.by_hand = None;
                st.notes = None;
                st.supervisor_offered = supervisor;
                st.state = State::UpToDate;
            }));
        }
        let plan = self.plan(&rel, &svcs).map_err(fail)?;
        *lock(&self.inner.offered) = Some(rel.clone());
        self.set(|st| {
            st.checked = Some(now());
            st.error = None;
            st.offered = Some(rel.version.to_string());
            st.by_hand = rel.by_hand;
            st.notes = rel.notes.clone();
            st.supervisor_offered = supervisor;
            st.state = State::Downloading;
            st.step = Some("Downloading".into());
        });
        let got = self.download(&plan);
        Ok(self.set(|st| {
            st.state = State::Available;
            st.step = None;
            match got {
                Ok(()) => st.downloaded = true,
                Err(e) => {
                    st.downloaded = false;
                    st.error = Some(format!("Couldn't download it yet: {e}"));
                }
            }
        }))
    }

    /// **Restart to update**, on its own thread. Returns at once.
    pub fn start_update(&self, trigger: Trigger) -> Result<Status, String> {
        self.start(State::Updating, move |u| u.update(trigger))
    }

    /// **Roll back**, on its own thread. Returns at once with `rolling_back`.
    pub fn start_rollback(&self) -> Result<Status, String> {
        self.start(State::RollingBack, Updater::rollback)
    }

    fn start(
        &self,
        state: State,
        run: impl FnOnce(&Updater) -> Result<Status, String> + Send + 'static,
    ) -> Result<Status, String> {
        if let Some(off) = self.get().off {
            return Err(off);
        }
        if self.get().state.busy() || self.inner.busy.try_lock().is_err() {
            return Err("an update is already running".into());
        }
        let st = self.set(|st| {
            st.state = state;
            st.error = None;
        });
        let u = self.clone();
        std::thread::spawn(move || {
            let _ = run(&u);
        });
        Ok(st)
    }

    fn record(&self, r: Record) {
        self.set(|st| {
            st.history.insert(0, r);
            st.history.truncate(KEEP);
        });
        self.save();
    }

    /// Updates to the newest signed release, and waits for it. If a new container isn't healthy,
    /// or the product's self-test fails, every service goes back to the kept version by itself.
    pub fn update(&self, trigger: Trigger) -> Result<Status, String> {
        if let Some(off) = self.get().off {
            return Err(off);
        }
        let _busy = lock(&self.inner.busy);
        let automatic = trigger == Trigger::Automatic;
        let fail = |e: String| {
            self.set(|st| {
                st.state = State::Failed;
                st.error = Some(e.clone());
                st.step = None;
                st.blockers.clear();
            });
            e
        };
        self.set(|st| {
            st.state = State::Updating;
            st.step = Some("Checking the release".into());
        });
        let svcs = self.services().map_err(fail)?;
        if svcs.is_empty() {
            return Err(fail("no container on this server is part of it".into()));
        }
        // Checked again here, not taken from the last check: only what verifies now goes in.
        let rel = self.latest().map_err(fail)?;
        let from = svcs.iter().find_map(|s| s.version.clone());
        if rel.version <= Self::running_version(&svcs) {
            let _ = self.observe();
            return Ok(self.set(|st| {
                st.state = State::UpToDate;
                st.offered = None;
                st.step = None;
            }));
        }
        if rel.by_hand == Some(true) {
            return Err(fail(format!(
                "{} needs a step on the server, so it can't be installed from here.",
                rel.version
            )));
        }
        if automatic && rel.by_hand != Some(false) {
            return Err(fail(format!(
                "{} doesn't say it can install by itself, so it waits to be accepted by hand.",
                rel.version
            )));
        }
        let plan = self.plan(&rel, &svcs).map_err(fail)?;
        // Every image first, so nothing changes until all of them are here.
        self.set(|st| st.step = Some("Downloading".into()));
        self.download(&plan).map_err(fail)?;
        self.set(|st| st.downloaded = true);
        let version = rel.version.to_string();

        // Nothing is lost: wait for running work (by hand), or don't start (automatic).
        let began = std::time::Instant::now();
        loop {
            let blockers = self.inner.hooks.blockers().map_err(fail)?;
            if blockers.is_empty() {
                break;
            }
            if automatic {
                return Ok(self.set(|st| {
                    st.state = State::Available;
                    st.step = None;
                    st.blockers = blockers;
                }));
            }
            if began.elapsed() >= self.inner.settings.wait_max {
                return Err(fail(format!("Still waiting for: {}", blockers.join(", "))));
            }
            self.set(|st| {
                st.state = State::Waiting;
                st.step = Some("Waiting for running work".into());
                st.blockers = blockers;
            });
            std::thread::sleep(self.inner.settings.wait_poll);
        }
        self.set(|st| {
            st.state = State::Updating;
            st.blockers.clear();
            st.step = Some("Taking a backup".into());
        });
        let backup = match self.inner.hooks.before(from.as_deref(), &version) {
            Ok(b) => b,
            Err(e) => {
                self.inner.hooks.done();
                let e = format!("Nothing changed: the step before the update failed: {e}");
                self.record(Record {
                    at: now(),
                    from: from.clone(),
                    to: version.clone(),
                    result: "failed".into(),
                    backup: None,
                    automatic,
                    error: Some(e.clone()),
                });
                return Err(fail(e));
            }
        };
        let result = self.swap(&plan, &version);
        let result = result.and_then(|()| {
            self.set(|st| st.step = Some("Self-test".into()));
            let now_svcs = self.services()?;
            self.inner
                .hooks
                .after(
                    self.inner.engine.as_ref(),
                    &now_svcs,
                    self.inner.settings.after_wait,
                )
                .map_err(|e| SwapError::Unhealthy(format!("the self-test failed: {e}")))
        });
        let outcome = match result {
            Ok(()) => {
                *lock(&self.inner.offered) = None;
                let _ = self.observe();
                Ok(self.set(|st| {
                    st.state = State::Updated;
                    st.offered = None;
                    st.downloaded = false;
                    st.by_hand = None;
                    st.error = None;
                    st.step = None;
                }))
            }
            Err(SwapError::Unhealthy(why)) => {
                // Back to the kept version, every service, newest swap first.
                let undo = self.undo(&plan);
                let _ = self.observe();
                if undo.is_empty() {
                    Ok(self.set(|st| {
                        st.state = State::RolledBack;
                        st.error = Some(format!("{version} didn't start well: {why}"));
                        st.step = None;
                    }))
                } else {
                    Err(fail(format!(
                        "{version} didn't start well ({why}), and going back failed: {}",
                        undo.join("; ")
                    )))
                }
            }
        };
        self.inner.hooks.done();
        let st = self.get();
        self.record(Record {
            at: now(),
            from,
            to: version,
            result: match st.state {
                State::Updated => "updated",
                State::RolledBack => "rolled_back",
                _ => "failed",
            }
            .into(),
            backup,
            automatic,
            error: st.error.clone(),
        });
        // The status handed back includes this update's own entry.
        outcome.map(|_| self.get())
    }

    /// Swaps every service in `plan` and waits for each to be healthy.
    fn swap(&self, plan: &[(&Service, String)], version: &str) -> Result<(), SwapError> {
        let (wait, settle) = (self.inner.settings.health_wait, self.inner.settings.settle);
        for (s, image) in plan {
            self.set(|st| st.step = Some(format!("Starting {}", s.service)));
            self.inner
                .engine
                .replace(s, image, version)
                .map_err(|e| SwapError::Unhealthy(e.why))?;
            self.inner
                .engine
                .healthy(&s.name, wait, settle)
                .map_err(SwapError::Unhealthy)?;
        }
        Ok(())
    }

    /// Puts back every service in `plan` that has a kept container, newest first. Returns what
    /// couldn't be put back.
    fn undo(&self, plan: &[(&Service, String)]) -> Vec<String> {
        let mut failed = Vec::new();
        let now_svcs = self
            .inner
            .engine
            .services(&self.feed().id)
            .unwrap_or_default();
        for (s, _) in plan.iter().rev() {
            let kept = now_svcs
                .iter()
                .any(|n| n.name == s.name && n.previous.is_some());
            // A service not yet swapped is still its old self, with an older version kept from an
            // earlier update: leave it.
            let swapped = now_svcs
                .iter()
                .find(|n| n.name == s.name)
                .is_none_or(|n| n.id != s.id);
            if kept
                && swapped
                && let Err(e) = self.inner.engine.restore(&s.name)
            {
                failed.push(e);
            }
        }
        failed
    }

    /// **Roll back**: goes back to the version kept, and waits for it.
    pub fn rollback(&self) -> Result<Status, String> {
        if let Some(off) = self.get().off {
            return Err(off);
        }
        let _busy = lock(&self.inner.busy);
        let fail = |e: String| {
            self.set(|st| {
                st.state = State::Failed;
                st.error = Some(e.clone());
                st.step = None;
            });
            e
        };
        self.set(|st| st.state = State::RollingBack);
        let svcs = self.services().map_err(fail)?;
        let from = svcs.iter().find_map(|s| s.version.clone());
        let kept: Vec<&Service> = svcs.iter().filter(|s| s.previous.is_some()).collect();
        if kept.is_empty() {
            return Err(fail("no earlier version is kept".into()));
        }
        let to = kept
            .iter()
            .find_map(|s| s.previous.clone().flatten())
            .unwrap_or_else(|| "unknown".into());
        let (wait, settle) = (self.inner.settings.health_wait, self.inner.settings.settle);
        for s in &kept {
            self.inner.engine.restore(&s.name).map_err(fail)?;
        }
        for s in &kept {
            self.inner
                .engine
                .healthy(&s.name, wait, settle)
                .map_err(fail)?;
        }
        let _ = self.observe();
        self.record(Record {
            at: now(),
            from,
            to,
            result: "rollback".into(),
            backup: None,
            automatic: false,
            error: None,
        });
        Ok(self.set(|st| {
            st.state = State::RolledBack;
            st.error = None;
            st.offered = None;
            st.downloaded = false;
            st.step = None;
        }))
    }

    /// The person's choice, from the product's Settings → Updates.
    pub fn set_policy(&self, policy: Policy) -> Result<Status, String> {
        policy.validate()?;
        let st = self.set(|st| st.policy = policy);
        self.save();
        Ok(st)
    }

    /// Called every minute by the shell with the time now and the minute of the day in the
    /// server's time zone. Checks when due (at start, then every six hours) and installs a
    /// downloaded release by itself when the person chose that and this is an allowed moment.
    pub fn tick_at(&self, unix: u64, minute: u16) {
        let st = self.get();
        if st.off.is_some() || st.state.busy() {
            return;
        }
        let due = st
            .checked
            .is_none_or(|t| unix.saturating_sub(t) >= CHECK_EVERY.as_secs());
        let st = if st.policy.auto_check && due {
            match self.check() {
                Ok(st) => st,
                Err(_) => return,
            }
        } else {
            st
        };
        // A failed download is tried again on the next tick.
        let st = if st.state == State::Available && !st.downloaded {
            self.check().unwrap_or(st)
        } else {
            st
        };
        if st.state == State::Available
            && st.downloaded
            && st.by_hand == Some(false)
            && st.policy.allows(minute)
            && matches!(self.inner.hooks.blockers(), Ok(b) if b.is_empty())
        {
            let _ = self.update(Trigger::Automatic);
        }
    }

    /// [`Self::tick_at`] with the clock and the server's time zone.
    pub fn tick(&self) {
        let z = jiff::Zoned::now();
        let minute = (z.hour() as u16) * 60 + z.minute() as u16;
        self.tick_at(now(), minute);
    }
}

enum SwapError {
    Unhealthy(String),
}

impl From<String> for SwapError {
    fn from(e: String) -> SwapError {
        SwapError::Unhealthy(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_hh_mm() {
        assert_eq!(minutes("03:00"), Ok(180));
        assert_eq!(minutes("23:59"), Ok(1439));
        for bad in ["3:00", "24:00", "03:60", "0300", "", "ab:cd"] {
            assert!(minutes(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_window_allows_only_its_own_minutes_even_across_midnight() {
        let w = |f: &str, t: &str| Policy {
            auto_check: true,
            mode: Mode::Window {
                from: f.into(),
                to: t.into(),
            },
        };
        let night = w("03:00", "05:00");
        assert!(!night.allows(179));
        assert!(night.allows(180));
        assert!(night.allows(299));
        assert!(!night.allows(300));
        let across = w("23:00", "01:00");
        assert!(across.allows(23 * 60 + 30));
        assert!(across.allows(30));
        assert!(!across.allows(60));
        assert!(!across.allows(12 * 60));
        assert!(w("03:00", "03:00").validate().is_err());
        assert!(
            !Policy::default().allows(180),
            "by hand never installs by itself"
        );
        let any = Policy {
            auto_check: true,
            mode: Mode::NothingLost,
        };
        assert!(any.allows(0) && any.allows(1439));
    }

    #[test]
    fn the_policy_reads_and_writes_as_settings_sends_it() {
        let p: Policy = serde_json::from_str(
            r#"{"auto_check":true,"mode":"window","from":"03:00","to":"05:00"}"#,
        )
        .unwrap();
        assert_eq!(
            p.mode,
            Mode::Window {
                from: "03:00".into(),
                to: "05:00".into()
            }
        );
        let p: Policy = serde_json::from_str(r#"{"auto_check":false,"mode":"manual"}"#).unwrap();
        assert_eq!(p.mode, Mode::Manual);
        assert_eq!(
            serde_json::to_value(Policy {
                auto_check: true,
                mode: Mode::NothingLost
            })
            .unwrap(),
            serde_json::json!({"auto_check": true, "mode": "nothing_lost"})
        );
    }
}
