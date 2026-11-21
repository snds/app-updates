//! How the update helper drives an app's containers: the [`Engine`] seam, and [`Docker`], which
//! fills it through the Docker Engine API.
//!
//! The helper touches only containers that opt in with the label [`LABEL_APP`]. It never changes a
//! container's settings: a new container is the old one's own settings with only the image (and
//! the version label) changed, so an update can't add a mount, a device or a privilege. The old
//! container is kept, stopped, as `<name>-previous` until the next update, for rollback.
//!
//! The API is reached at an address the person chooses (the service app's `[docker] address`): best its own socket proxy
//! that allows only the container, image and network calls, or the
//! Docker socket itself. Either is powerful; see docs/self-hosting-service-apps.md.

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

/// The label that opts a container in, with its app's id as the value (`warpcore`).
pub const LABEL_APP: &str = "org.snds.update.app";
/// The label naming the container's service in its app's releases (`web`). Default: its name.
pub const LABEL_SERVICE: &str = "org.snds.update.service";
/// The label the helper sets on a container it makes: the release's version.
pub const LABEL_VERSION: &str = "org.snds.update.version";
/// The image's own version label, read when the helper hasn't set one yet.
pub const LABEL_OCI_VERSION: &str = "org.opencontainers.image.version";
/// The name suffix of the container kept for rollback.
pub const PREVIOUS: &str = "-previous";

/// One running service of an app: a container that carries [`LABEL_APP`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Service {
    pub id: String,
    /// The container's name, without Docker's leading `/`.
    pub name: String,
    /// The service it is in the app's releases.
    pub service: String,
    /// The image it was made from, as given.
    pub image: String,
    /// The version it runs, from its labels.
    pub version: Option<String>,
    /// The version of the container kept for rollback, if one is kept (`None` inside: unknown).
    pub previous: Option<Option<String>>,
}

/// An update's swap of one service failed. `started` says whether the old container was already
/// set aside (so a rollback is needed) or is untouched.
#[derive(Debug)]
pub struct SwapFailed {
    pub started: bool,
    pub why: String,
}

/// What the helper needs from a container runtime. [`Docker`] fills it; tests fill it with a fake.
pub trait Engine: Send + Sync {
    /// The app's services, in name order. Containers kept for rollback aren't listed.
    fn services(&self, app: &str) -> Result<Vec<Service>, String>;
    /// Pulls `image` (pinned by digest).
    fn pull(&self, image: &str) -> Result<(), String>;
    /// Sets the service's container aside as `<name>-previous` (dropping an older one kept there)
    /// and starts a new one from `image`, with the same settings and `version` as its label.
    fn replace(&self, svc: &Service, image: &str, version: &str) -> Result<(), SwapFailed>;
    /// Waits until the container named `name` is healthy: its health check passes, or, with no
    /// health check, it keeps running for `settle`. Fails at `wait`.
    fn healthy(&self, name: &str, wait: Duration, settle: Duration) -> Result<(), String>;
    /// Goes back to the container kept as `<name>-previous`: removes the one named `name` and
    /// starts the kept one under its name again.
    fn restore(&self, name: &str) -> Result<(), String>;
    /// Whether `image` (pinned by digest) is already on this server: downloaded ahead of time.
    fn has_image(&self, image: &str) -> Result<bool, String>;
    /// Runs `command` once in a new container copied from the container named `from` (its image,
    /// settings, mounts and networks), with no published ports, no restart and without the app's
    /// label, waits up to `wait` for it to end, removes it, and returns its exit code and the end
    /// of its output. For a product's self-test after an update.
    fn run_once(
        &self,
        from: &str,
        command: &[String],
        wait: Duration,
    ) -> Result<(i64, String), String>;
}

/// The name suffix of a one-off container made by [`Engine::run_once`].
pub const ONCE: &str = "-once";

/// The Docker Engine API version used: Docker 20.10 and later.
const API: &str = "/v1.41";
/// The largest answer read from Docker.
const READ_MAX: usize = 64 * 1024 * 1024;

/// Where the Docker Engine API is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Address {
    /// `tcp://host:port`: plain HTTP, such as a socket proxy on the container's own network.
    Tcp(String),
    /// `unix:///path`: the Docker socket.
    Unix(std::path::PathBuf),
}

