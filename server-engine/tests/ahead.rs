//! What the service-app engine adds to the relay's helper (docs/self-hosting-service-apps.md,
//! section 3): the download ahead of time, the automatic or by-hand choice, the signed by-hand
//! flag, and the product's own steps around an update.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{
    Behaves, Channel, FakeDocker, Key, digest, manifest, settings, signed_manifest, updater,
};
use server_engine::docker::{Engine, Service};
use server_engine::{Hooks, Mode, NoHooks, Policy, State, Status, Trigger, Updater};

const APP: &str = "warpcore";
/// 04:00, inside a 03:00–05:00 window.
const IN: u16 = 4 * 60;
/// 12:00, outside it.
const OUT: u16 = 12 * 60;

fn img(svc: &str, c: char) -> String {
    format!("ghcr.io/snds/warpcore-{svc}@{}", digest(c))
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Warpcore 1.0.0 as web and sandbox; 1.1.0 (`b`) in the registry.
fn warpcore(docker: &FakeDocker, sandbox_selftest: i64) {
    docker.publish_image(&img("web", 'a'), "1.0.0", Behaves::Healthy);
    docker.publish_image(&img("web", 'b'), "1.1.0", Behaves::Healthy);
    docker.publish_image(&img("sandbox", 'a'), "1.0.0", Behaves::Healthy);
    docker.publish_image_with(
        &img("sandbox", 'b'),
        "1.1.0",
        Behaves::Healthy,
        sandbox_selftest,
    );
    docker.run("warpcore-sandbox-1", APP, "sandbox", &img("sandbox", 'a'));
    docker.run("warpcore-web-1", APP, "web", &img("web", 'a'));
}

fn release(key: &Key, version: &str, by_hand: Option<bool>, c: char) -> String {
    let images = [("sandbox", img("sandbox", c)), ("web", img("web", c))];
    let images: Vec<(&str, &str)> = images.iter().map(|(s, i)| (*s, i.as_str())).collect();
    match by_hand {
        Some(b) => signed_manifest(key, APP, version, b, &images),
        None => manifest(key, APP, version, &images),
    }
}

fn running(docker: &FakeDocker, name: &str) -> Option<String> {
    docker.with(|w| w.ctr(name).filter(|c| c.running).map(|c| c.image.clone()))
}

fn wait(u: &Updater) -> Status {
    let began = Instant::now();
    loop {
        let st = u.status();
        if !matches!(
            st.state,
            State::Updating | State::RollingBack | State::Waiting
        ) {
            return st;
        }
        assert!(
            began.elapsed() < Duration::from_secs(20),
            "still {:?}",
            st.state
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Warpcore's steps, recorded: what is running, the backup, the self-test, and holding new work.
#[derive(Clone, Default)]
struct Steps {
    log: Arc<Mutex<Vec<String>>>,
    blockers: Arc<Mutex<Vec<String>>>,
    backup_fails: bool,
}

impl Steps {
    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    fn busy(&self, tasks: &[&str]) {
        *self.blockers.lock().unwrap() = tasks.iter().map(|t| (*t).to_owned()).collect();
    }
}

impl Hooks for Steps {
    fn blockers(&self) -> Result<Vec<String>, String> {
        Ok(self.blockers.lock().unwrap().clone())
    }
    fn before(&self, from: Option<&str>, to: &str) -> Result<Option<String>, String> {
        self.log
            .lock()
            .unwrap()
            .push(format!("before {} -> {to}", from.unwrap_or("?")));
        if self.backup_fails {
            return Err("pg_dump: connection refused".into());
        }
        Ok(Some("2026-11-20T120000Z".into()))
    }
    fn after(
        &self,
        engine: &dyn Engine,
        services: &[Service],
        wait: Duration,
    ) -> Result<(), String> {
        self.log.lock().unwrap().push("after".into());
        let sandbox = services.iter().find(|s| s.service == "sandbox").unwrap();
        let (code, out) = engine.run_once(&sandbox.name, &["selftest".into()], wait)?;
        if code == 0 {
            Ok(())
        } else {
            Err(format!("exit {code}: {}", out.trim()))
        }
    }
    fn done(&self) {
        self.log.lock().unwrap().push("done".into());
    }
}

#[test]
fn a_newer_release_is_downloaded_ahead_of_time_without_touching_what_runs() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);

    let st = u.check().unwrap();
    assert_eq!(st.state, State::Available);
    assert_eq!(st.offered.as_deref(), Some("1.1.0"));
    assert!(st.downloaded, "{:?}", st.error);
    assert_eq!(st.by_hand, Some(false));
    docker.with(|w| {
        // Both images are here, by digest, and nothing was made, stopped or restarted.
        assert_eq!(w.pulls, vec![img("sandbox", 'b'), img("web", 'b')]);
        assert!(w.images.contains_key(&img("web", 'b')));
        assert!(w.created.is_empty());
        assert!(
            !w.calls
                .iter()
                .any(|c| c.contains("/stop") || c.contains("/start") || c.starts_with("DELETE"))
        );
    });
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));

    // A second check pulls nothing more, and the update itself pulls nothing either.
    assert!(u.check().unwrap().downloaded);
    let st = u.update(Trigger::ByHand).unwrap();
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    assert_eq!(docker.with(|w| w.pulls.len()), 2);
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'b')));
}

