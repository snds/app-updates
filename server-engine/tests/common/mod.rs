//! Test parts for the engine: a fake publish channel, a fake Docker Engine API and a minisign
//! signer, ported from Synapse's relay tests. Everything runs offline on 127.0.0.1; no real
//! registry or product is used.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine;
use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use server_engine::engine::{Feed, MemoryStore, Settings};
use server_engine::release;
use server_engine::{Hooks, Updater};
use tiny_http::{Header, Response, Server};

fn b64(b: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// A digest made of one repeated hex digit.
pub fn digest(c: char) -> String {
    format!("sha256:{}", c.to_string().repeat(64))
}

// ---- A release key, signing as minisign does (its legacy, non-pre-hashed form, which Tauri's
// updater and the helper both accept; the pre-hashed form is checked with a real `tauri signer`
// fixture).

pub struct Key {
    sk: SigningKey,
    id: [u8; 8],
}

impl Key {
    pub fn new(seed: u8) -> Key {
        Key {
            sk: SigningKey::from_bytes(&[seed; 32]),
            id: [seed, 1, 2, 3, 4, 5, 6, 7],
        }
    }

    /// The public key as the updater reads it: base64 of minisign's two-line `.pub` text.
    pub fn public(&self) -> String {
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.id);
        raw.extend_from_slice(self.sk.verifying_key().as_bytes());
        b64(format!("untrusted comment: minisign public key\n{}\n", b64(&raw)).as_bytes())
    }

    /// A signature over `data` as `latest.json` carries it: base64 of minisign's `.sig` text.
    pub fn sign(&self, data: &[u8]) -> String {
        let sig = self.sk.sign(data).to_bytes();
        let mut raw = b"Ed".to_vec();
        raw.extend_from_slice(&self.id);
        raw.extend_from_slice(&sig);
        let trusted = "timestamp:0\tfile:release";
        let mut global = sig.to_vec();
        global.extend_from_slice(trusted.as_bytes());
        let global = self.sk.sign(&global).to_bytes();
        b64(format!(
            "untrusted comment: signature\n{}\ntrusted comment: {trusted}\n{}\n",
            b64(&raw),
            b64(&global)
        )
        .as_bytes())
    }
}

/// A `latest.json` for `app` at `version`, each service's image signed by `key`.
pub fn manifest(key: &Key, app: &str, version: &str, images: &[(&str, &str)]) -> String {
    let platforms: serde_json::Map<String, Value> = images
        .iter()
        .map(|(svc, img)| {
            let sig = key.sign(release::statement(app, version, svc, img).as_bytes());
            (
                format!("server-{svc}"),
                json!({ "url": img, "signature": sig }),
            )
        })
        .collect();
    json!({ "version": version, "notes": "", "pub_date": "2026-11-16T00:00:00Z", "platforms": platforms })
        .to_string()
}

/// [`manifest`] with the release's own signed flags, as CI publishes it: `by_hand` signed by `key`.
pub fn signed_manifest(
    key: &Key,
    app: &str,
    version: &str,
    by_hand: bool,
    images: &[(&str, &str)],
) -> String {
    let mut m: Value = serde_json::from_str(&manifest(key, app, version, images)).unwrap();
    m["byHand"] = json!(by_hand);
    m["byHandAt"] = json!("0.9.0");
    m["signature"] =
        json!(key.sign(release::meta_statement(app, version, by_hand, "0.9.0").as_bytes()));
    m.to_string()
}

// ---- A fake publish channel: serves `/latest.json`, and `/away` redirects off the allowed hosts.

pub struct Channel {
    pub url: String,
    body: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Channel {
    pub fn start() -> Channel {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let body = Arc::new(Mutex::new(String::from("{}")));
        let stop = Arc::new(AtomicBool::new(false));
        let (b, s) = (body.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let Ok(Some(req)) = server.recv_timeout(Duration::from_millis(100)) else {
                    continue;
                };
                let resp = match req.url() {
                    "/latest.json" => Response::from_string(b.lock().unwrap().clone()),
                    "/away" => Response::from_string("").with_status_code(302).with_header(
                        Header::from_bytes("Location", "https://evil.example/latest.json").unwrap(),
                    ),
                    _ => Response::from_string("").with_status_code(404),
                };
                let _ = req.respond(resp);
            }
        });
        Channel {
            url: format!("http://127.0.0.1:{port}/latest.json"),
            body,
            stop,
            handle: Some(handle),
        }
    }

    pub fn publish(&self, latest: String) {
        *self.body.lock().unwrap() = latest;
    }
}

