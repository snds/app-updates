//! X15e and LS18, ported from Synapse's `relay/tests/x15e.rs`: the engine updates Warpcore to a new
//! signed release, then rolls it back; it installs only releases signed by the channel's key,
//! keeps the last version, rolls back on a failed health check, and never pulls `latest`.
//!
//! Offline: a fake publish channel and a fake Docker Engine API on 127.0.0.1 stand in for GitHub,
//! the registry, Docker and Warpcore.

mod common;

use std::time::{Duration, Instant};

use common::{Behaves, Channel, FakeDocker, Key, digest, manifest, settings, updater};
use server_engine::release;
use server_engine::{NoHooks, State, Status, Trigger, Updater};

const APP: &str = "warpcore";

fn img(svc: &str, c: char) -> String {
    format!("ghcr.io/snds/warpcore-{svc}@{}", digest(c))
}

/// Warpcore 1.0.0 running as one `web` service, 1.1.0 (`b`) published to the registry.
fn warpcore(docker: &FakeDocker, next: Behaves) {
    docker.publish_image(&img("web", 'a'), "1.0.0", Behaves::Healthy);
    docker.publish_image(&img("web", 'b'), "1.1.0", next);
    docker.run("warpcore-web", APP, "web", &img("web", 'a'));
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

fn running_image(docker: &FakeDocker, name: &str) -> Option<String> {
    docker.with(|w| w.ctr(name).filter(|c| c.running).map(|c| c.image.clone()))
}

#[test]
fn x15e_the_engine_updates_warpcore_to_a_new_signed_release_then_rolls_it_back() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    // An unlabelled container on the same server is never touched.
    docker.publish_image(&img("other", 'c'), "3.0.0", Behaves::Runs);
    docker.run("plex", "not-ours", "plex", &img("other", 'c'));
    docker.with(|w| {
        let c = w.ctrs.iter_mut().find(|c| c.name == "plex").unwrap();
        c.config["Labels"] = serde_json::json!({});
    });
    channel.publish(manifest(&key, APP, "1.1.0", &[("web", &img("web", 'b'))]));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);

    let st = u.status();
    assert_eq!((st.app.as_str(), st.name.as_str()), (APP, "Warpcore"));
    assert_eq!(st.version.as_deref(), Some("1.0.0"));
    assert_eq!(st.services, vec!["web"]);
    let st = u.check().unwrap();
    assert_eq!(st.state, State::Available);
    assert_eq!(st.offered.as_deref(), Some("1.1.0"));

    // "Restart to update".
    let st = u.start_update(Trigger::ByHand).unwrap();
    assert_eq!(st.state, State::Updating);
    let st = wait(&u);
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    assert_eq!(st.version.as_deref(), Some("1.1.0"));
    assert_eq!(
        st.previous.as_deref(),
        Some("1.0.0"),
        "the last version is kept"
    );
    assert_eq!(st.offered, None);
    assert_eq!(
        running_image(&docker, "warpcore-web"),
        Some(img("web", 'b'))
    );
    docker.with(|w| {
        // Only the signed digest was pulled, once.
        assert_eq!(w.pulls, vec![img("web", 'b')]);
        let kept = w.ctr("warpcore-web-previous").unwrap();
        assert!(!kept.running);
        assert_eq!(kept.image, img("web", 'a'));
        // The new container has the old one's own settings and nothing more.
        let (name, body) = w.created.last().unwrap().clone();
        assert_eq!(name, "warpcore-web");
        assert_eq!(body["Image"], img("web", 'b'));
        assert_eq!(
            body["HostConfig"]["Binds"][0],
            "/mnt/user/appdata/warpcore:/data"
        );
        assert_eq!(
            body["HostConfig"]["PortBindings"]["8080/tcp"][0]["HostPort"],
            "8080"
        );
        assert!(body["HostConfig"].get("Privileged").is_none());
        let env: Vec<&str> = body["Env"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(env, vec!["WARP_SECRET_REF=sy://warpcore/db"]);
        assert_eq!(body["Labels"]["org.snds.update.version"], "1.1.0");
        assert_eq!(
            body["NetworkingConfig"]["EndpointsConfig"]["warp"]["Aliases"],
            serde_json::json!(["web"])
        );
        assert!(body.get("Hostname").is_none());
        assert!(w.ctr("plex").unwrap().running);
        assert!(!w.calls.iter().any(|c| c.contains("plex")));
    });
    assert_eq!(u.status().history[0].result, "updated");

    assert_eq!(u.check().unwrap().state, State::UpToDate);

    // "Roll back".
    assert_eq!(u.start_rollback().unwrap().state, State::RollingBack);
    let st = wait(&u);
    assert_eq!(st.state, State::RolledBack, "{:?}", st.error);
    assert_eq!(st.version.as_deref(), Some("1.0.0"));
    assert_eq!(st.previous, None);
    assert_eq!(
        running_image(&docker, "warpcore-web"),
        Some(img("web", 'a'))
    );
    assert!(docker.with(|w| w.ctr("warpcore-web-previous").is_none()));
    assert_eq!(
        docker.with(|w| w.pulls.len()),
        1,
        "a rollback pulls nothing"
    );
    assert_eq!(st.history[0].result, "rollback");
    assert_eq!(u.check().unwrap().state, State::Available);
}