#[test]
fn by_hand_waits_for_the_person_and_automatic_waits_for_an_allowed_moment() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);

    // The default is by hand: ticks check and download, and never install.
    assert_eq!(u.status().policy, Policy::default());
    u.tick_at(now(), IN);
    let st = u.status();
    assert_eq!(st.state, State::Available);
    assert!(st.downloaded);
    u.tick_at(now(), IN);
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));

    // Automatic in a window: outside it, nothing; inside it, it installs by itself.
    u.set_policy(Policy {
        auto_check: true,
        mode: Mode::Window {
            from: "03:00".into(),
            to: "05:00".into(),
        },
    })
    .unwrap();
    u.tick_at(now(), OUT);
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));
    u.tick_at(now(), IN);
    let st = u.status();
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'b')));
    assert!(st.history[0].automatic);
    assert_eq!(st.history[0].result, "updated");
    // A bad window is refused.
    assert!(
        u.set_policy(Policy {
            auto_check: true,
            mode: Mode::Window {
                from: "3am".into(),
                to: "05:00".into()
            }
        })
        .is_err()
    );
}

#[test]
fn when_nothing_would_be_lost_means_no_running_task_at_that_moment() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let steps = Steps::default();
    let u = updater(
        settings(APP, &channel.url, &key.public()),
        &docker,
        steps.clone(),
    );
    u.set_policy(Policy {
        auto_check: true,
        mode: Mode::NothingLost,
    })
    .unwrap();

    steps.busy(&["Task 12 (holds a VM)"]);
    u.tick_at(now(), OUT);
    u.tick_at(now(), OUT);
    let st = u.status();
    assert_eq!(st.state, State::Available);
    assert!(st.downloaded);
    assert!(
        steps.log().is_empty(),
        "no backup is taken while a task runs"
    );
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));

    steps.busy(&[]);
    u.tick_at(now(), OUT);
    let st = u.status();
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    assert_eq!(st.history[0].backup.as_deref(), Some("2026-11-20T120000Z"));
}

#[test]
fn restart_to_update_waits_for_running_tasks_and_names_them() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let steps = Steps::default();
    let u = updater(
        settings(APP, &channel.url, &key.public()),
        &docker,
        steps.clone(),
    );
    steps.busy(&["Task 12 (holds a VM)"]);
    u.start_update(Trigger::ByHand).unwrap();
    let began = Instant::now();
    let st = loop {
        let st = u.status();
        if st.state == State::Waiting {
            break st;
        }
        assert!(began.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(st.blockers, vec!["Task 12 (holds a VM)"]);
    assert!(steps.log().is_empty());
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));
    steps.busy(&[]);
    let st = wait(&u);
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    assert!(st.blockers.is_empty());
    assert_eq!(steps.log(), vec!["before 1.0.0 -> 1.1.0", "after", "done"]);
}

#[test]
fn by_hand_releases_and_unsigned_flags_never_install_by_themselves() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    u.set_policy(Policy {
        auto_check: true,
        mode: Mode::NothingLost,
    })
    .unwrap();

    // Signed as needing a step by hand: offered, never installed from here.
    channel.publish(release(&key, "1.1.0", Some(true), 'b'));
    u.tick_at(now(), IN);
    let st = u.status();
    assert_eq!((st.state, st.by_hand), (State::Available, Some(true)));
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("needs a step on the server")
    );
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));

    // No release signature (an older CI): by hand only.
    channel.publish(release(&key, "1.1.0", None, 'b'));
    assert_eq!(u.check().unwrap().by_hand, None);
    u.tick_at(now() + 7 * 3600, IN);
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));
    assert!(
        u.update(Trigger::Automatic)
            .unwrap_err()
            .contains("accepted by hand")
    );
    assert_eq!(u.update(Trigger::ByHand).unwrap().state, State::Updated);

    // A flag flipped after signing: the whole release is refused.
    let mut flipped: serde_json::Value =
        serde_json::from_str(&release(&key, "1.2.0", Some(true), 'c')).unwrap();
    flipped["byHand"] = false.into();
    channel.publish(flipped.to_string());
    assert!(u.check().unwrap_err().contains("doesn't verify"));
    // And so is a signature without the flag it covers.
    let mut dropped: serde_json::Value =
        serde_json::from_str(&release(&key, "1.2.0", Some(false), 'c')).unwrap();
    dropped.as_object_mut().unwrap().remove("byHand");
    channel.publish(dropped.to_string());
    assert!(u.check().is_err());
}