impl Address {
    pub fn parse(s: &str) -> Result<Address, String> {
        if let Some(rest) = s.strip_prefix("tcp://") {
            let host = rest.trim_end_matches('/');
            if host.is_empty() || host.contains('/') {
                return Err(format!("{s}: not tcp://host:port"));
            }
            Ok(Address::Tcp(host.to_owned()))
        } else if let Some(path) = s.strip_prefix("unix://") {
            Ok(Address::Unix(path.into()))
        } else {
            Err(format!("{s}: not tcp://host:port or unix:///path"))
        }
    }
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

/// A registry sign-in for pulls: a user and a read-only token (for ghcr.io, a classic token with
/// `read:packages` only). It is sent only with a pull from that registry, never to another one.
#[derive(Clone)]
pub struct RegistryAuth {
    /// The registry's host, such as `ghcr.io`.
    pub host: String,
    pub user: String,
    pub token: String,
}

impl std::fmt::Debug for RegistryAuth {
    // The token never reaches a log, an error or a status.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RegistryAuth {{ host: {:?}, user: {:?} }}",
            self.host, self.user
        )
    }
}

impl RegistryAuth {
    /// The sign-in for `host`, or why it can't be used.
    pub fn new(host: &str, user: &str, token: &str) -> Result<RegistryAuth, String> {
        let host = host.trim().to_ascii_lowercase();
        if host.is_empty()
            || !host
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b".-:".contains(&b))
        {
            return Err(format!("{host}: not a registry host, such as ghcr.io"));
        }
        if user.trim().is_empty() || token.trim().is_empty() {
            return Err("a registry sign-in needs both a user and a token".into());
        }
        Ok(RegistryAuth {
            host,
            user: user.trim().to_owned(),
            token: token.trim().to_owned(),
        })
    }

    /// The `X-Registry-Auth` value Docker reads: URL-safe base64 of the sign-in as JSON.
    fn header(&self) -> String {
        use base64::Engine as _;
        let json = json!({
            "username": self.user,
            "password": self.token,
            "serveraddress": self.host,
        });
        base64::engine::general_purpose::URL_SAFE.encode(json.to_string())
    }
}

/// The registry an image reference names, as Docker reads it: the first part, when it has a dot
/// or a port or is `localhost`; otherwise Docker Hub.
fn registry_of(image: &str) -> String {
    let name = image.split('@').next().unwrap_or(image);
    match name.split_once('/') {
        Some((first, _)) if first.contains('.') || first.contains(':') || first == "localhost" => {
            first.to_ascii_lowercase()
        }
        _ => "docker.io".into(),
    }
}

/// The Docker Engine API, over plain HTTP/1.1.
pub struct Docker {
    at: Address,
    auth: Option<RegistryAuth>,
}

/// An answer: its status and body.
struct Answer(u16, Vec<u8>);

impl Answer {
    fn json(&self) -> Result<Value, String> {
        serde_json::from_slice(&self.1).map_err(|e| format!("Docker's answer: {e}"))
    }

    fn ok(self, what: &str) -> Result<Answer, String> {
        if (200..300).contains(&self.0) {
            Ok(self)
        } else {
            let why = serde_json::from_slice::<Value>(&self.1)
                .ok()
                .and_then(|v| v.get("message").and_then(Value::as_str).map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&self.1).trim().to_owned());
            Err(format!("{what}: Docker answered {}: {why}", self.0))
        }
    }
}

/// Percent-encodes a query value.
fn enc(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Decodes a chunked HTTP body.
fn unchunk(mut b: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let end = b
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("a chunk without its size")?;
        let size = std::str::from_utf8(&b[..end])
            .ok()
            .and_then(|s| usize::from_str_radix(s.split(';').next()?.trim(), 16).ok())
            .ok_or("a chunk size that isn't a number")?;
        b = &b[end + 2..];
        if size == 0 {
            return Ok(out);
        }
        if b.len() < size + 2 {
            return Err("a chunk cut short".into());
        }
        out.extend_from_slice(&b[..size]);
        b = &b[size + 2..];
    }
}

/// Parses a whole HTTP/1.1 response (the connection was closed after it).
fn parse_response(raw: &[u8]) -> Result<Answer, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("Docker's answer has no head")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse().ok())
        .ok_or("Docker's answer has no status")?;
    let mut chunked = false;
    let mut length = None;
    for l in lines {
        if let Some((k, v)) = l.split_once(':') {
            let (k, v) = (k.trim().to_ascii_lowercase(), v.trim());
            if k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked") {
                chunked = true;
            } else if k == "content-length" {
                length = v.parse::<usize>().ok();
            }
        }
    }
    let body = &raw[split + 4..];
    let body = if chunked {
        unchunk(body)?
    } else {
        body[..length.unwrap_or(body.len()).min(body.len())].to_vec()
    };
    Ok(Answer(status, body))
}

impl Docker {
    pub fn new(at: Address) -> Docker {
        Docker { at, auth: None }
    }

    /// Pulls from `auth.host` sign in with it. Pulls from any other registry don't.
    pub fn with_registry_auth(mut self, auth: RegistryAuth) -> Docker {
        self.auth = Some(auth);
        self
    }

