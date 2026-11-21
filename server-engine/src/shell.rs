//! The service app: one per product, running the engine for that product only.
//!
//! It reads a config file built into the product's image ([`Config`]), keeps the person's choice
//! and past updates in its data folder, checks at start and every six hours, and serves a small
//! HTTP API on the product's private networks:
//!
//! | Call | Who | What |
//! |---|---|---|
//! | `GET /healthz` | anyone on its networks | `ok` |
//! | `GET /v1/status` | anyone on its networks | the status, read-only (the status-only view for other apps) |
//! | `POST /v1/check` | the product, with its token | **Check for updates** (downloads a newer release ahead of time) |
//! | `POST /v1/update` | the product | **Restart to update**: answers 202 at once |
//! | `POST /v1/rollback` | the product | **Roll back**: answers 202 at once |
//! | `PUT /v1/policy` | the product | the automatic or by-hand choice |
//!
//! The token (`SERVICE_APP_TOKEN`) is shared with the product's own API, which shows Settings →
//! Updates to its owners. It is read from the environment only, and never printed.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

use crate::docker::{Address, Docker, Engine, RegistryAuth, Service};
use crate::engine::{Feed, Hooks, Policy, Saved, Settings, Store, Trigger, Updater};
use crate::release::GITHUB_HOSTS;