impl Drop for Channel {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The engine's settings for one app on `channel`, trusting `key`, with short waits.
pub fn settings(app: &str, endpoint: &str, key: &str) -> Settings {
    let mut s = Settings::new(Feed {
        id: app.into(),
        name: "Warpcore".into(),
        endpoint: endpoint.into(),
        key: key.into(),
        hosts: vec!["github.com".into()],
        optional: vec!["artifacts".into()],
        supervisor: Some("server".into()),
    });
    s.health_wait = Duration::from_secs(3);
    s.settle = Duration::from_millis(150);
    s.wait_poll = Duration::from_millis(50);
    s.wait_max = Duration::from_secs(5);
    s.after_wait = Duration::from_secs(5);
    s.loopback_feeds = true;
    s
}

/// An engine on `docker` with `hooks`, keeping its settings in memory.
pub fn updater(settings: Settings, docker: &FakeDocker, hooks: impl Hooks + 'static) -> Updater {
    Updater::new(
        settings,
        docker.engine(),
        Box::new(hooks),
        Box::new(MemoryStore::default()),
    )
}

// ---- A fake Docker Engine API (the calls the helper makes, over plain HTTP on loopback).

/// How a container made from an image behaves once started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaves {
    /// Its health check passes.
    Healthy,
    /// Its health check fails.
    Unhealthy,
    /// It has no health check and keeps running.
    Runs,
    /// It has no health check and exits at once.
    Exits,
}

#[derive(Debug, Clone)]
pub struct Image {
    pub id: String,
    pub reference: String,
    pub config: Value,
    pub behaves: Behaves,
    /// What a one-off run of `selftest` in a container from this image exits with.
    pub selftest: i64,
}

#[derive(Debug, Clone)]
pub struct Ctr {
    pub id: String,
    pub name: String,
    /// The image reference it was made from.
    pub image: String,
    pub image_id: String,
    pub config: Value,
    pub host: Value,
    pub networks: BTreeMap<String, Value>,
    pub running: bool,
    pub health_checks: u32,
    /// Set once it has ended.
    pub exit_code: Option<i64>,
}

impl Ctr {
    fn summary(&self) -> Value {
        json!({ "name": self.name, "image": self.image, "config": self.config, "host": self.host,
                "networks": self.networks })
    }
}

#[derive(Default)]
pub struct World {
    /// What a registry would serve, by reference.
    pub registry: HashMap<String, Image>,
    /// Images on this server, by reference and by id.
    pub images: HashMap<String, Image>,
    pub ctrs: Vec<Ctr>,
    /// Every pull asked for, as given.
    pub pulls: Vec<String>,
    /// Every create body, by name.
    pub created: Vec<(String, Value)>,
    /// Every call, `METHOD path`.
    pub calls: Vec<String>,
    /// Every one-off self-test run: its container's settings.
    pub selftests: Vec<Value>,
}

impl World {
    fn find(&self, key: &str) -> Option<usize> {
        self.ctrs.iter().position(|c| {
            c.name == key || c.id == key || (key.len() >= 12 && c.id.starts_with(key))
        })
    }

    fn image(&self, key: &str) -> Option<&Image> {
        self.images
            .get(key)
            .or_else(|| self.images.values().find(|i| i.id == key))
    }

    pub fn ctr(&self, name: &str) -> Option<&Ctr> {
        self.find(name).map(|i| &self.ctrs[i])
    }

