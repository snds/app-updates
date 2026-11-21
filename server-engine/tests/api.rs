//! The service app's HTTP API: anyone on its networks may read the status; only the product,
//! with its token, may check, update, roll back or change the choice.

mod common;

use std::sync::Arc;
use std::time::Duration;

use common::{Behaves, Channel, FakeDocker, Key, digest, settings, signed_manifest, updater};
use server_engine::NoHooks;
use server_engine::shell::Api;

const APP: &str = "warpcore";
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn img(svc: &str, c: char) -> String {
    format!("ghcr.io/snds/warpcore-{svc}@{}", digest(c))
}

fn call(
    base: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: &str,
) -> (u16, serde_json::Value) {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(10)))
        .build()
        .into();
    let url = format!("{base}{path}");
    let mut resp = match method {
        "GET" => {
            let mut r = agent.get(&url);
            if let Some(t) = token {
                r = r.header("Authorization", &format!("Bearer {t}"));
            }
            r.call().unwrap()
        }
        _ => {
            let mut r = if method == "PUT" {
                agent.put(&url)
            } else {
                agent.post(&url)
            };
            if let Some(t) = token {
                r = r.header("Authorization", &format!("Bearer {t}"));
            }
            r.send(body.to_owned()).unwrap()
        }
    };
    let code = resp.status().as_u16();
    let text = resp.body_mut().read_to_string().unwrap();
    (
        code,
        serde_json::from_str(&text).unwrap_or(serde_json::Value::Null),
    )
}

fn start(token: Option<&str>) -> (String, Channel, FakeDocker) {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    docker.publish_image(&img("web", 'a'), "1.0.0", Behaves::Healthy);
    docker.publish_image(&img("web", 'b'), "1.1.0", Behaves::Healthy);
    docker.run("warpcore-web-1", APP, "web", &img("web", 'a'));
    channel.publish(signed_manifest(
        &key,
        APP,
        "1.1.0",
        false,
        &[("web", &img("web", 'b'))],
    ));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base = format!("http://{}", server.server_addr().to_ip().unwrap());
    let api = Arc::new(Api::new(u, token.map(str::to_owned)));
    std::thread::spawn(move || api.serve_on(server));
    (base, channel, docker)
}

#[test]
fn the_status_is_open_and_every_change_needs_the_token() {
    let (base, _c, docker) = start(Some(TOKEN));
    let (code, st) = call(&base, "GET", "/v1/status", None, "");
    assert_eq!(code, 200);
    assert_eq!(st["app"], APP);
    assert_eq!(st["version"], "1.0.0");
    assert_eq!(call(&base, "GET", "/healthz", None, "").0, 200);

    for (m, p) in [
        ("POST", "/v1/check"),
        ("POST", "/v1/update"),
        ("POST", "/v1/rollback"),
        ("PUT", "/v1/policy"),
    ] {
        assert_eq!(call(&base, m, p, None, "{}").0, 401, "{m} {p}");
        assert_eq!(
            call(&base, m, p, Some("0123456789abcdef0123456789abcdee"), "{}").0,
            401,
            "{m} {p}"
        );
    }
    assert!(
        docker.with(|w| w.pulls.is_empty()),
        "nothing happened without the token"
    );

    let (code, st) = call(&base, "POST", "/v1/check", Some(TOKEN), "");
    assert_eq!(code, 200);
    assert_eq!(
        (st["state"].as_str(), st["downloaded"].as_bool()),
        (Some("available"), Some(true))
    );

    let (code, st) = call(
        &base,
        "PUT",
        "/v1/policy",
        Some(TOKEN),
        r#"{"auto_check":true,"mode":"window","from":"03:00","to":"05:00"}"#,
    );
    assert_eq!(code, 200, "{st}");
    assert_eq!(st["policy"]["mode"], "window");
    let (code, _) = call(
        &base,
        "PUT",
        "/v1/policy",
        Some(TOKEN),
        r#"{"auto_check":true,"mode":"sometimes"}"#,
    );
    assert_eq!(code, 409);

    let (code, st) = call(&base, "POST", "/v1/update", Some(TOKEN), "");
    assert_eq!((code, st["state"].as_str()), (202, Some("updating")));
    let began = std::time::Instant::now();
    loop {
        let (_, st) = call(&base, "GET", "/v1/status", None, "");
        if st["state"] == "updated" {
            assert_eq!(st["version"], "1.1.0");
            break;
        }
        assert!(began.elapsed() < Duration::from_secs(10), "{st}");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(call(&base, "GET", "/v1/nothing", None, "").0, 404);
}

#[test]
fn a_short_or_missing_token_turns_every_change_off() {
    for token in [None, Some("short")] {
        let (base, _c, _d) = start(token);
        assert_eq!(call(&base, "GET", "/v1/status", None, "").0, 200);
        assert_eq!(call(&base, "POST", "/v1/check", Some("short"), "").0, 401);
        assert_eq!(call(&base, "POST", "/v1/check", Some(""), "").0, 401);
    }
}

#[test]
fn a_first_install_runs_setup_with_the_signed_release_and_only_when_nothing_is_installed() {
    let key = Key::new(7);
    let channel = Channel::start();
    let docker = FakeDocker::start();
    channel.publish(signed_manifest(
        &key,
        APP,
        "1.1.0",
        false,
        &[
            ("web", &img("web", 'b')),
            ("artifacts", &img("artifacts", 'b')),
        ],
    ));
    let u = updater(settings(APP, &channel.url, &key.public()), &docker, NoHooks);
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("env.txt");
    let setup = server_engine::shell::Setup {
        command: vec![
            "sh".into(),
            "-c".into(),
            format!("env > '{}'", out.display()),
        ],
    };
    assert!(server_engine::shell::setup_if_needed(&u, &setup).unwrap());
    let env = std::fs::read_to_string(&out).unwrap();
    assert!(env.contains("SETUP_VERSION=1.1.0"), "{env}");
    assert!(env.contains(&format!("SETUP_IMAGE_WEB={}", img("web", 'b'))));
    assert!(env.contains(&format!("SETUP_IMAGE_ARTIFACTS={}", img("artifacts", 'b'))));
    assert!(
        !env.contains("SERVICE_APP_TOKEN"),
        "setup gets no token it wasn't given"
    );

    // Once a service is there, setup never runs again.
    std::fs::remove_file(&out).unwrap();
    docker.publish_image(&img("web", 'b'), "1.1.0", Behaves::Healthy);
    docker.run("warpcore-web-1", APP, "web", &img("web", 'b'));
    assert!(!server_engine::shell::setup_if_needed(&u, &setup).unwrap());
    assert!(!out.exists());

    // A release that isn't signed by the key is never set up from.
    let docker2 = FakeDocker::start();
    channel.publish(signed_manifest(
        &Key::new(9),
        APP,
        "1.1.0",
        false,
        &[("web", &img("web", 'b'))],
    ));
    let u2 = updater(
        settings(APP, &channel.url, &key.public()),
        &docker2,
        NoHooks,
    );
    assert!(server_engine::shell::setup_if_needed(&u2, &setup).is_err());
    assert!(!out.exists());
}