/// The service app's config, built into the product's image (`/etc/service-app/app.toml`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub app: AppConfig,
    #[serde(default)]
    pub listen: Listen,
    pub docker: DockerConfig,
    #[serde(default)]
    pub hooks: HookConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppConfig {
    pub id: String,
    pub name: String,
    pub endpoint: String,
    /// The release key's `.pub` text (base64), in the image. Never a setting on the server: anyone
    /// who could change it could sign their own releases.
    pub key_file: PathBuf,
    #[serde(default)]
    pub optional: Vec<String>,
    #[serde(default)]
    pub supervisor: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Listen {
    pub addr: String,
}

impl Default for Listen {
    fn default() -> Listen {
        Listen {
            addr: "0.0.0.0:7490".into(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DockerConfig {
    /// The service app's own socket proxy, `tcp://host:port`.
    pub address: String,
    /// The registry the sign-in is for. The user and token come from the environment
    /// (`SERVICE_APP_REGISTRY_USER`, `SERVICE_APP_REGISTRY_TOKEN`: the template's masked fields).
    #[serde(default = "default_registry")]
    pub registry: String,
}

fn default_registry() -> String {
    "ghcr.io".into()
}

/// The product's steps around an update, as fixed commands and addresses. Nothing here comes
/// from a request.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookConfig {
    /// `GET`: `{"blockers": ["…"]}`, the running work an update would interrupt.
    pub blockers_url: Option<String>,
    /// `POST {"hold": true|false}`: stop new work starting, then let it start again.
    pub hold_url: Option<String>,
    /// Run in the service app before anything changes; it must make a new entry in
    /// `backups_dir`, whose name is shown with the update.
    #[serde(default)]
    pub before_command: Vec<String>,
    pub backups_dir: Option<PathBuf>,
    /// After every service is healthy: `after_command` in a one-off copy of `after_service`.
    pub after_service: Option<String>,
    #[serde(default)]
    pub after_command: Vec<String>,
}

impl Config {
    pub fn read(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let c: Config = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        c.validate()?;
        Ok(c)
    }

    fn validate(&self) -> Result<(), String> {
        let id_ok = |s: &str| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        };
        if !id_ok(&self.app.id) {
            return Err(format!("{}: not an app id", self.app.id));
        }
        Address::parse(&self.docker.address)?;
        let h = &self.hooks;
        if h.after_service.is_some() == h.after_command.is_empty() {
            return Err("hooks: after_service and after_command go together".into());
        }
        if !h.before_command.is_empty() && h.backups_dir.is_none() {
            return Err("hooks: before_command needs backups_dir, to name the backup".into());
        }
        Ok(())
    }

    pub fn key(&self) -> Result<String, String> {
        Ok(std::fs::read_to_string(&self.app.key_file)
            .map_err(|e| format!("{}: {e}", self.app.key_file.display()))?
            .trim()
            .to_owned())
    }

    pub fn feed(&self) -> Result<Feed, String> {
        Ok(Feed {
            id: self.app.id.clone(),
            name: self.app.name.clone(),
            endpoint: self.app.endpoint.clone(),
            key: self.key()?,
            hosts: GITHUB_HOSTS.iter().map(|h| (*h).to_owned()).collect(),
            optional: self.app.optional.clone(),
            supervisor: self.app.supervisor.clone(),
        })
    }
}

/// [`Saved`] in `<data>/state.json`, written whole and renamed into place, mode 600.
pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(dir: &Path) -> FileStore {
        FileStore {
            path: dir.join("state.json"),
        }
    }
}

impl Store for FileStore {
    fn load(&self) -> Saved {
        std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self, saved: &Saved) -> Result<(), String> {
        let tmp = self.path.with_extension("json.new");
        let body = serde_json::to_vec_pretty(saved).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        std::fs::rename(&tmp, &self.path).map_err(|e| format!("{}: {e}", self.path.display()))
    }
}

/// The product's steps, from [`HookConfig`].
pub struct ConfigHooks {
    cfg: HookConfig,
    token: Option<String>,
    agent: ureq::Agent,
}

impl ConfigHooks {
    pub fn new(cfg: HookConfig, token: Option<String>) -> ConfigHooks {
        let agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        ConfigHooks { cfg, token, agent }
    }

    fn bearer(&self) -> String {
        format!("Bearer {}", self.token.as_deref().unwrap_or_default())
    }

    fn hold(&self, on: bool) -> Result<(), String> {
        let Some(url) = &self.cfg.hold_url else {
            return Ok(());
        };
        let resp = self
            .agent
            .post(url)
            .header("Authorization", &self.bearer())
            .header("Content-Type", "application/json")
            .send(json!({ "hold": on }).to_string())
            .map_err(|e| format!("asking the product to hold new work: {e}"))?;
        let code = resp.status().as_u16();
        if (200..300).contains(&code) {
            Ok(())
        } else {
            Err(format!(
                "asking the product to hold new work: it answered {code}"
            ))
        }
    }

    fn backups(&self) -> BTreeSet<String> {
        self.cfg
            .backups_dir
            .as_ref()
            .and_then(|d| std::fs::read_dir(d).ok())
            .map(|r| {
                r.filter_map(|e| e.ok()?.file_name().into_string().ok())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// The last few lines of `text`, for an error.
fn tail(text: &str) -> String {
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(5)..].join(" / ")
}

impl Hooks for ConfigHooks {
    fn blockers(&self) -> Result<Vec<String>, String> {
        let Some(url) = &self.cfg.blockers_url else {
            return Ok(Vec::new());
        };
        let mut resp = self
            .agent
            .get(url)
            .header("Authorization", &self.bearer())
            .call()
            .map_err(|e| format!("asking the product what is running: {e}"))?;
        let code = resp.status().as_u16();
        if code != 200 {
            return Err(format!(
                "asking the product what is running: it answered {code}"
            ));
        }
        #[derive(Deserialize)]
        struct Answer {
            blockers: Vec<String>,
        }
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024)
            .read_to_vec()
            .map_err(|e| format!("what the product said is running: {e}"))?;
        let a: Answer = serde_json::from_slice(&body)
            .map_err(|e| format!("what the product said is running: {e}"))?;
        Ok(a.blockers)
    }

    fn before(&self, _from: Option<&str>, _to: &str) -> Result<Option<String>, String> {
        self.hold(true)?;
        let Some((cmd, args)) = self.cfg.before_command.split_first() else {
            return Ok(None);
        };
        let had = self.backups();
        let out = std::process::Command::new(cmd)
            .args(args)
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|e| format!("{cmd}: {e}"))?;
        if !out.status.success() {
            return Err(format!(
                "the backup failed: {}",
                tail(&String::from_utf8_lossy(&out.stderr))
            ));
        }
        let made = self.backups().difference(&had).max().cloned();
        made.map(Some)
            .ok_or_else(|| "the backup step finished but made no backup".into())
    }

    fn after(
        &self,
        engine: &dyn Engine,
        services: &[Service],
        wait: Duration,
    ) -> Result<(), String> {
        let Some(svc) = &self.cfg.after_service else {
            return Ok(());
        };
        let from = services
            .iter()
            .find(|s| &s.service == svc)
            .ok_or_else(|| format!("no {svc} service to run the self-test in"))?;
        let (code, out) = engine.run_once(&from.name, &self.cfg.after_command, wait)?;
        if code == 0 {
            Ok(())
        } else {
            Err(format!("exit {code}: {}", tail(&out)))
        }
    }

    fn done(&self) {
        let _ = self.hold(false);
    }
}

/// Compares two strings in time that doesn't depend on where they differ.
fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// The shortest token accepted. A shorter one turns the calls that change things off.
pub const TOKEN_MIN: usize = 32;
const BODY_MAX: u64 = 16 * 1024;
const CONNECTIONS_MAX: usize = 16;

/// The HTTP API around an [`Updater`]. `token`: the product's shared token, or `None` to refuse
/// every call that changes something.
pub struct Api {
    pub updater: Updater,
    token: Option<String>,
}

impl Api {
    pub fn new(updater: Updater, token: Option<String>) -> Api {
        let token = token.filter(|t| t.trim().len() >= TOKEN_MIN);
        Api { updater, token }
    }

    fn authorized(&self, req: &tiny_http::Request) -> bool {
        let Some(token) = &self.token else {
            return false;
        };
        req.headers()
            .iter()
            .find(|h| h.field.equiv("Authorization"))
            .and_then(|h| h.value.as_str().strip_prefix("Bearer "))
            .is_some_and(|t| same(t.trim(), token))
    }

    /// Answers one request: its status code and JSON body.
    pub fn answer(&self, req: &mut tiny_http::Request) -> (u16, serde_json::Value) {
        let method = req.method().as_str().to_owned();
        let path = req.url().split('?').next().unwrap_or_default().to_owned();
        let err = |code: u16, e: &str| (code, json!({ "error": e }));
        match (method.as_str(), path.as_str()) {
            ("GET", "/healthz") => (200, json!("ok")),
            ("GET", "/v1/status") => (200, json!(self.updater.status())),
            ("POST" | "PUT", "/v1/check" | "/v1/update" | "/v1/rollback" | "/v1/policy") => {
                if !self.authorized(req) {
                    return err(401, "this call needs the product's token");
                }
                let mut body = String::new();
                if req
                    .as_reader()
                    .take(BODY_MAX + 1)
                    .read_to_string(&mut body)
                    .is_err()
                    || body.len() as u64 > BODY_MAX
                {
                    return err(413, "too large");
                }
                let done = match (method.as_str(), path.as_str()) {
                    ("POST", "/v1/check") => self.updater.check().map(|s| (200, s)),
                    ("POST", "/v1/update") => {
                        self.updater.start_update(Trigger::ByHand).map(|s| (202, s))
                    }
                    ("POST", "/v1/rollback") => self.updater.start_rollback().map(|s| (202, s)),
                    ("PUT", "/v1/policy") => serde_json::from_str::<Policy>(&body)
                        .map_err(|e| format!("not a policy: {e}"))
                        .and_then(|p| self.updater.set_policy(p))
                        .map(|s| (200, s)),
                    _ => return err(405, "not allowed"),
                };
                match done {
                    Ok((code, st)) => (code, json!(st)),
                    Err(e) => err(409, &e),
                }
            }
            _ => err(404, "no such call"),
        }
    }

    /// Serves on `addr` until the process ends.
    pub fn serve(self: Arc<Self>, addr: &str) -> Result<(), String> {
        let server = tiny_http::Server::http(addr).map_err(|e| format!("{addr}: {e}"))?;
        eprintln!("service-app: listening on {addr}");
        self.serve_on(server);
        Ok(())
    }

    /// Serves on a listener already bound (tests bind port 0).
    pub fn serve_on(self: Arc<Self>, server: tiny_http::Server) {
        let open = Arc::new(AtomicUsize::new(0));
        for mut req in server.incoming_requests() {
            if open.load(Ordering::Relaxed) >= CONNECTIONS_MAX {
                let _ = req.respond(tiny_http::Response::from_string("busy").with_status_code(503));
                continue;
            }
            open.fetch_add(1, Ordering::Relaxed);
            let (api, open) = (self.clone(), open.clone());
            std::thread::spawn(move || {
                let (code, body) = api.answer(&mut req);
                let peer = req
                    .remote_addr()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_default();
                if req.method() != &tiny_http::Method::Get {
                    eprintln!(
                        "service-app: {} {} from {peer}: {code}",
                        req.method(),
                        req.url()
                    );
                }
                let resp = tiny_http::Response::from_string(body.to_string())
                    .with_status_code(code)
                    .with_header(
                        tiny_http::Header::from_bytes("Content-Type", "application/json")
                            .expect("a fixed header"),
                    );
                let _ = req.respond(resp);
                open.fetch_sub(1, Ordering::Relaxed);
            });
        }
    }
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

/// Builds the service app from its config and the environment.
pub fn build(cfg: &Config) -> Result<(Arc<Api>, Updater), String> {
    let token = env("SERVICE_APP_TOKEN");
    if token.as_ref().is_none_or(|t| t.trim().len() < TOKEN_MIN) {
        eprintln!(
            "service-app: SERVICE_APP_TOKEN is missing or shorter than {TOKEN_MIN} characters, so \
             only the status can be read."
        );
    }
    let mut docker = Docker::new(Address::parse(&cfg.docker.address)?);
    match (
        env("SERVICE_APP_REGISTRY_USER"),
        env("SERVICE_APP_REGISTRY_TOKEN"),
    ) {
        (Some(u), Some(t)) => {
            docker = docker.with_registry_auth(RegistryAuth::new(&cfg.docker.registry, &u, &t)?);
        }
        (None, None) => {}
        _ => {
            return Err(
                "a registry sign-in needs both SERVICE_APP_REGISTRY_USER and \
                 SERVICE_APP_REGISTRY_TOKEN"
                    .into(),
            );
        }
    }
    let data = PathBuf::from(env("SERVICE_APP_DATA").unwrap_or_else(|| "/data".into()));
    let settings = Settings::new(cfg.feed()?);
    let updater = Updater::new(
        settings,
        Box::new(docker),
        Box::new(ConfigHooks::new(cfg.hooks.clone(), token.clone())),
        Box::new(FileStore::new(&data)),
    );
    Ok((Arc::new(Api::new(updater.clone(), token)), updater))
}

/// Runs the service app: the timer on its own thread, the API on this one.
pub fn run(cfg: &Config) -> Result<(), String> {
    let (api, updater) = build(cfg)?;
    std::thread::spawn(move || {
        loop {
            updater.tick();
            std::thread::sleep(Duration::from_secs(60));
        }
    });
    api.serve(&cfg.listen.addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_compare_whole() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "abcd"));
        assert!(!same("", "a"));
    }

    #[test]
    fn a_config_reads_and_its_hooks_go_together() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("app.toml");
        let good = r#"
[app]
id = "warpcore"
name = "Warpcore"
endpoint = "https://github.com/snds/releases/releases/download/warpcore-server/latest.json"
key_file = "/etc/service-app/release.pub"
optional = ["artifacts"]
supervisor = "server"

[docker]
address = "tcp://docker-proxy:2375"

[hooks]
blockers_url = "http://api:3000/api/internal/updates/blockers"
hold_url = "http://api:3000/api/internal/updates/hold"
before_command = ["sh", "/backup/backup.sh", "once"]
backups_dir = "/backups"
after_service = "sandbox"
after_command = ["node", "dist/selftest.js"]
"#;
        std::fs::write(&path, good).unwrap();
        let c = Config::read(&path).unwrap();
        assert_eq!(c.listen.addr, "0.0.0.0:7490");
        assert_eq!(c.docker.registry, "ghcr.io");
        assert_eq!(c.app.optional, vec!["artifacts"]);
        for bad in [
            good.replace("after_service = \"sandbox\"\n", ""),
            good.replace("backups_dir = \"/backups\"\n", ""),
            good.replace("tcp://docker-proxy:2375", "http://docker-proxy:2375"),
            good.replace("id = \"warpcore\"", "id = \"Warp core\""),
            good.replace("[hooks]", "[hooks]\nshell = \"rm -rf /\""),
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(Config::read(&path).is_err());
        }
    }

    #[test]
    fn the_file_store_keeps_the_choice_and_history() {
        let dir = tempfile::tempdir().unwrap();
        let s = FileStore::new(dir.path());
        assert_eq!(s.load(), Saved::default());
        let saved = Saved {
            policy: Policy {
                auto_check: false,
                mode: crate::engine::Mode::NothingLost,
            },
            history: Vec::new(),
        };
        s.save(&saved).unwrap();
        assert_eq!(FileStore::new(dir.path()).load(), saved);
    }

    #[test]
    fn a_backup_step_must_make_a_backup_and_its_name_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join("backups");
        std::fs::create_dir(&backups).unwrap();
        std::fs::create_dir(backups.join("2026-11-19T033000Z")).unwrap();
        let hooks = |cmd: Vec<String>| {
            ConfigHooks::new(
                HookConfig {
                    before_command: cmd,
                    backups_dir: Some(backups.clone()),
                    ..HookConfig::default()
                },
                None,
            )
        };
        let made = backups.join("2026-11-20T120000Z");
        let h = hooks(vec!["mkdir".into(), made.display().to_string()]);
        assert_eq!(
            h.before(Some("1.0.0"), "1.1.0").unwrap().as_deref(),
            Some("2026-11-20T120000Z")
        );
        let h = hooks(vec!["true".into()]);
        assert!(
            h.before(None, "1.1.0")
                .unwrap_err()
                .contains("made no backup")
        );
        let h = hooks(vec!["false".into()]);
        assert!(
            h.before(None, "1.1.0")
                .unwrap_err()
                .contains("backup failed")
        );
    }
}