    /// Labels as Docker shows them: the image's, then the container's own on top.
    fn labels(&self, c: &Ctr) -> Value {
        let mut l = self
            .image(&c.image_id)
            .and_then(|i| i.config["Labels"].as_object().cloned())
            .unwrap_or_default();
        if let Some(own) = c.config["Labels"].as_object() {
            for (k, v) in own {
                l.insert(k.clone(), v.clone());
            }
        }
        Value::Object(l)
    }

    fn inspect(&self, c: &Ctr) -> Value {
        let img = self.image(&c.image_id).cloned();
        let behaves = img.as_ref().map_or(Behaves::Runs, |i| i.behaves);
        let mut cfg = c.config.clone();
        cfg["Labels"] = self.labels(c);
        cfg["Image"] = json!(c.image);
        let mut state = json!({ "Running": c.running, "ExitCode": c.exit_code.unwrap_or(0) });
        if c.running {
            match behaves {
                Behaves::Healthy => {
                    state["Health"] = json!({ "Status": if c.health_checks == 0 { "starting" } else { "healthy" } })
                }
                Behaves::Unhealthy => {
                    state["Health"] = json!({ "Status": if c.health_checks == 0 { "starting" } else { "unhealthy" } })
                }
                Behaves::Runs | Behaves::Exits => {}
            }
        }
        json!({
            "Id": c.id,
            "Name": format!("/{}", c.name),
            "Image": c.image_id,
            "Config": cfg,
            "HostConfig": c.host,
            "NetworkSettings": { "Networks": c.networks },
            "State": state,
            "RestartCount": 0,
        })
    }
}

fn dec(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 3 <= b.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(if b[i] == b'+' { b' ' } else { b[i] });
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

fn query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| kv.split_once('='))
        .map(|(k, v)| (dec(k), dec(v)))
        .collect()
}

static NEXT: AtomicU64 = AtomicU64::new(1);

fn new_id() -> String {
    format!(
        "{:064x}",
        NEXT.fetch_add(1, Ordering::Relaxed) * 0x1_0000_0001
    )
}