    /// The sign-in for a pull of `image`, if it is from the registry the sign-in is for.
    fn auth_for(&self, image: &str) -> Option<String> {
        self.auth
            .as_ref()
            .filter(|a| registry_of(image) == a.host)
            .map(RegistryAuth::header)
    }

    fn connect(&self, wait: Duration) -> Result<Box<dyn Stream>, String> {
        match &self.at {
            Address::Tcp(host) => {
                use std::net::ToSocketAddrs;
                let addr = host
                    .to_socket_addrs()
                    .map_err(|e| format!("Docker at {host}: {e}"))?
                    .next()
                    .ok_or_else(|| format!("Docker at {host}: no address"))?;
                let s = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(10))
                    .map_err(|e| format!("Docker at {host}: {e}"))?;
                s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
                s.set_write_timeout(Some(wait)).map_err(|e| e.to_string())?;
                Ok(Box::new(s))
            }
            #[cfg(unix)]
            Address::Unix(path) => {
                let s = std::os::unix::net::UnixStream::connect(path)
                    .map_err(|e| format!("Docker at {}: {e}", path.display()))?;
                s.set_read_timeout(Some(wait)).map_err(|e| e.to_string())?;
                s.set_write_timeout(Some(wait)).map_err(|e| e.to_string())?;
                Ok(Box::new(s))
            }
            #[cfg(not(unix))]
            Address::Unix(path) => Err(format!(
                "Docker at {}: a unix socket isn't available here",
                path.display()
            )),
        }
    }

    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        wait: Duration,
    ) -> Result<Answer, String> {
        self.call_with(method, path, body, wait, None)
    }

    /// [`Self::call`], with the registry sign-in header when `auth` is given (pulls only).
    fn call_with(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        wait: Duration,
        auth: Option<&str>,
    ) -> Result<Answer, String> {
        let mut s = self.connect(wait)?;
        let body = body.map(Value::to_string).unwrap_or_default();
        let auth = auth
            .map(|a| format!("X-Registry-Auth: {a}\r\n"))
            .unwrap_or_default();
        let head = format!(
            "{method} {API}{path} HTTP/1.1\r\nHost: docker\r\nConnection: close\r\n\
             Content-Type: application/json\r\n{auth}Content-Length: {}\r\n\r\n",
            body.len()
        );
        s.write_all(head.as_bytes())
            .and_then(|()| s.write_all(body.as_bytes()))
            .and_then(|()| s.flush())
            .map_err(|e| format!("Docker: {e}"))?;
        let mut raw = Vec::new();
        s.take(READ_MAX as u64 + 1)
            .read_to_end(&mut raw)
            .map_err(|e| format!("Docker: {e}"))?;
        if raw.len() > READ_MAX {
            return Err("Docker's answer is too large".into());
        }
        parse_response(&raw)
    }

    fn get(&self, path: &str) -> Result<Answer, String> {
        self.call("GET", path, None, Duration::from_secs(30))
    }

    fn post(&self, path: &str, body: Option<&Value>) -> Result<Answer, String> {
        self.call("POST", path, body, Duration::from_secs(120))
    }

    fn inspect(&self, name: &str) -> Result<Option<Value>, String> {
        let a = self.get(&format!("/containers/{}/json", enc(name)))?;
        if a.0 == 404 {
            return Ok(None);
        }
        a.ok("looking at a container")?.json().map(Some)
    }

    /// Stops and removes a container; one that isn't there is fine.
    fn remove(&self, name: &str) -> Result<(), String> {
        let a = self.call(
            "DELETE",
            &format!("/containers/{}?force=1", enc(name)),
            None,
            Duration::from_secs(120),
        )?;
        if a.0 == 404 {
            return Ok(());
        }
        a.ok("removing a container").map(drop)
    }

    fn stop(&self, name: &str) -> Result<(), String> {
        let a = self.post(&format!("/containers/{}/stop?t=30", enc(name)), None)?;
        if a.0 == 304 {
            return Ok(());
        }
        a.ok("stopping a container").map(drop)
    }

    fn start(&self, name: &str) -> Result<(), String> {
        let a = self.post(&format!("/containers/{}/start", enc(name)), None)?;
        if a.0 == 304 {
            return Ok(());
        }
        a.ok("starting a container").map(drop)
    }

    fn rename(&self, from: &str, to: &str) -> Result<(), String> {
        self.post(
            &format!("/containers/{}/rename?name={}", enc(from), enc(to)),
            None,
        )?
        .ok("renaming a container")
        .map(drop)
    }

    /// The new container's settings: the old one's, with what came from its old image left out
    /// (so the new image's own defaults apply), and only the image and version label changed.
    fn new_settings(&self, old: &Value, image: &str, version: &str) -> Result<Value, String> {
        let id = old.get("Id").and_then(Value::as_str).unwrap_or_default();
        let mut cfg = old
            .get("Config")
            .and_then(Value::as_object)
            .cloned()
            .ok_or("a container without settings")?;
        let image_cfg = match old.get("Image").and_then(Value::as_str) {
            Some(img) => {
                let a = self.get(&format!("/images/{}/json", enc(img)))?;
                if a.0 == 404 {
                    Value::Null
                } else {
                    a.ok("looking at an image")?.json()?["Config"].take()
                }
            }
            None => Value::Null,
        };
        strip_image_defaults(&mut cfg, &image_cfg, id);
        cfg.insert("Image".into(), json!(image));
        let labels = cfg
            .entry("Labels")
            .or_insert_with(|| json!({}))
            .as_object_mut()
            .ok_or("a container's labels aren't a map")?;
        labels.insert(LABEL_VERSION.into(), json!(version));
        let host = old.get("HostConfig").cloned().unwrap_or_else(|| json!({}));
        let mut body = Value::Object(cfg);
        body["HostConfig"] = host;
        Ok(body)
    }
}