#[test]
fn a_failed_backup_changes_nothing() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let steps = Steps {
        backup_fails: true,
        ..Steps::default()
    };
    let u = updater(
        settings(APP, &channel.url, &key.public()),
        &docker,
        steps.clone(),
    );
    let e = u.update(Trigger::ByHand).unwrap_err();
    assert!(
        e.contains("Nothing changed") && e.contains("connection refused"),
        "{e}"
    );
    docker.with(|w| assert!(w.created.is_empty()));
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));
    assert_eq!(
        steps.log(),
        vec!["before 1.0.0 -> 1.1.0", "done"],
        "work may start again"
    );
    assert_eq!(u.status().history[0].result, "failed");
}

#[test]
fn a_failed_self_test_rolls_every_service_back() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 3);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let steps = Steps::default();
    let u = updater(
        settings(APP, &channel.url, &key.public()),
        &docker,
        steps.clone(),
    );
    let st = u.update(Trigger::ByHand).unwrap();
    assert_eq!(st.state, State::RolledBack);
    let e = st.error.unwrap();
    assert!(
        e.contains("self-test failed") && e.contains("exit 3"),
        "{e}"
    );
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));
    assert_eq!(
        running(&docker, "warpcore-sandbox-1"),
        Some(img("sandbox", 'a'))
    );
    assert_eq!(steps.log(), vec!["before 1.0.0 -> 1.1.0", "after", "done"]);
    let r = &st.history[0];
    assert_eq!(
        (r.result.as_str(), r.backup.as_deref()),
        ("rolled_back", Some("2026-11-20T120000Z"))
    );
    docker.with(|w| {
        // The self-test ran once, in a copy of the new sandbox with its own limits, publishing
        // nothing and not counted as a service, and it is gone.
        assert_eq!(w.selftests.len(), 1);
        let t = &w.selftests[0];
        assert_eq!(t["name"], "warpcore-sandbox-1-once");
        assert_eq!(t["image"], img("sandbox", 'b'));
        assert_eq!(t["host"]["Binds"][0], "/mnt/user/appdata/warpcore:/data");
        assert!(t["host"].get("PortBindings").is_none());
        assert!(t["config"]["Labels"].get("org.snds.update.app").is_none());
        assert!(w.ctr("warpcore-sandbox-1-once").is_none());
        assert_eq!(w.ctrs.len(), 2);
    });
}

#[test]
fn a_passing_self_test_keeps_the_previous_containers_for_rollback() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let u = updater(
        settings(APP, &channel.url, &key.public()),
        &docker,
        Steps::default(),
    );
    let st = u.update(Trigger::ByHand).unwrap();
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    assert_eq!(st.previous.as_deref(), Some("1.0.0"));
    docker.with(|w| {
        assert!(w.ctr("warpcore-web-1-previous").is_some_and(|c| !c.running));
        assert!(
            w.ctr("warpcore-sandbox-1-previous")
                .is_some_and(|c| !c.running)
        );
    });
    let st = u.rollback().unwrap();
    assert_eq!(st.state, State::RolledBack);
    docker.with(|w| {
        assert!(w.ctr("warpcore-web-1-previous").is_none());
        assert!(w.ctr("warpcore-sandbox-1-previous").is_none());
    });
    assert_eq!(running(&docker, "warpcore-web-1"), Some(img("web", 'a')));
}

#[test]
fn the_choice_and_history_survive_a_restart_of_the_service_app() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, 0);
    channel.publish(release(&key, "1.1.0", Some(false), 'b'));
    let store = Arc::new(server_engine::engine::MemoryStore::default());
    struct Shared(Arc<server_engine::engine::MemoryStore>);
    impl server_engine::Store for Shared {
        fn load(&self) -> server_engine::Saved {
            self.0.load()
        }
        fn save(&self, s: &server_engine::Saved) -> Result<(), String> {
            self.0.save(s)
        }
    }
    let make = || {
        Updater::new(
            settings(APP, &channel.url, &key.public()),
            docker.engine(),
            Box::new(NoHooks),
            Box::new(Shared(store.clone())),
        )
    };
    let u = make();
    let p = Policy {
        auto_check: false,
        mode: Mode::NothingLost,
    };
    u.set_policy(p.clone()).unwrap();
    u.update(Trigger::ByHand).unwrap();
    let again = make().status();
    assert_eq!(again.policy, p);
    assert_eq!(again.history.len(), 1);
    // With checking off, a tick doesn't check.
    let fresh = make();
    fresh.tick_at(now(), IN);
    assert_eq!(fresh.status().checked, None);
}
