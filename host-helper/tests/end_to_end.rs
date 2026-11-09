//! The helper and the client over a real socket, with a fake Docker that records every call.

#![cfg(feature = "server")]

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use host_helper::allow::{AllowList, ListError, Stack, Step};
use host_helper::docker::{Container, Docker, DockerError, ImageInfo, RunOutput};
use host_helper::protocol::{self, H_NONCE, H_SIGNATURE, H_STACK, H_TIME, Key};
use host_helper::server::Helper;
use host_helper::{Client, ClientError, UpdateRequest};

const LIST: &str = r#"
listen = ["127.0.0.1:0"]
keys_dir = "/config/keys"

[stack.lockdown]
directory = "/mnt/user/appdata/lockdown"
services = ["lockdown"]
image = "ghcr.io/snds/lockdown-server"

[stack.vale]
directory = "/mnt/user/appdata/vale-gateway"
services = ["gateway"]
image = "ghcr.io/snds/vale-gateway"

[stack.warpcore]
directory = "/mnt/user/appdata/warpcore/src/deploy"
services = ["web", "api", "sandbox"]
images = ["ghcr.io/snds/warpcore-web", "ghcr.io/snds/warpcore-api", "ghcr.io/snds/warpcore-sandbox"]
before = { service = "backup", command = ["sh", "/backup/backup.sh", "once"] }
after = { service = "sandbox", command = ["node", "dist/selftest.js"], no_deps = true }
"#;

const WARPCORE: [&str; 3] = [
    "ghcr.io/snds/warpcore-web",
    "ghcr.io/snds/warpcore-api",
    "ghcr.io/snds/warpcore-sandbox",
];

/// Warpcore's three repositories, each with its own digest ('c', 'd', 'e').
fn warpcore_digests() -> BTreeMap<String, String> {
    WARPCORE
        .iter()
        .zip(['c', 'd', 'e'])
        .map(|(r, c)| (r.to_string(), digest(c)))
        .collect()
}

fn key_of(stack: &str) -> Key {
    let byte = match stack {
        "lockdown" => "1a",
        "vale" => "2b",
        _ => "3c",
    };
    Key::parse(&byte.repeat(32)).unwrap()
}

fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

#[derive(Default)]
struct Fake {
    calls: Mutex<Vec<String>>,
    /// The digest a pull produces.
    pulled: Mutex<String>,
    /// When set, compose_up waits for a message before returning.
    hold: Mutex<Option<mpsc::Receiver<()>>>,
    /// Per-repository digests, overriding `pulled`.
    digests: Mutex<HashMap<String, String>>,
    /// Exit status of a one-shot run, per service (0 when unset).
    exits: Mutex<HashMap<String, i32>>,
}

#[derive(Clone, Default)]
struct FakeDocker(Arc<Fake>);

impl FakeDocker {
    fn calls(&self) -> Vec<String> {
        self.0.calls.lock().unwrap().clone()
    }
    fn record(&self, call: String) {
        self.0.calls.lock().unwrap().push(call);
    }
}

impl Docker for FakeDocker {
    fn pull(&self, reference: &str) -> Result<(), DockerError> {
        self.record(format!("pull {reference}"));
        Ok(())
    }
    fn image(&self, reference: &str) -> Result<ImageInfo, DockerError> {
        self.record(format!("image {reference}"));
        // `repository:tag`, or a local image ID (only Lockdown's status asks for one here).
        let repo = match reference.rsplit_once(':') {
            Some((repo, _)) if repo.starts_with("ghcr.io/") => repo,
            _ => "ghcr.io/snds/lockdown-server",
        };
        let d = self
            .0
            .digests
            .lock()
            .unwrap()
            .get(repo)
            .cloned()
            .unwrap_or_else(|| self.0.pulled.lock().unwrap().clone());
        Ok(ImageInfo {
            repo_digests: vec![format!("{repo}@{d}")],
            version: Some("0.1.0-main.20261108093000".into()),
        })
    }
    fn tag(&self, from: &str, to: &str) -> Result<(), DockerError> {
        self.record(format!("tag {from} {to}"));
        Ok(())
    }
    fn compose_up(&self, stack: &Stack, services: &[String]) -> Result<(), DockerError> {
        self.record(format!("up {} {}", stack.name, services.join(",")));
        let hold = self.0.hold.lock().unwrap().take();
        if let Some(rx) = hold {
            let _ = rx.recv_timeout(Duration::from_secs(10));
        }
        Ok(())
    }
    fn compose_restart(&self, stack: &Stack, services: &[String]) -> Result<(), DockerError> {
        self.record(format!("restart {} {}", stack.name, services.join(",")));
        Ok(())
    }
    fn compose_run(&self, stack: &Stack, step: &Step) -> Result<RunOutput, DockerError> {
        self.record(format!(
            "run {} {}{} {}",
            stack.name,
            step.service,
            if step.no_deps { " no-deps" } else { "" },
            step.command.join(" ")
        ));
        let code = self
            .0
            .exits
            .lock()
            .unwrap()
            .get(&step.service)
            .copied()
            .unwrap_or(0);
        Ok(RunOutput {
            exit_code: Some(code),
            output: format!("{} said {code}", step.service),
        })
    }
    fn containers(&self, stack: &Stack, service: &str) -> Result<Vec<Container>, DockerError> {
        self.record(format!("ps {} {service}", stack.name));
        Ok(vec![Container {
            state: "running".into(),
            config_image: format!("{}:main", stack.repository()),
            image_id: "sha256:local".into(),
        }])
    }
}