/// Leaves out of a container's settings what its image set, as Watchtower does, so the new image's
/// defaults apply: environment entries and labels the image has, and command, entry point, working
/// folder, user, ports, volumes, health check and stop signal equal to the image's. A host name
/// that is the container's own short id goes too.
fn strip_image_defaults(cfg: &mut Map<String, Value>, image: &Value, id: &str) {
    if let Some(Value::String(h)) = cfg.get("Hostname")
        && id.len() >= 12
        && h == &id[..12]
    {
        cfg.remove("Hostname");
    }
    let Some(image) = image.as_object() else {
        return;
    };
    if let (Some(Value::Array(env)), Some(Value::Array(img))) =
        (cfg.get_mut("Env"), image.get("Env"))
    {
        env.retain(|e| !img.contains(e));
    }
    if let (Some(Value::Object(labels)), Some(Value::Object(img))) =
        (cfg.get_mut("Labels"), image.get("Labels"))
    {
        labels.retain(|k, v| img.get(k) != Some(v));
    }
    for k in [
        "Cmd",
        "Entrypoint",
        "WorkingDir",
        "User",
        "ExposedPorts",
        "Volumes",
        "Healthcheck",
        "StopSignal",
    ] {
        if cfg.get(k).is_some() && cfg.get(k) == image.get(k) {
            cfg.remove(k);
        }
    }
}

/// The networks a container is on, as `create` and `connect` take them: aliases (without its own
/// short id), fixed addresses and links. Runtime fields are left out.
fn endpoints(old: &Value) -> Vec<(String, Value)> {
    let id = old.get("Id").and_then(Value::as_str).unwrap_or_default();
    let short = id.get(..12).unwrap_or(id);
    let mode = old["HostConfig"]["NetworkMode"]
        .as_str()
        .unwrap_or_default();
    if mode == "host" || mode == "none" || mode.starts_with("container:") {
        return Vec::new();
    }
    let Some(nets) = old["NetworkSettings"]["Networks"].as_object() else {
        return Vec::new();
    };
    let mut out: Vec<(String, Value)> = nets
        .iter()
        .map(|(name, n)| {
            let mut e = Map::new();
            if let Some(a) = n.get("Aliases").and_then(Value::as_array) {
                let a: Vec<&Value> = a.iter().filter(|x| x.as_str() != Some(short)).collect();
                e.insert("Aliases".into(), json!(a));
            }
            for k in ["IPAMConfig", "Links"] {
                if let Some(v) = n.get(k).filter(|v| !v.is_null()) {
                    e.insert(k.into(), v.clone());
                }
            }
            (name.clone(), Value::Object(e))
        })
        .collect();
    // The network named in its mode goes first: `create` takes one network, `connect` the rest.
    out.sort_by_key(|(n, _)| (n != mode, n.clone()));
    out
}

fn labels(v: &Value) -> &Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    v.get("Labels")
        .and_then(Value::as_object)
        .unwrap_or_else(|| EMPTY.get_or_init(Map::new))
}