pub struct FakeDocker {
    pub addr: String,
    pub world: Arc<Mutex<World>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

fn reply(code: u16, v: Value) -> Response<std::io::Cursor<Vec<u8>>> {
    Response::from_data(v.to_string().into_bytes()).with_status_code(code)
}

fn handle(
    w: &mut World,
    method: &str,
    url: &str,
    body: &[u8],
) -> Response<std::io::Cursor<Vec<u8>>> {
    let url = url.strip_prefix("/v1.41").unwrap_or(url);
    let (path, q) = url.split_once('?').unwrap_or((url, ""));
    let q = query(q);
    let parts: Vec<String> = path.trim_start_matches('/').split('/').map(dec).collect();
    let parts: Vec<&str> = parts.iter().map(String::as_str).collect();
    w.calls.push(format!("{method} {path}"));
    let missing = || reply(404, json!({ "message": "No such container" }));
    match (method, parts.as_slice()) {
        ("GET", ["containers", "json"]) => {
            let want: Vec<String> = q
                .get("filters")
                .and_then(|f| serde_json::from_str::<Value>(f).ok())
                .and_then(|f| f["label"].as_array().cloned())
                .unwrap_or_default()
                .iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect();
            let list: Vec<Value> = w
                .ctrs
                .iter()
                .filter(|c| {
                    let l = w.labels(c);
                    want.iter().all(|kv| {
                        let (k, v) = kv.split_once('=').unwrap();
                        l[k].as_str() == Some(v)
                    })
                })
                .map(|c| {
                    json!({ "Id": c.id, "Names": [format!("/{}", c.name)], "Image": c.image,
                            "Labels": w.labels(c), "State": if c.running { "running" } else { "exited" } })
                })
                .collect();
            reply(200, json!(list))
        }
        ("GET", ["containers", name, "json"]) => match w.find(name) {
            Some(i) => {
                let v = w.inspect(&w.ctrs[i]);
                if w.ctrs[i].running {
                    w.ctrs[i].health_checks += 1;
                }
                if w.ctrs[i].running
                    && w.image(&w.ctrs[i].image_id).map(|i| i.behaves) == Some(Behaves::Exits)
                {
                    w.ctrs[i].running = false;
                }
                reply(200, v)
            }
            None => missing(),
        },
        ("GET", ["images", name, "json"]) => match w.image(name) {
            Some(i) => reply(
                200,
                json!({ "Id": i.id, "RepoDigests": [i.reference], "Config": i.config }),
            ),
            None => reply(404, json!({ "message": "No such image" })),
        },
        ("POST", ["images", "create"]) => {
            let r = q.get("fromImage").cloned().unwrap_or_default();
            w.pulls.push(r.clone());
            match w.registry.get(&r).cloned() {
                Some(img) => {
                    w.images.insert(r.clone(), img);
                    Response::from_data(
                        format!(
                            "{{\"status\":\"Pulling from {r}\"}}\n{{\"status\":\"Downloaded\"}}\n"
                        )
                        .into_bytes(),
                    )
                }
                None => reply(
                    404,
                    json!({ "message": format!("manifest for {r} not found") }),
                ),
            }
        }
        ("POST", ["containers", "create"]) => {
            let name = q.get("name").cloned().unwrap_or_default();
            if w.find(&name).is_some() {
                return reply(409, json!({ "message": "name in use" }));
            }
            let b: Value = serde_json::from_slice(body).unwrap();
            w.created.push((name.clone(), b.clone()));
            let image = b["Image"].as_str().unwrap_or_default().to_owned();
            let Some(img) = w.image(&image).cloned() else {
                return reply(404, json!({ "message": "No such image" }));
            };
            let mut config = b.clone();
            if let Some(o) = config.as_object_mut() {
                o.remove("HostConfig");
                o.remove("NetworkingConfig");
            }
            let networks = b["NetworkingConfig"]["EndpointsConfig"]
                .as_object()
                .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                .unwrap_or_default();
            let id = new_id();
            w.ctrs.push(Ctr {
                id: id.clone(),
                name,
                image,
                image_id: img.id,
                config,
                host: b["HostConfig"].clone(),
                networks,
                running: false,
                health_checks: 0,
                exit_code: None,
            });
            reply(201, json!({ "Id": id, "Warnings": [] }))
        }
        ("POST", ["containers", name, "start"]) => match w.find(name) {
            Some(i) if w.ctrs[i].running => reply(304, json!({})),
            Some(i) => {
                w.ctrs[i].running = true;
                w.ctrs[i].health_checks = 0;
                // A one-off self-test ends at once, with the exit code its image gives it.
                if w.ctrs[i].config["Cmd"] == json!(["selftest"]) {
                    let code = w.image(&w.ctrs[i].image_id).map_or(0, |i| i.selftest);
                    w.ctrs[i].running = false;
                    w.ctrs[i].exit_code = Some(code);
                    let summary = w.ctrs[i].summary();
                    w.selftests.push(summary);
                }
                reply(204, json!({}))
            }
            None => missing(),
        },
        ("POST", ["containers", name, "stop"]) => match w.find(name) {
            Some(i) if !w.ctrs[i].running => reply(304, json!({})),
            Some(i) => {
                w.ctrs[i].running = false;
                reply(204, json!({}))
            }
            None => missing(),
        },
        ("POST", ["containers", name, "rename"]) => {
            let to = q.get("name").cloned().unwrap_or_default();
            if w.find(&to).is_some() {
                return reply(409, json!({ "message": "name in use" }));
            }
            match w.find(name) {
                Some(i) => {
                    w.ctrs[i].name = to;
                    reply(204, json!({}))
                }
                None => missing(),
            }
        }
        ("GET", ["containers", name, "logs"]) => match w.find(name) {
            Some(i) => {
                let text = if w.ctrs[i].exit_code == Some(0) {
                    "self-test passed\n"
                } else {
                    "boot failed\nself-test failed\n"
                };
                Response::from_data(text.as_bytes().to_vec())
            }
            None => missing(),
        },
        ("DELETE", ["containers", name]) => match w.find(name) {
            Some(i) => {
                w.ctrs.remove(i);
                reply(204, json!({}))
            }
            None => missing(),
        },
        ("POST", ["networks", net, "connect"]) => {
            let b: Value = serde_json::from_slice(body).unwrap();
            match w.find(b["Container"].as_str().unwrap_or_default()) {
                Some(i) => {
                    w.ctrs[i]
                        .networks
                        .insert((*net).to_owned(), b["EndpointConfig"].clone());
                    reply(200, json!({}))
                }
                None => missing(),
            }
        }
        _ => reply(
            404,
            json!({ "message": format!("the fake has no {method} {path}") }),
        ),
    }
}

impl FakeDocker {
    pub fn start() -> FakeDocker {
        let server = Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        let world = Arc::new(Mutex::new(World::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let (w, s) = (world.clone(), stop.clone());
        let handle = std::thread::spawn(move || {
            while !s.load(Ordering::Relaxed) {
                let Ok(Some(mut req)) = server.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                let mut body = Vec::new();
                let _ = std::io::Read::read_to_end(req.as_reader(), &mut body);
                let resp = {
                    let mut w = w.lock().unwrap();
                    handle(&mut w, req.method().as_str(), req.url(), &body)
                };
                let _ = req.respond(resp);
            }
        });
        FakeDocker {
            addr: format!("tcp://127.0.0.1:{port}"),
            world,
            stop,
            handle: Some(handle),
        }
    }

    pub fn engine(&self) -> Box<server_engine::docker::Docker> {
        Box::new(server_engine::docker::Docker::new(
            server_engine::docker::Address::parse(&self.addr).unwrap(),
        ))
    }

    /// An image a registry serves at `reference`, with its version label.
    pub fn publish_image(&self, reference: &str, version: &str, behaves: Behaves) {
        self.publish_image_with(reference, version, behaves, 0);
    }

    /// [`Self::publish_image`], with what its self-test exits with.
    pub fn publish_image_with(
        &self,
        reference: &str,
        version: &str,
        behaves: Behaves,
        selftest: i64,
    ) {
        let img = Image {
            id: format!("sha256:{}", new_id()),
            reference: reference.into(),
            config: json!({
                "Env": ["PATH=/usr/bin", format!("APP_VERSION={version}")],
                "Labels": { "org.opencontainers.image.version": version },
                "Cmd": ["serve"],
                "Healthcheck": if matches!(behaves, Behaves::Healthy | Behaves::Unhealthy) { json!({ "Test": ["CMD", "check"] }) } else { Value::Null },
            }),
            behaves,
            selftest,
        };
        self.world
            .lock()
            .unwrap()
            .registry
            .insert(reference.into(), img);
    }

    /// A running container `name` of `app`'s `service`, made from `reference` (published first),
    /// with settings of its own: a mount, a port, an environment entry and a network alias.
    pub fn run(&self, name: &str, app: &str, service: &str, reference: &str) {
        let mut w = self.world.lock().unwrap();
        let img = w.registry[reference].clone();
        w.images.insert(reference.into(), img.clone());
        let mut env = img.config["Env"].as_array().cloned().unwrap();
        env.push(json!("WARP_SECRET_REF=sy://warpcore/db"));
        let id = new_id();
        w.ctrs.push(Ctr {
            id: id.clone(),
            name: name.into(),
            image: reference.into(),
            image_id: img.id,
            config: json!({
                "Hostname": id[..12],
                "Env": env,
                "Cmd": ["serve"],
                "Labels": { "org.snds.update.app": app, "org.snds.update.service": service },
            }),
            host: json!({
                "Binds": ["/mnt/user/appdata/warpcore:/data"],
                "PortBindings": { "8080/tcp": [{ "HostPort": "8080" }] },
                "RestartPolicy": { "Name": "unless-stopped" },
                "NetworkMode": "warp",
            }),
            networks: BTreeMap::from([(
                "warp".to_owned(),
                json!({ "Aliases": [service, &id[..12]], "NetworkID": "n1", "IPAddress": "10.1.0.5" }),
            )]),
            running: true,
            health_checks: 5,
            exit_code: None,
        });
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut World) -> T) -> T {
        f(&mut self.world.lock().unwrap())
    }
}

impl Drop for FakeDocker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