struct Running {
    url: String,
    docker: FakeDocker,
    log: Arc<Mutex<Vec<String>>>,
}

fn start() -> Running {
    let list = AllowList::parse(LIST, |stack: &str, _: &Path| -> Result<Key, ListError> {
        Ok(key_of(stack))
    })
    .unwrap();
    let docker = FakeDocker::default();
    *docker.0.pulled.lock().unwrap() = digest('a');
    docker.0.digests.lock().unwrap().extend(warpcore_digests());
    let log = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&log);
    let helper = Arc::new(Helper::new(list, docker.clone()).with_clock_and_log(
        || {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
        },
        move |line: &str| sink.lock().unwrap().push(line.to_owned()),
    ));
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let helper = Arc::clone(&helper);
            std::thread::spawn(move || {
                let mut stream = stream;
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                helper.serve_connection(&mut stream, "127.0.0.1");
            });
        }
    });
    Running { url, docker, log }
}

fn client(r: &Running, stack: &str) -> Client {
    Client::new(&r.url, stack, key_of(stack)).unwrap()
}

fn refused(e: ClientError) -> (u16, String, String) {
    match e {
        ClientError::Refused {
            status,
            code,
            message,
        } => (status, code, message),
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// Sends a hand-made request and returns the status line and body.
fn raw(url: &str, head: &str, body: &str) -> (u16, String) {
    let addr = url.strip_prefix("http://").unwrap();
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "POST {head}content-length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1.to_owned();
    (status, body)
}

fn signed_head(stack: &str, key: &Key, path: &str, time: u64, nonce: &str, body: &str) -> String {
    let sig = protocol::sign(
        key,
        &protocol::request_input("POST", path, stack, time, nonce, body.as_bytes()),
    );
    format!(
        "{path} HTTP/1.1\r\n{H_STACK}: {stack}\r\n{H_TIME}: {time}\r\n{H_NONCE}: {nonce}\r\n{H_SIGNATURE}: {sig}\r\n"
    )
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[test]
fn status_shows_the_running_image() {
    let r = start();
    let s = client(&r, "lockdown").status().unwrap();
    assert_eq!(s.stack, "lockdown");
    assert_eq!(s.repository, "ghcr.io/snds/lockdown-server");
    assert_eq!(s.services.len(), 1);
    assert_eq!(s.services[0].state, "running");
    assert_eq!(
        s.services[0].image.as_deref(),
        Some("ghcr.io/snds/lockdown-server:main")
    );
    assert_eq!(s.services[0].digest, Some(digest('a')));
    assert_eq!(
        s.services[0].version.as_deref(),
        Some("0.1.0-main.20261108093000")
    );
    assert_eq!(
        r.docker.calls(),
        ["ps lockdown lockdown", "image sha256:local"]
    );
}

#[test]
fn update_pulls_checks_the_digest_and_recreates_only_the_allowed_service() {
    let r = start();
    let tag = "0.1.0-main.20261108093000";
    let done = client(&r, "lockdown")
        .update(&UpdateRequest {
            tag: Some(tag.into()),
            expect_digest: Some(digest('a')),
            ..Default::default()
        })
        .unwrap();
    assert_eq!(done.pulled, format!("ghcr.io/snds/lockdown-server:{tag}"));
    assert_eq!(done.digest, digest('a'));
    assert_eq!(done.services, ["lockdown"]);
    assert_eq!(
        r.docker.calls(),
        [
            format!("pull ghcr.io/snds/lockdown-server:{tag}"),
            format!("image ghcr.io/snds/lockdown-server:{tag}"),
            format!(
                "tag ghcr.io/snds/lockdown-server@{} ghcr.io/snds/lockdown-server:main",
                digest('a')
            ),
            "up lockdown lockdown".into(),
        ]
    );
    let log = r.log.lock().unwrap().join("\n");
    assert!(
        log.contains(&format!("stack=lockdown op=update tag={tag} result=ok")),
        "{log}"
    );
}

#[test]
fn update_to_main_needs_no_retag() {
    let r = start();
    client(&r, "vale")
        .update(&UpdateRequest::default())
        .unwrap();
    assert_eq!(
        r.docker.calls(),
        [
            "pull ghcr.io/snds/vale-gateway:main",
            "image ghcr.io/snds/vale-gateway:main",
            "up vale gateway",
        ]
    );
}

#[test]
fn a_different_digest_restarts_nothing() {
    let r = start();
    let (status, code, message) = refused(
        client(&r, "lockdown")
            .update(&UpdateRequest {
                tag: None,
                expect_digest: Some(digest('b')),
                ..Default::default()
            })
            .unwrap_err(),
    );
    assert_eq!((status, code.as_str()), (409, "digest_mismatch"));
    assert!(message.contains("Nothing was restarted"));
    assert!(
        !r.docker
            .calls()
            .iter()
            .any(|c| c.starts_with("up") || c.starts_with("tag"))
    );
}

#[test]
fn tags_outside_the_list_are_refused_before_any_pull() {
    let r = start();
    for tag in ["latest", "0.1.0", "main; rm -rf /", "--all", "../main"] {
        let (status, _, message) = refused(
            client(&r, "lockdown")
                .update(&UpdateRequest {
                    tag: Some(tag.into()),
                    expect_digest: None,
                    ..Default::default()
                })
                .unwrap_err(),
        );
        assert_eq!(status, 403, "{tag}");
        assert!(message.contains("isn't allowed"), "{message}");
    }
    let (status, code, _) = refused(
        client(&r, "lockdown")
            .update(&UpdateRequest {
                tag: None,
                expect_digest: Some("sha256:short".into()),
                ..Default::default()
            })
            .unwrap_err(),
    );
    assert_eq!((status, code.as_str()), (400, "bad_digest"));
    assert!(r.docker.calls().is_empty());
}

#[test]
fn restart_restarts_the_allowed_services() {
    let r = start();
    let done = client(&r, "vale").restart().unwrap();
    assert_eq!(done.services, ["gateway"]);
    assert_eq!(r.docker.calls(), ["restart vale gateway"]);
}

#[test]
fn a_wrong_key_or_another_stacks_name_does_nothing() {
    let r = start();
    let wrong = Client::new(&r.url, "lockdown", Key::generate().unwrap()).unwrap();
    let (status, code, _) = refused(wrong.restart().unwrap_err());
    assert_eq!((status, code.as_str()), (401, "bad_signature"));
    // Vale's key, claiming to be Lockdown.
    let borrowed = Client::new(&r.url, "lockdown", key_of("vale")).unwrap();
    let (status, _, _) = refused(borrowed.restart().unwrap_err());
    assert_eq!(status, 401);
    let unknown = Client::new(&r.url, "media", key_of("vale")).unwrap();
    let (_, code, _) = refused(unknown.status().unwrap_err());
    assert_eq!(code, "unknown_stack");
    assert!(r.docker.calls().is_empty());
}

#[test]
fn a_replayed_or_stale_request_is_refused() {
    let r = start();
    let key = key_of("lockdown");
    let nonce = protocol::new_nonce().unwrap();
    let head = signed_head("lockdown", &key, "/v1/restart", now(), &nonce, "{}");
    assert_eq!(raw(&r.url, &head, "{}").0, 200);
    let (status, body) = raw(&r.url, &head, "{}");
    assert_eq!(status, 401);
    assert!(body.contains("replayed"), "{body}");

    let old = signed_head(
        "lockdown",
        &key,
        "/v1/restart",
        now() - 600,
        &protocol::new_nonce().unwrap(),
        "{}",
    );
    let (status, body) = raw(&r.url, &old, "{}");
    assert_eq!(status, 401);
    assert!(body.contains("clocks agree"), "{body}");

    // The signature covers the body: the same headers with another body fail.
    let n = protocol::new_nonce().unwrap();
    let head = signed_head("lockdown", &key, "/v1/update", now(), &n, "{}");
    let (status, body) = raw(&r.url, &head, r#"{"tag":"0.0.1-main.20200101000000"}"#);
    assert_eq!(status, 401);
    assert!(body.contains("bad_signature"));
    assert_eq!(r.docker.calls(), ["restart lockdown lockdown"]);
}

#[test]
fn only_three_operations_and_only_post() {
    let r = start();
    let key = key_of("lockdown");
    let head = signed_head(
        "lockdown",
        &key,
        "/v1/exec",
        now(),
        &protocol::new_nonce().unwrap(),
        "{}",
    );
    assert_eq!(raw(&r.url, &head, "{}").0, 404);
    let addr = r.url.strip_prefix("http://").unwrap();
    let mut s = TcpStream::connect(addr).unwrap();
    write!(s, "GET /v1/status HTTP/1.1\r\n\r\n").unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 405"), "{out}");
    assert_eq!(raw(&r.url, "/v1/status HTTP/1.1\r\n", "{}").0, 401);
    assert!(r.docker.calls().is_empty());
}

#[test]
fn one_change_per_stack_at_a_time() {
    let r = start();
    let (tx, rx) = mpsc::channel();
    *r.docker.0.hold.lock().unwrap() = Some(rx);
    let c = client(&r, "lockdown");
    let first = std::thread::spawn(move || c.update(&UpdateRequest::default()));
    // Wait until the update is inside compose up.
    for _ in 0..100 {
        if r.docker.calls().iter().any(|c| c.starts_with("up")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (status, code, message) = refused(client(&r, "lockdown").restart().unwrap_err());
    assert_eq!((status, code.as_str()), (409, "busy"));
    assert!(message.contains("already running"));
    // Another stack isn't held up, and status still answers.
    client(&r, "vale").restart().unwrap();
    client(&r, "lockdown").status().unwrap();
    tx.send(()).unwrap();
    first.join().unwrap().unwrap();
}

#[test]
fn each_stack_is_rate_limited() {
    let r = start();
    let c = client(&r, "vale");
    for _ in 0..30 {
        c.status().unwrap();
    }
    let (status, code, _) = refused(c.status().unwrap_err());
    assert_eq!((status, code.as_str()), (429, "rate_limited"));
    client(&r, "lockdown").status().unwrap();
}

#[test]
fn many_failures_shut_the_door_for_a_minute() {
    let r = start();
    let wrong = Client::new(&r.url, "lockdown", Key::generate().unwrap()).unwrap();
    for _ in 0..20 {
        let _ = wrong.status();
    }
    let (status, code, _) = refused(client(&r, "lockdown").status().unwrap_err());
    assert_eq!((status, code.as_str()), (429, "too_many_failures"));
}

#[test]
fn the_log_names_who_and_what_but_never_a_key_or_signature() {
    let r = start();
    client(&r, "lockdown").status().unwrap();
    let _ = Client::new(&r.url, "vale", key_of("lockdown"))
        .unwrap()
        .restart();
    let log = r.log.lock().unwrap().join("\n");
    assert!(
        log.contains("peer=127.0.0.1 stack=lockdown op=status result=ok"),
        "{log}"
    );
    assert!(
        log.contains("stack=vale op=restart result=refused 401 bad_signature"),
        "{log}"
    );
    for stack in ["lockdown", "vale"] {
        assert!(!log.contains(&key_of(stack).to_hex()));
    }
    assert!(!log.to_lowercase().contains("signature="));
}

#[test]
fn the_client_ignores_an_unsigned_success() {
    // A stand-in that answers 200 without the helper's signature.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        if let Ok((mut s, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"stack":"lockdown","services":["lockdown"]}"#;
            let _ = write!(
                s,
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    let c = Client::new(&url, "lockdown", key_of("lockdown")).unwrap();
    match c.restart().unwrap_err() {
        ClientError::BadReply(m) => assert!(m.contains("wasn't signed")),
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_unreachable_helper_says_so() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    let e = Client::new(&url, "lockdown", key_of("lockdown"))
        .unwrap()
        .status()
        .unwrap_err();
    assert!(matches!(e, ClientError::Unreachable(_)));
    assert!(e.to_string().contains("can't be reached"));
}

const WC_TAG: &str = "0.4.0-main.20261109010000";

fn warpcore_update() -> UpdateRequest {
    UpdateRequest {
        tag: Some(WC_TAG.into()),
        expect_digests: Some(warpcore_digests()),
        ..Default::default()
    }
}

#[test]
fn several_images_are_pulled_and_checked_then_started_together() {
    let r = start();
    let done = client(&r, "warpcore").update(&warpcore_update()).unwrap();
    assert_eq!(done.services, ["web", "api", "sandbox"]);
    assert_eq!(done.images.len(), 3);
    assert_eq!(
        done.images[1].pulled,
        format!("ghcr.io/snds/warpcore-api:{WC_TAG}")
    );
    assert_eq!(done.images[1].digest, digest('d'));
    assert_eq!(done.before.as_ref().unwrap().exit_code, Some(0));
    let after = done.after.unwrap();
    assert!(after.ok);
    assert_eq!(after.service, "sandbox");
    assert_eq!(after.output, "sandbox said 0");
    let mut want: Vec<String> = WARPCORE
        .iter()
        .map(|r| format!("pull {r}:{WC_TAG}"))
        .collect();
    want.extend(WARPCORE.iter().map(|r| format!("image {r}:{WC_TAG}")));
    want.push("run warpcore backup sh /backup/backup.sh once".into());
    for (repo, c) in WARPCORE.iter().zip(['c', 'd', 'e']) {
        want.push(format!("tag {repo}@{} {repo}:main", digest(c)));
    }
    want.push("up warpcore web,api,sandbox".into());
    want.push("run warpcore sandbox no-deps node dist/selftest.js".into());
    assert_eq!(r.docker.calls(), want);

    let status = client(&r, "warpcore").status().unwrap();
    assert_eq!(status.repositories, WARPCORE);
    assert!(status.progress.running.is_none());
    let last = status.progress.last.unwrap();
    assert!(last.ok && last.failed_step.is_none());
    assert_eq!(last.operation, "update");
    assert!(last.after.unwrap().ok);
}

#[test]
fn one_wrong_digest_among_several_starts_nothing() {
    let r = start();
    r.docker
        .0
        .digests
        .lock()
        .unwrap()
        .insert(WARPCORE[2].into(), digest('f'));
    let (status, code, message) = refused(
        client(&r, "warpcore")
            .update(&warpcore_update())
            .unwrap_err(),
    );
    assert_eq!((status, code.as_str()), (409, "digest_mismatch"));
    assert!(message.contains("warpcore-sandbox") && message.contains("Nothing was restarted"));
    assert!(
        !r.docker
            .calls()
            .iter()
            .any(|c| c.starts_with("run") || c.starts_with("tag") || c.starts_with("up"))
    );
    let last = client(&r, "warpcore")
        .status()
        .unwrap()
        .progress
        .last
        .unwrap();
    assert!(!last.ok);
    assert_eq!(last.failed_step.as_deref(), Some("check"));
    assert_eq!(last.error.unwrap().error, "digest_mismatch");
}

#[test]
fn a_failed_before_step_starts_nothing() {
    let r = start();
    r.docker.0.exits.lock().unwrap().insert("backup".into(), 2);
    let (status, code, message) = refused(
        client(&r, "warpcore")
            .update(&warpcore_update())
            .unwrap_err(),
    );
    assert_eq!((status, code.as_str()), (502, "before_failed"));
    assert!(
        message.contains("exited with 2") && message.contains("backup said 2"),
        "{message}"
    );
    let calls = r.docker.calls();
    assert!(calls.last().unwrap().starts_with("run warpcore backup"));
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("tag") || c.starts_with("up"))
    );
    let last = client(&r, "warpcore")
        .status()
        .unwrap()
        .progress
        .last
        .unwrap();
    assert_eq!(last.failed_step.as_deref(), Some("before"));
}

#[test]
fn a_failed_after_step_is_reported_not_undone() {
    let r = start();
    r.docker.0.exits.lock().unwrap().insert("sandbox".into(), 3);
    let done = client(&r, "warpcore").update(&warpcore_update()).unwrap();
    let after = done.after.unwrap();
    assert!(!after.ok);
    assert_eq!(after.exit_code, Some(3));
    assert_eq!(after.output, "sandbox said 3");
    // The services were started, and nothing ran after the self-test.
    let calls = r.docker.calls();
    assert!(calls.contains(&"up warpcore web,api,sandbox".to_owned()));
    assert!(calls.last().unwrap().starts_with("run warpcore sandbox"));
    let last = client(&r, "warpcore")
        .status()
        .unwrap()
        .progress
        .last
        .unwrap();
    assert!(last.ok);
    assert_eq!(last.after.unwrap().exit_code, Some(3));
}

#[test]
fn several_images_need_a_version_and_every_digest() {
    let r = start();
    let wc = client(&r, "warpcore");
    let main = UpdateRequest {
        tag: None,
        ..warpcore_update()
    };
    let (status, code, message) = refused(wc.update(&main).unwrap_err());
    assert_eq!((status, code.as_str()), (403, "tag_not_allowed"));
    assert!(message.contains("version tag"), "{message}");

    let none = UpdateRequest {
        expect_digests: None,
        ..warpcore_update()
    };
    assert_eq!(refused(wc.update(&none).unwrap_err()).1, "digests_required");

    let mut short = warpcore_digests();
    short.remove(WARPCORE[0]);
    let missing = UpdateRequest {
        expect_digests: Some(short),
        ..warpcore_update()
    };
    assert_eq!(refused(wc.update(&missing).unwrap_err()).1, "bad_digests");

    let mut extra = warpcore_digests();
    extra.insert("ghcr.io/snds/lockdown-server".into(), digest('a'));
    let extra = UpdateRequest {
        expect_digests: Some(extra),
        ..warpcore_update()
    };
    assert_eq!(refused(wc.update(&extra).unwrap_err()).1, "bad_digests");

    let one = UpdateRequest {
        expect_digest: Some(digest('c')),
        ..warpcore_update()
    };
    assert_eq!(refused(wc.update(&one).unwrap_err()).1, "bad_request");

    let mut bad = warpcore_digests();
    bad.insert(WARPCORE[1].into(), "sha256:short".into());
    let bad = UpdateRequest {
        expect_digests: Some(bad),
        ..warpcore_update()
    };
    assert_eq!(refused(wc.update(&bad).unwrap_err()).1, "bad_digest");

    assert!(r.docker.calls().is_empty());
}

#[test]
fn a_one_image_stack_also_takes_its_digest_by_repository() {
    let r = start();
    let ld = client(&r, "lockdown");
    let by_repo = UpdateRequest {
        expect_digests: Some(BTreeMap::from([(
            "ghcr.io/snds/lockdown-server".to_owned(),
            digest('b'),
        )])),
        ..Default::default()
    };
    assert_eq!(
        refused(ld.update(&by_repo).unwrap_err()).1,
        "digest_mismatch"
    );
    let other = UpdateRequest {
        expect_digests: Some(BTreeMap::from([(WARPCORE[0].to_owned(), digest('a'))])),
        ..Default::default()
    };
    assert_eq!(refused(ld.update(&other).unwrap_err()).1, "bad_digests");
}

#[test]
fn status_shows_the_step_an_update_is_on() {
    let r = start();
    let (tx, rx) = mpsc::channel();
    *r.docker.0.hold.lock().unwrap() = Some(rx);
    let url = r.url.clone();
    let worker = std::thread::spawn(move || {
        Client::new(&url, "warpcore", key_of("warpcore"))
            .unwrap()
            .update(&warpcore_update())
    });
    let wc = client(&r, "warpcore");
    let mut seen = None;
    for _ in 0..200 {
        let p = wc.status().unwrap().progress;
        if let Some(running) = p.running
            && running.step == "recreate"
        {
            seen = Some(running);
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let running = seen.expect("status never showed the recreate step");
    assert_eq!(running.operation, "update");
    tx.send(()).unwrap();
    assert!(worker.join().unwrap().is_ok());
    let p = wc.status().unwrap().progress;
    assert!(p.running.is_none() && p.last.unwrap().ok);
}