fn version_of(labels: &Map<String, Value>) -> Option<String> {
    labels
        .get(LABEL_VERSION)
        .or_else(|| labels.get(LABEL_OCI_VERSION))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

impl Engine for Docker {
    fn services(&self, app: &str) -> Result<Vec<Service>, String> {
        let filter = json!({ "label": [format!("{LABEL_APP}={app}")] }).to_string();
        let list = self
            .get(&format!("/containers/json?all=1&filters={}", enc(&filter)))?
            .ok("listing containers")?
            .json()?;
        let list = list.as_array().ok_or("Docker's list isn't a list")?;
        let name_of = |c: &Value| {
            c["Names"][0]
                .as_str()
                .map(|n| n.trim_start_matches('/').to_owned())
        };
        let mut kept = std::collections::HashMap::new();
        for c in list {
            if let Some(n) = name_of(c)
                && let Some(base) = n.strip_suffix(PREVIOUS)
            {
                kept.insert(base.to_owned(), version_of(labels(c)));
            }
        }
        let mut out: Vec<Service> = list
            .iter()
            .filter_map(|c| {
                let name = name_of(c)?;
                if name.ends_with(PREVIOUS) {
                    return None;
                }
                let l = labels(c);
                Some(Service {
                    id: c["Id"].as_str()?.to_owned(),
                    service: l
                        .get(LABEL_SERVICE)
                        .and_then(Value::as_str)
                        .map_or_else(|| name.clone(), str::to_owned),
                    image: c["Image"].as_str().unwrap_or_default().to_owned(),
                    version: version_of(l),
                    previous: kept.get(&name).cloned(),
                    name,
                })
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn pull(&self, image: &str) -> Result<(), String> {
        crate::release::image_ok(image)?;
        let auth = self.auth_for(image);
        let a = self
            .call_with(
                "POST",
                &format!("/images/create?fromImage={}", enc(image)),
                None,
                Duration::from_secs(30 * 60),
                auth.as_deref(),
            )?
            .ok("pulling an image")?;
        // A pull answers 200 and streams its progress; a failure arrives as an `error` line.
        for line in a.1.split(|b| *b == b'\n') {
            if let Ok(v) = serde_json::from_slice::<Value>(line)
                && let Some(e) = v.get("error").and_then(Value::as_str)
            {
                return Err(format!("pulling {image}: {e}"));
            }
        }
        Ok(())
    }

    fn replace(&self, svc: &Service, image: &str, version: &str) -> Result<(), SwapFailed> {
        let before = |why: String| SwapFailed {
            started: false,
            why,
        };
        let after = |why: String| SwapFailed { started: true, why };
        let old = self
            .inspect(&svc.name)
            .map_err(before)?
            .ok_or_else(|| before(format!("{} isn't there", svc.name)))?;
        let mut body = self.new_settings(&old, image, version).map_err(before)?;
        let nets = endpoints(&old);
        if let Some((first, e)) = nets.first() {
            body["NetworkingConfig"] = json!({ "EndpointsConfig": { first.clone(): e } });
        }
        let kept = format!("{}{PREVIOUS}", svc.name);
        self.remove(&kept).map_err(before)?;
        self.stop(&svc.name).map_err(before)?;
        if let Err(e) = self.rename(&svc.name, &kept) {
            let _ = self.start(&svc.name);
            return Err(before(e));
        }
        let made = self
            .post(
                &format!("/containers/create?name={}", enc(&svc.name)),
                Some(&body),
            )
            .and_then(|a| a.ok("making the new container"))
            .and_then(|a| a.json())
            .map_err(after)?;
        let id = made["Id"]
            .as_str()
            .ok_or_else(|| after("Docker made a container with no id".into()))?
            .to_owned();
        for (net, e) in nets.iter().skip(1) {
            self.post(
                &format!("/networks/{}/connect", enc(net)),
                Some(&json!({ "Container": id, "EndpointConfig": e })),
            )
            .and_then(|a| a.ok("joining a network"))
            .map_err(after)?;
        }
        self.start(&id).map_err(after)
    }

    fn healthy(&self, name: &str, wait: Duration, settle: Duration) -> Result<(), String> {
        let began = Instant::now();
        let mut running_since: Option<(Instant, u64)> = None;
        loop {
            let v = self
                .inspect(name)?
                .ok_or_else(|| format!("{name} isn't there"))?;
            let state = &v["State"];
            let restarts = v["RestartCount"].as_u64().unwrap_or(0);
            match state["Health"]["Status"].as_str() {
                Some("healthy") => return Ok(()),
                Some("unhealthy") => return Err(format!("{name} failed its health check")),
                Some(_) => {}
                None => {
                    if state["Running"].as_bool() != Some(true) {
                        return Err(format!("{name} stopped"));
                    }
                    match running_since {
                        Some((at, n)) if n == restarts => {
                            if at.elapsed() >= settle {
                                return Ok(());
                            }
                        }
                        _ => running_since = Some((Instant::now(), restarts)),
                    }
                }
            }
            if began.elapsed() >= wait {
                return Err(format!("{name} wasn't healthy in time"));
            }
            std::thread::sleep(
                Duration::from_millis(250)
                    .min(wait / 4)
                    .max(Duration::from_millis(20)),
            );
        }
    }

    fn restore(&self, name: &str) -> Result<(), String> {
        let kept = format!("{name}{PREVIOUS}");
        if self.inspect(&kept)?.is_none() {
            return Err(format!("no earlier version of {name} is kept"));
        }
        self.remove(name)?;
        self.rename(&kept, name)?;
        self.start(name)
    }

    fn has_image(&self, image: &str) -> Result<bool, String> {
        crate::release::image_ok(image)?;
        let a = self.get(&format!("/images/{}/json", enc(image)))?;
        if a.0 == 404 {
            return Ok(false);
        }
        a.ok("looking at an image").map(|_| true)
    }

    fn run_once(
        &self,
        from: &str,
        command: &[String],
        wait: Duration,
    ) -> Result<(i64, String), String> {
        let old = self
            .inspect(from)?
            .ok_or_else(|| format!("{from} isn't there"))?;
        let image = old["Config"]["Image"]
            .as_str()
            .ok_or("a container without an image")?
            .to_owned();
        let version = version_of(labels(&old["Config"])).unwrap_or_default();
        let mut body = self.new_settings(&old, &image, &version)?;
        once_settings(&mut body, command);
        let nets = endpoints(&old);
        if let Some((first, e)) = nets.first() {
            body["NetworkingConfig"] =
                json!({ "EndpointsConfig": { first.clone(): once_endpoint(e) } });
        }
        let name = format!("{from}{ONCE}");
        self.remove(&name)?;
        let made = self
            .post(
                &format!("/containers/create?name={}", enc(&name)),
                Some(&body),
            )?
            .ok("making a one-off container")?
            .json()?;
        let id = made["Id"]
            .as_str()
            .ok_or("Docker made a container with no id")?
            .to_owned();
        let result = (|| {
            for (net, e) in nets.iter().skip(1) {
                self.post(
                    &format!("/networks/{}/connect", enc(net)),
                    Some(&json!({ "Container": id, "EndpointConfig": once_endpoint(e) })),
                )?
                .ok("joining a network")?;
            }
            self.start(&id)?;
            let began = Instant::now();
            loop {
                let v = self
                    .inspect(&id)?
                    .ok_or("the one-off container went away")?;
                if v["State"]["Running"].as_bool() != Some(true) {
                    let code = v["State"]["ExitCode"].as_i64().unwrap_or(-1);
                    return Ok((code, self.output(&id).unwrap_or_default()));
                }
                if began.elapsed() >= wait {
                    return Err(format!("{} didn't finish in time", command.join(" ")));
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        })();
        let _ = self.remove(&id);
        result
    }
}

/// A one-off's settings: the command given, nothing published, no restart, and not one of the
/// app's services (so it is never listed, swapped or kept).
fn once_settings(body: &mut Value, command: &[String]) {
    body["Cmd"] = json!(command);
    if let Some(o) = body.as_object_mut() {
        o.remove("ExposedPorts");
        o.remove("Healthcheck");
    }
    if let Some(l) = body["Labels"].as_object_mut() {
        l.remove(LABEL_APP);
        l.remove(LABEL_SERVICE);
        l.retain(|k, _| !k.starts_with("com.docker.compose."));
    }
    let host = &mut body["HostConfig"];
    if let Some(h) = host.as_object_mut() {
        h.remove("PortBindings");
        h.insert("PublishAllPorts".into(), json!(false));
        h.insert("RestartPolicy".into(), json!({ "Name": "no" }));
        h.insert("AutoRemove".into(), json!(false));
    }
}

/// A one-off joins its networks without the service's aliases or fixed address, so nothing
/// mistakes it for the service.
fn once_endpoint(e: &Value) -> Value {
    let mut e = e.clone();
    if let Some(o) = e.as_object_mut() {
        o.remove("Aliases");
        o.remove("IPAMConfig");
    }
    e
}

impl Docker {
    /// The last lines a container printed. Docker frames each with an 8-byte header unless it has
    /// a terminal; both are read.
    fn output(&self, id: &str) -> Result<String, String> {
        let a = self
            .get(&format!(
                "/containers/{}/logs?stdout=1&stderr=1&tail=40",
                enc(id)
            ))?
            .ok("reading a container's output")?;
        Ok(demux(&a.1))
    }
}

/// Strips Docker's log stream framing, if it is there.
fn demux(b: &[u8]) -> String {
    let framed = b.len() >= 8 && matches!(b[0], 0..=2) && b[1..4] == [0, 0, 0];
    if !framed {
        return String::from_utf8_lossy(b).into_owned();
    }
    let mut out = Vec::new();
    let mut rest = b;
    while rest.len() >= 8 {
        let n = u32::from_be_bytes([rest[4], rest[5], rest[6], rest[7]]) as usize;
        let end = (8 + n).min(rest.len());
        out.extend_from_slice(&rest[8..end]);
        rest = &rest[end..];
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A Docker API that answers every request with an empty pull and keeps each request's head.
    fn fake_docker() -> (Address, std::sync::mpsc::Receiver<String>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let at = Address::Tcp(l.local_addr().unwrap().to_string());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for s in l.incoming() {
                let mut s = s.unwrap();
                let mut buf = [0u8; 8192];
                let n = s.read(&mut buf).unwrap();
                let head = String::from_utf8_lossy(&buf[..n]).into_owned();
                let _ = tx.send(head);
                let _ = s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
        (at, rx)
    }

    fn signed_in(at: Address) -> Docker {
        Docker::new(at)
            .with_registry_auth(RegistryAuth::new("ghcr.io", "sean", "ghp_secret").unwrap())
    }

    #[test]
    fn a_pull_from_the_signed_in_registry_carries_the_sign_in() {
        let (at, heads) = fake_docker();
        let d = signed_in(at);
        d.pull(&format!("ghcr.io/snds/warpcore-web@{DIGEST}"))
            .unwrap();
        let head = heads.recv().unwrap();
        let value = head
            .lines()
            .find_map(|l| l.strip_prefix("X-Registry-Auth: "))
            .expect("the pull carries X-Registry-Auth");
        use base64::Engine as _;
        let json: Value = serde_json::from_slice(
            &base64::engine::general_purpose::URL_SAFE
                .decode(value)
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            json,
            json!({ "username": "sean", "password": "ghp_secret", "serveraddress": "ghcr.io" })
        );
    }

    #[test]
    fn the_sign_in_never_goes_to_another_registry_or_another_call() {
        let (at, heads) = fake_docker();
        let d = signed_in(at);
        d.pull(&format!("docker.io/library/busybox@{DIGEST}"))
            .unwrap();
        assert!(!heads.recv().unwrap().contains("X-Registry-Auth"));
        d.pull(&format!("ghcr.io.evil.example/snds/web@{DIGEST}"))
            .unwrap();
        assert!(!heads.recv().unwrap().contains("X-Registry-Auth"));
        let _ = d.inspect("warpcore-web-1");
        assert!(!heads.recv().unwrap().contains("X-Registry-Auth"));
    }

    #[test]
    fn without_a_sign_in_a_pull_sends_none() {
        let (at, heads) = fake_docker();
        Docker::new(at)
            .pull(&format!("ghcr.io/snds/warpcore-web@{DIGEST}"))
            .unwrap();
        assert!(!heads.recv().unwrap().contains("X-Registry-Auth"));
    }

    #[test]
    fn registries_are_read_as_docker_reads_them() {
        assert_eq!(registry_of(&format!("ghcr.io/snds/a@{DIGEST}")), "ghcr.io");
        assert_eq!(registry_of(&format!("GHCR.io/snds/a@{DIGEST}")), "ghcr.io");
        assert_eq!(
            registry_of(&format!("localhost:5000/a@{DIGEST}")),
            "localhost:5000"
        );
        assert_eq!(registry_of(&format!("snds/a@{DIGEST}")), "docker.io");
        assert_eq!(registry_of(&format!("busybox@{DIGEST}")), "docker.io");
    }

    #[test]
    fn a_sign_in_needs_a_host_user_and_token_and_never_prints_the_token() {
        assert!(RegistryAuth::new("", "u", "t").is_err());
        assert!(RegistryAuth::new("ghcr.io/x", "u", "t").is_err());
        assert!(RegistryAuth::new("ghcr.io", "", "t").is_err());
        assert!(RegistryAuth::new("ghcr.io", "u", " ").is_err());
        let a = RegistryAuth::new("ghcr.io", "sean", "ghp_secret").unwrap();
        assert!(!format!("{a:?}").contains("ghp_secret"));
    }

    #[test]
    fn answers_parse_with_a_length_or_in_chunks() {
        let a = parse_response(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}junk").unwrap();
        assert_eq!((a.0, a.1.as_slice()), (201, b"{}".as_slice()));
        let a = parse_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x\r\nde\r\n0\r\n\r\n",
        )
        .unwrap();
        assert_eq!(a.1, b"abcde");
        assert!(
            parse_response(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n9\r\nab")
                .is_err()
        );
    }

    #[test]
    fn addresses_are_tcp_or_unix() {
        assert_eq!(
            Address::parse("tcp://proxy:2375").unwrap(),
            Address::Tcp("proxy:2375".into())
        );
        assert_eq!(
            Address::parse("unix:///var/run/docker.sock").unwrap(),
            Address::Unix("/var/run/docker.sock".into())
        );
        assert!(Address::parse("http://proxy:2375").is_err());
        assert!(Address::parse("tcp://").is_err());
    }

    #[test]
    fn what_the_old_image_set_is_left_to_the_new_one() {
        let mut cfg = json!({
            "Hostname": "0123456789ab",
            "Env": ["PATH=/bin", "MINE=1"],
            "Labels": { "from.image": "x", "mine": "y", "org.snds.update.app": "warpcore" },
            "Cmd": ["serve"],
            "User": "me",
        })
        .as_object()
        .cloned()
        .unwrap();
        let image = json!({ "Env": ["PATH=/bin"], "Labels": { "from.image": "x" }, "Cmd": ["serve"], "User": "other" });
        strip_image_defaults(&mut cfg, &image, "0123456789abcdef");
        assert_eq!(
            Value::Object(cfg),
            json!({
                "Env": ["MINE=1"],
                "Labels": { "mine": "y", "org.snds.update.app": "warpcore" },
                "User": "me",
            })
        );
    }

    #[test]
    fn networks_keep_aliases_and_addresses_but_not_runtime_fields() {
        let old = json!({
            "Id": "0123456789abcdef",
            "HostConfig": { "NetworkMode": "warp" },
            "NetworkSettings": { "Networks": {
                "bridge": { "Aliases": null, "IPAddress": "172.17.0.2" },
                "warp": { "Aliases": ["web", "0123456789ab"], "IPAMConfig": { "IPv4Address": "10.0.0.5" }, "NetworkID": "n", "EndpointID": "e" }
            }}
        });
        let e = endpoints(&old);
        assert_eq!(e[0].0, "warp");
        assert_eq!(
            e[0].1,
            json!({ "Aliases": ["web"], "IPAMConfig": { "IPv4Address": "10.0.0.5" } })
        );
        assert_eq!(e[1], ("bridge".to_owned(), json!({})));
        let host = json!({ "HostConfig": { "NetworkMode": "host" }, "NetworkSettings": { "Networks": { "host": {} } } });
        assert!(endpoints(&host).is_empty());
    }

    #[test]
    fn a_one_off_publishes_nothing_restarts_never_and_isnt_a_service() {
        let mut body = json!({
            "Image": "x@sha256:aa",
            "Cmd": ["serve"],
            "ExposedPorts": { "8080/tcp": {} },
            "Labels": { "org.snds.update.app": "warpcore", "org.snds.update.service": "sandbox",
                        "com.docker.compose.service": "sandbox", "mine": "y" },
            "HostConfig": { "PortBindings": { "8080/tcp": [{ "HostPort": "8080" }] },
                            "RestartPolicy": { "Name": "always" }, "Devices": [{ "PathOnHost": "/dev/kvm" }],
                            "CapDrop": ["ALL"], "ReadonlyRootfs": true },
        });
        once_settings(&mut body, &["node".into(), "dist/selftest.js".into()]);
        assert_eq!(body["Cmd"], json!(["node", "dist/selftest.js"]));
        assert!(body.get("ExposedPorts").is_none());
        assert_eq!(body["Labels"], json!({ "mine": "y" }));
        assert!(body["HostConfig"].get("PortBindings").is_none());
        assert_eq!(body["HostConfig"]["RestartPolicy"]["Name"], "no");
        // Its limits are the service's own: a one-off is never looser than what it copies.
        assert_eq!(body["HostConfig"]["CapDrop"], json!(["ALL"]));
        assert_eq!(body["HostConfig"]["ReadonlyRootfs"], true);
        assert_eq!(body["HostConfig"]["Devices"][0]["PathOnHost"], "/dev/kvm");
        assert_eq!(
            once_endpoint(&json!({ "Aliases": ["sandbox"], "IPAMConfig": {}, "Links": ["a"] })),
            json!({ "Links": ["a"] })
        );
    }

    #[test]
    fn output_is_read_framed_or_plain() {
        let mut framed = vec![1, 0, 0, 0, 0, 0, 0, 3];
        framed.extend_from_slice(b"ok\n");
        framed.extend_from_slice(&[2, 0, 0, 0, 0, 0, 0, 4]);
        framed.extend_from_slice(b"bad\n");
        assert_eq!(demux(&framed), "ok\nbad\n");
        assert_eq!(demux(b"plain text\n"), "plain text\n");
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(enc("a=b c/@:"), "a%3Db%20c%2F%40%3A");
    }
}