#[test]
fn ls18_a_failed_health_check_rolls_every_service_back_by_itself() {
    for bad in [Behaves::Unhealthy, Behaves::Exits] {
        let key = Key::new(7);
        let channel = Channel::start();
        let docker = FakeDocker::start();
        // Two services: the api one updates well, the web one fails after it.
        docker.publish_image(&img("api", 'a'), "1.0.0", Behaves::Runs);
        docker.publish_image(&img("api", 'b'), "1.1.0", Behaves::Healthy);
        docker.publish_image(&img("web", 'a'), "1.0.0", Behaves::Healthy);
        docker.publish_image(&img("web", 'b'), "1.1.0", bad);
        docker.run("warpcore-api", APP, "api", &img("api", 'a'));
        docker.run("warpcore-web", APP, "web", &img("web", 'a'));
        channel.publish(manifest(
            &key,
            APP,
            "1.1.0",
            &[("api", &img("api", 'b')), ("web", &img("web", 'b'))],
        ));
        let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
        let st = u.update(Trigger::ByHand).unwrap();
        assert_eq!(st.state, State::RolledBack, "{bad:?}");
        assert!(
            st.error.as_deref().unwrap().contains("1.1.0"),
            "{:?}",
            st.error
        );
        assert_eq!(st.version.as_deref(), Some("1.0.0"));
        assert_eq!(
            running_image(&docker, "warpcore-api"),
            Some(img("api", 'a'))
        );
        assert_eq!(
            running_image(&docker, "warpcore-web"),
            Some(img("web", 'a'))
        );
        docker.with(|w| {
            assert_eq!(w.ctrs.len(), 2, "no new or kept container is left behind");
            assert_eq!(w.pulls, vec![img("api", 'b'), img("web", 'b')]);
        });
        assert_eq!(st.history[0].result, "rolled_back");
    }
}

#[test]
fn ls18_a_release_not_signed_by_the_channel_key_is_never_pulled() {
    let (key, other) = (Key::new(7), Key::new(9));
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);

    channel.publish(manifest(&other, APP, "1.1.0", &[("web", &img("web", 'b'))]));
    let e = u.check().unwrap_err();
    assert!(e.contains("doesn't verify"), "{e}");
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("doesn't verify")
    );

    let good: serde_json::Value =
        serde_json::from_str(&manifest(&key, APP, "1.1.0", &[("web", &img("web", 'b'))])).unwrap();
    let mut moved = good.clone();
    moved["version"] = "9.0.0".into();
    channel.publish(moved.to_string());
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("doesn't verify")
    );
    let mut moved = good.clone();
    moved["platforms"]["server-web"]["url"] = img("web", 'c').into();
    channel.publish(moved.to_string());
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("doesn't verify")
    );
    channel.publish(manifest(
        &key,
        "vale-gateway",
        "1.1.0",
        &[("web", &img("web", 'b'))],
    ));
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("doesn't verify")
    );

    let mut unsigned = good.clone();
    unsigned["platforms"]["server-web"]["signature"] = "".into();
    channel.publish(unsigned.to_string());
    assert!(u.update(Trigger::ByHand).is_err());

    let mut half = good.clone();
    half["platforms"]["server-api"] =
        serde_json::json!({ "url": img("api", 'b'), "signature": other.sign(b"x") });
    channel.publish(half.to_string());
    assert!(u.update(Trigger::ByHand).is_err());

    // A check that fails downloads nothing either.
    assert!(
        docker.with(|w| w.pulls.is_empty()),
        "nothing unsigned is ever pulled"
    );
    assert_eq!(
        running_image(&docker, "warpcore-web"),
        Some(img("web", 'a'))
    );
    assert_eq!(u.status().state, State::Failed);
}

#[test]
fn ls18_it_never_pulls_latest_or_any_tag() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    for tagged in [
        "ghcr.io/snds/warpcore-web:latest".to_owned(),
        "ghcr.io/snds/warpcore-web".to_owned(),
        format!("ghcr.io/snds/warpcore-web:1.1.0@{}", digest('b')),
    ] {
        channel.publish(manifest(&key, APP, "1.1.0", &[("web", &tagged)]));
        let e = u.check().unwrap_err();
        assert!(e.contains("digest") || e.contains("tag"), "{e}");
        let e = u.update(Trigger::ByHand).unwrap_err();
        assert!(e.contains("digest") || e.contains("tag"), "{e}");
    }
    assert!(docker.with(|w| w.pulls.is_empty()));
    use server_engine::docker::Engine;
    assert!(
        docker
            .engine()
            .pull("ghcr.io/snds/warpcore-web:latest")
            .is_err()
    );
    assert!(
        docker
            .engine()
            .has_image("ghcr.io/snds/warpcore-web:latest")
            .is_err()
    );
    assert!(docker.with(|w| w.pulls.is_empty()));
}

#[test]
fn only_a_newer_release_is_offered_or_installed() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    for v in ["1.0.0", "0.9.0", "1.0.0-main.20261116000000"] {
        channel.publish(manifest(&key, APP, v, &[("web", &img("web", 'b'))]));
        assert_eq!(u.check().unwrap().state, State::UpToDate, "{v}");
        assert_eq!(
            u.update(Trigger::ByHand).unwrap().state,
            State::UpToDate,
            "{v}"
        );
    }
    assert!(docker.with(|w| w.pulls.is_empty()));
    assert!(u.rollback().unwrap_err().contains("no earlier version"));
}

#[test]
fn a_release_must_match_the_services_this_server_runs() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    channel.publish(manifest(&key, APP, "1.1.0", &[("api", &img("api", 'b'))]));
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("no image for its web service")
    );
    channel.publish(manifest(
        &key,
        APP,
        "1.1.0",
        &[("web", &img("web", 'b')), ("worker", &img("worker", 'b'))],
    ));
    assert!(
        u.update(Trigger::ByHand)
            .unwrap_err()
            .contains("adds a worker service")
    );
    assert!(docker.with(|w| w.pulls.is_empty()));
}

#[test]
fn optional_services_and_the_service_app_itself_may_be_in_a_release_but_arent_swapped() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    // The service app's own container, labelled for the product, as `server`.
    docker.publish_image(&img("server", 'a'), "1.0.0", Behaves::Runs);
    docker.run("warpcore-server", APP, "server", &img("server", 'a'));
    channel.publish(manifest(
        &key,
        APP,
        "1.1.0",
        &[
            ("web", &img("web", 'b')),
            ("artifacts", &img("artifacts", 'b')),
            ("server", &img("server", 'b')),
        ],
    ));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    let st = u.check().unwrap();
    assert_eq!(
        st.services,
        vec!["web"],
        "the service app isn't one of the services it swaps"
    );
    assert_eq!(st.supervisor_offered.as_deref(), Some("1.1.0"));
    let st = u.update(Trigger::ByHand).unwrap();
    assert_eq!(st.state, State::Updated, "{:?}", st.error);
    docker.with(|w| {
        assert_eq!(
            w.pulls,
            vec![img("web", 'b')],
            "only what this server runs is pulled"
        );
        assert_eq!(w.ctr("warpcore-server").unwrap().image, img("server", 'a'));
        assert!(w.ctr("warpcore-server").unwrap().running);
    });
}

#[test]
fn a_failed_pull_changes_nothing() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    channel.publish(manifest(&key, APP, "1.2.0", &[("web", &img("web", 'd'))]));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    let st = u.check().unwrap();
    assert_eq!(st.state, State::Available);
    assert!(!st.downloaded);
    assert!(st.error.unwrap().contains("not found"));
    assert!(u.update(Trigger::ByHand).unwrap_err().contains("not found"));
    assert_eq!(
        running_image(&docker, "warpcore-web"),
        Some(img("web", 'a'))
    );
    assert!(docker.with(|w| w.created.is_empty() && w.ctrs.len() == 1));
}

#[test]
fn the_channel_is_https_on_its_hosts_and_redirects_are_checked() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    let away = channel.url.replace("/latest.json", "/away");
    let u = updater(settings(APP, &away, &key.public()), &docker, NoHooks);
    assert!(
        u.check()
            .unwrap_err()
            .contains("don't come from evil.example")
    );
    let mut s = settings(APP, &channel.url, &key.public());
    s.loopback_feeds = false;
    let u = updater(s, &docker, NoHooks);
    assert!(u.check().unwrap_err().contains("https"));
}

#[test]
fn with_no_key_built_in_updates_are_off() {
    let channel = Channel::start();
    let docker = FakeDocker::start();
    warpcore(&docker, Behaves::Healthy);
    let u = updater(settings(APP, &channel.url, ""), &docker, NoHooks);
    let st = u.status();
    assert_eq!(st.state, State::Off);
    assert!(st.off.unwrap().contains("no key"));
    assert_eq!(st.version.as_deref(), Some("1.0.0"));
    assert!(u.update(Trigger::ByHand).is_err());
    assert!(u.start_update(Trigger::ByHand).is_err());
}

#[test]
fn a_signature_from_tauri_signer_verifies() {
    // Made once with `tauri signer generate` (a throwaway key; only its public half is kept) and
    // `tauri signer sign` over the statement for warpcore 1.1.0 web
    // ghcr.io/snds/warpcore-web@sha256:b…b. This is the pre-hashed form CI's signer makes.
    let key = include_str!("fixtures/tauri-signer-test.pub").trim();
    let sig = include_str!("fixtures/tauri-signer-statement.sig").trim();
    let image = format!("ghcr.io/snds/warpcore-web@{}", digest('b'));
    let text = release::statement(APP, "1.1.0", "web", &image);
    release::verify(key, text.as_bytes(), sig).unwrap();
    assert!(
        release::verify(
            key,
            release::statement(APP, "1.2.0", "web", &image).as_bytes(),
            sig
        )
        .is_err()
    );
    let m: release::Manifest = serde_json::from_value(serde_json::json!({
        "version": "1.1.0",
        "platforms": { "server-web": { "url": image, "signature": sig } }
    }))
    .unwrap();
    let r = release::check(APP, key, &m).unwrap();
    assert_eq!(r.version.to_string(), "1.1.0");
    assert_eq!(r.by_hand, None, "no release signature: by hand only");
}
