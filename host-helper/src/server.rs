//! The helper: checks each request, then does the one allowed thing.
//!
//! Order of checks, so that nobody without a stack's key can make the helper do work or fill
//! its memory: the global limit on failed requests, the path, the headers, the time, the
//! signature, then the nonce (stored only after the signature passed), the stack's rate, and
//! one change at a time per stack.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::allow::{AllowList, Stack};
use crate::docker::{self, Docker};
use crate::http::{self, Message, ReadError};
use crate::protocol::{
    self, ErrorReply, Finished, H_NONCE, H_SIGNATURE, H_STACK, H_TIME, MAX_BODY, MAX_SKEW_SECS,
    Operation, Progress, PulledImage, RestartReply, Running, ServiceStatus, StatusReply,
    StepReport, UpdateReply, UpdateRequest, step,
};

/// Signed requests one stack may make in a minute.
pub const STACK_PER_MINUTE: usize = 30;
/// Failed requests (any stack, or none) in a minute before the helper refuses everything
/// unsigned for the rest of that minute.
pub const FAILURES_PER_MINUTE: usize = 20;
/// Connections handled at once.
pub const MAX_CONNECTIONS: usize = 16;
/// Remembered nonces per stack (a signed request each, at most `STACK_PER_MINUTE` a minute,
/// kept for the replay window).
const MAX_NONCES: usize = 1024;

/// One reply, before it is written.
#[derive(Debug)]
pub struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
    /// Set for a request whose signature passed: the reply is signed for its nonce.
    pub sign_for: Option<(String, String)>,
}

#[derive(Default)]
struct StackState {
    busy: Mutex<()>,
    nonces: Mutex<HashMap<String, u64>>,
    recent: Mutex<VecDeque<u64>>,
    /// Read by `status` while a change runs, so it never waits on `busy`.
    progress: Mutex<Progress>,
}

impl StackState {
    fn progress(&self) -> std::sync::MutexGuard<'_, Progress> {
        self.progress.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn at(&self, step: &str) {
        if let Some(r) = self.progress().running.as_mut() {
            r.step = step.to_owned();
        }
    }
}

/// The helper's state: the list, Docker, and the counters.
pub struct Helper<D: Docker> {
    list: AllowList,
    docker: D,
    states: BTreeMap<String, StackState>,
    failures: Mutex<VecDeque<u64>>,
    clock: Box<dyn Fn() -> u64 + Send + Sync>,
    log: Box<dyn Fn(&str) + Send + Sync>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn refuse(status: u16, code: &str, message: impl Into<String>) -> Reply {
    let body = serde_json::to_vec(&ErrorReply {
        error: code.into(),
        message: message.into(),
    })
    .expect("an error always serializes");
    Reply {
        status,
        body,
        sign_for: None,
    }
}

fn trim_window(q: &mut VecDeque<u64>, now: u64) {
    while q.front().is_some_and(|t| *t + 60 <= now) {
        q.pop_front();
    }
}

impl<D: Docker> Helper<D> {
    pub fn new(list: AllowList, docker: D) -> Self {
        let states = list
            .stacks
            .keys()
            .map(|k| (k.clone(), StackState::default()))
            .collect();
        Helper {
            list,
            docker,
            states,
            failures: Mutex::new(VecDeque::new()),
            clock: Box::new(now),
            log: Box::new(|line| eprintln!("{line}")),
        }
    }

    /// The same, with its own clock and log (tests).
    pub fn with_clock_and_log(
        mut self,
        clock: impl Fn() -> u64 + Send + Sync + 'static,
        log: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        self.clock = Box::new(clock);
        self.log = Box::new(log);
        self
    }

    pub fn list(&self) -> &AllowList {
        &self.list
    }

    fn failed(&self, now: u64) {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        trim_window(&mut f, now);
        f.push_back(now);
    }

    fn too_many_failures(&self, now: u64) -> bool {
        let mut f = self.failures.lock().unwrap_or_else(|e| e.into_inner());
        trim_window(&mut f, now);
        f.len() >= FAILURES_PER_MINUTE
    }

    /// Handles one parsed request from `peer` (an address, or `socket`).
    pub fn handle(&self, req: &Message, peer: &str) -> Reply {
        let now = (self.clock)();
        let path = req.start.get(1).map(String::as_str).unwrap_or("");
        let op_name = Operation::from_path(path).map_or("unknown", Operation::name);
        let stack_name = req.header(H_STACK).unwrap_or("-");
        let shown_stack = if protocol::valid_name(stack_name) {
            stack_name
        } else {
            "?"
        };
        let reply = self.decide(req, now);
        let result = if reply.status == 200 {
            "ok".to_owned()
        } else {
            serde_json::from_slice::<ErrorReply>(&reply.body)
                .map(|e| format!("refused {} {}", reply.status, e.error))
                .unwrap_or_else(|_| format!("refused {}", reply.status))
        };
        if reply.status != 200 && reply.sign_for.is_none() {
            self.failed(now);
        }
        let detail = if op_name == "update" && reply.sign_for.is_some() {
            serde_json::from_slice::<UpdateRequest>(&req.body)
                .ok()
                .and_then(|u| u.tag)
                .filter(|t| protocol::valid_tag(t))
                .map(|t| format!(" tag={t}"))
                .unwrap_or_default()
        } else {
            String::new()
        };
        (self.log)(&format!(
            "{} peer={peer} stack={shown_stack} op={op_name}{detail} result={result}",
            utc(now)
        ));
        reply
    }

    fn decide(&self, req: &Message, now: u64) -> Reply {
        if self.too_many_failures(now) {
            return refuse(
                429,
                "too_many_failures",
                "Too many refused requests in the last minute. Wait a minute and try again.",
            );
        }
        if req.start.first().map(String::as_str) != Some("POST") {
            return refuse(405, "method", "Only POST requests are accepted.");
        }
        let Some(op) = req.start.get(1).and_then(|p| Operation::from_path(p)) else {
            return refuse(
                404,
                "unknown_operation",
                "The helper does status, update and restart, nothing else.",
            );
        };
        let (Some(stack_name), Some(time), Some(nonce), Some(sig)) = (
            req.header(H_STACK),
            req.header(H_TIME),
            req.header(H_NONCE),
            req.header(H_SIGNATURE),
        ) else {
            return refuse(
                401,
                "unsigned",
                "The request isn't signed. Servers call the helper through its client, with their helper key.",
            );
        };
        let Some(stack) = self.list.stacks.get(stack_name) else {
            return refuse(
                401,
                "unknown_stack",
                "The helper doesn't manage a stack by that name. Check HOST_HELPER_STACK and the allow-list.",
            );
        };
        let Ok(time) = time.parse::<u64>() else {
            return refuse(401, "bad_time", "The request's time is malformed.");
        };
        if time.abs_diff(now) > MAX_SKEW_SECS {
            return refuse(
                401,
                "stale",
                "The request is too old or from the future. Check that the server's and the host's clocks agree.",
            );
        }
        if !protocol::valid_nonce(nonce) {
            return refuse(401, "bad_nonce", "The request's nonce is malformed.");
        }
        let input = protocol::request_input("POST", op.path(), stack_name, time, nonce, &req.body);
        if !protocol::verify(&stack.key, &input, sig) {
            return refuse(
                401,
                "bad_signature",
                "The request's signature doesn't match this stack's key. Check HOST_HELPER_KEY in the server's env file.",
            );
        }
        // Signed by the stack from here on: replies are signed too.
        let signed = |mut r: Reply| {
            r.sign_for = Some((stack.name.clone(), nonce.to_owned()));
            r
        };
        let state = &self.states[&stack.name];
        {
            let mut seen = state.nonces.lock().unwrap_or_else(|e| e.into_inner());
            seen.retain(|_, t| t.abs_diff(now) <= 2 * MAX_SKEW_SECS);
            if seen.contains_key(nonce) {
                return signed(refuse(
                    401,
                    "replayed",
                    "This request was already used once, so it was refused.",
                ));
            }
            if seen.len() >= MAX_NONCES {
                return signed(refuse(
                    429,
                    "busy",
                    "Too many requests from this stack. Wait a minute and try again.",
                ));
            }
            seen.insert(nonce.to_owned(), time);
        }
        {
            let mut recent = state.recent.lock().unwrap_or_else(|e| e.into_inner());
            trim_window(&mut recent, now);
            if recent.len() >= STACK_PER_MINUTE {
                return signed(refuse(
                    429,
                    "rate_limited",
                    format!(
                        "Too many requests from {} in the last minute. Wait a minute and try again.",
                        stack.name
                    ),
                ));
            }
            recent.push_back(now);
        }
        let reply = match op {
            Operation::Status => self.status(stack, state),
            Operation::Update | Operation::Restart => {
                let Ok(_guard) = state.busy.try_lock() else {
                    return signed(refuse(
                        409,
                        "busy",
                        format!(
                            "An update or reload of {} is already running. Try again when it finishes.",
                            stack.name
                        ),
                    ));
                };
                let first = if op == Operation::Update {
                    step::REQUEST
                } else {
                    step::RESTART
                };
                state.progress().running = Some(Running {
                    operation: op.name().into(),
                    step: first.into(),
                    started: now,
                });
                let reply = if op == Operation::Update {
                    self.update(stack, &req.body, &|s: &str| state.at(s))
                } else {
                    self.restart(stack)
                };
                let mut progress = state.progress();
                let running = progress.running.take();
                let error = (reply.status != 200)
                    .then(|| serde_json::from_slice::<ErrorReply>(&reply.body).ok())
                    .flatten();
                let after = (reply.status == 200 && op == Operation::Update)
                    .then(|| serde_json::from_slice::<UpdateReply>(&reply.body).ok())
                    .flatten()
                    .and_then(|u| u.after);
                progress.last = Some(Finished {
                    operation: op.name().into(),
                    started: now,
                    finished: (self.clock)(),
                    ok: reply.status == 200,
                    failed_step: (reply.status != 200)
                        .then(|| running.map(|r| r.step))
                        .flatten(),
                    error,
                    after,
                });
                reply
            }
        };
        signed(reply)
    }

    fn status(&self, stack: &Stack, state: &StackState) -> Reply {
        let mut services = Vec::new();
        for svc in &stack.services {
            let containers = match self.docker.containers(stack, svc) {
                Ok(c) => c,
                Err(e) => return docker_failed("read the status", &e),
            };
            if containers.is_empty() {
                services.push(ServiceStatus {
                    service: svc.clone(),
                    state: "missing".into(),
                    image: None,
                    digest: None,
                    version: None,
                });
            }
            for c in containers {
                let info = self.docker.image(&c.image_id).unwrap_or_default();
                services.push(ServiceStatus {
                    service: svc.clone(),
                    state: c.state,
                    image: Some(c.config_image),
                    digest: stack
                        .repositories
                        .iter()
                        .find_map(|r| docker::digest_for(&info, r)),
                    version: info.version,
                });
            }
        }
        ok(&StatusReply {
            stack: stack.name.clone(),
            repository: stack.repository().to_owned(),
            repositories: stack.repositories.clone(),
            services,
            progress: state.progress().clone(),
        })
    }

    /// Pulls every image and checks every digest, runs `before`, then points compose's tag
    /// at the pulled builds and recreates the services together, then runs `after`. Nothing is
    /// recreated unless every pull, every check and `before` passed. `at` names each step.
    fn update(&self, stack: &Stack, body: &[u8], at: &dyn Fn(&str)) -> Reply {
        let req: UpdateRequest = if body.iter().all(u8::is_ascii_whitespace) {
            UpdateRequest::default()
        } else {
            match serde_json::from_slice(body) {
                Ok(r) => r,
                Err(_) => {
                    return refuse(
                        400,
                        "bad_request",
                        "The update request couldn't be read: it takes only `tag`, `expect_digest` and `expect_digests`.",
                    );
                }
            }
        };
        let tag = req.tag.unwrap_or_else(|| stack.compose_tag.clone());
        if !stack.allows_tag(&tag) {
            return refuse(
                403,
                "tag_not_allowed",
                format!(
                    "The tag {tag:?} isn't allowed for {}: only {} or a main-channel version (x.y.z-main.<time>).",
                    stack.name,
                    stack.tags.join(", ")
                ),
            );
        }
        let expected = match expected_digests(stack, &tag, req.expect_digest, req.expect_digests) {
            Ok(e) => e,
            Err(r) => return r,
        };

        at(step::PULL);
        for repo in &stack.repositories {
            let reference = format!("{repo}:{tag}");
            if let Err(e) = self.docker.pull(&reference) {
                return docker_failed(&format!("pull {reference}. Nothing was restarted"), &e);
            }
        }

        at(step::CHECK);
        let mut images = Vec::new();
        for (i, repo) in stack.repositories.iter().enumerate() {
            let reference = format!("{repo}:{tag}");
            let info = match self.docker.image(&reference) {
                Ok(i) => i,
                Err(e) => return docker_failed("read the pulled image. Nothing was restarted", &e),
            };
            let Some(digest) = docker::digest_for(&info, repo) else {
                return refuse(
                    502,
                    "no_digest",
                    format!(
                        "The pulled image has no digest from {repo}, so it wasn't used. Nothing was restarted."
                    ),
                );
            };
            if let Some(want) = &expected[i]
                && *want != digest
            {
                return refuse(
                    409,
                    "digest_mismatch",
                    format!(
                        "The pulled image {reference} isn't the expected build (it is {digest}, the release says {want}). Nothing was restarted."
                    ),
                );
            }
            images.push(PulledImage {
                pulled: reference,
                digest,
            });
        }

        let mut before = None;
        if let Some(b) = &stack.before {
            at(step::BEFORE);
            let report = match self.docker.compose_run(stack, b) {
                Ok(out) => report(b, out),
                Err(e) => {
                    return docker_failed(
                        &format!(
                            "run the `before` step ({}). Nothing was restarted",
                            b.service
                        ),
                        &e,
                    );
                }
            };
            if !report.ok {
                let code = report
                    .exit_code
                    .map_or("was stopped".to_owned(), |c| format!("exited with {c}"));
                return refuse(
                    502,
                    "before_failed",
                    format!(
                        "The `before` step ({}) {code}, so nothing was restarted. Its last words: {}",
                        b.service,
                        last_line(&report.output)
                    ),
                );
            }
            before = Some(report);
        }

        at(step::TAG);
        if tag != stack.compose_tag {
            // compose.yaml names `<repository>:<compose_tag>`; point that at the pulled build
            // (by digest, so nothing pulled in between can slip in).
            for (repo, img) in stack.repositories.iter().zip(&images) {
                let pinned = format!("{repo}@{}", img.digest);
                let target = format!("{repo}:{}", stack.compose_tag);
                if let Err(e) = self.docker.tag(&pinned, &target) {
                    return docker_failed("tag the pulled image", &e);
                }
            }
        }

        at(step::RECREATE);
        if let Err(e) = self.docker.compose_up(stack, &stack.services) {
            return docker_failed("start the new containers", &e);
        }

        let mut after = None;
        if let Some(a) = &stack.after {
            at(step::AFTER);
            after = Some(match self.docker.compose_run(stack, a) {
                Ok(out) => report(a, out),
                Err(e) => StepReport {
                    service: a.service.clone(),
                    exit_code: None,
                    ok: false,
                    output: e.0,
                },
            });
        }

        let first = images[0].clone();
        ok(&UpdateReply {
            stack: stack.name.clone(),
            pulled: first.pulled,
            digest: first.digest,
            services: stack.services.clone(),
            images: if stack.several_images() {
                images
            } else {
                Vec::new()
            },
            before,
            after,
        })
    }

    fn restart(&self, stack: &Stack) -> Reply {
        if let Err(e) = self.docker.compose_restart(stack, &stack.services) {
            return docker_failed("restart the containers", &e);
        }
        ok(&RestartReply {
            stack: stack.name.clone(),
            services: stack.services.clone(),
        })
    }

    /// Reads one request from `stream`, handles it and writes the reply.
    pub fn serve_connection(&self, stream: &mut (impl Read + Write), peer: &str) {
        let reply = match http::read_message(stream, MAX_BODY, false) {
            Ok(req) => self.handle(&req, peer),
            Err(ReadError::Io(_)) => return,
            Err(ReadError::TooLarge) => {
                self.failed((self.clock)());
                refuse(413, "too_large", "The request is too large.")
            }
            Err(ReadError::Bad(why)) => {
                self.failed((self.clock)());
                refuse(
                    400,
                    "malformed",
                    format!("The request is malformed: {why}."),
                )
            }
        };
        let mut headers = Vec::new();
        if let Some((stack, nonce)) = &reply.sign_for
            && let Some(s) = self.list.stacks.get(stack)
        {
            headers.push((
                H_SIGNATURE,
                protocol::sign(
                    &s.key,
                    &protocol::reply_input(nonce, reply.status, &reply.body),
                ),
            ));
        }
        let _ = http::write_reply(stream, reply.status, &headers, &reply.body);
    }
}

/// Each repository's expected digest, in the stack's order, from either request field.
///
/// A stack with several images must be updated to a version tag (so `compose_tag`, which
/// compose starts, changes only after every image passed) and with a digest for each image.
fn expected_digests(
    stack: &Stack,
    tag: &str,
    one: Option<String>,
    many: Option<BTreeMap<String, String>>,
) -> Result<Vec<Option<String>>, Reply> {
    let bad_digest = || {
        refuse(
            400,
            "bad_digest",
            "An expected digest must be sha256: and 64 hex characters.",
        )
    };
    let wrong_set = || {
        refuse(
            400,
            "bad_digests",
            format!(
                "`expect_digests` must name exactly {}'s images: {}.",
                stack.name,
                stack.repositories.join(", ")
            ),
        )
    };
    if !stack.several_images() {
        let want = match (one, many) {
            (Some(_), Some(_)) => {
                return Err(refuse(
                    400,
                    "bad_request",
                    "Send `expect_digest` or `expect_digests`, not both.",
                ));
            }
            (d, None) => d,
            (None, Some(mut m)) => {
                let d = m.remove(stack.repository()).ok_or_else(wrong_set)?;
                if !m.is_empty() {
                    return Err(wrong_set());
                }
                Some(d)
            }
        };
        if want.as_deref().is_some_and(|d| !protocol::valid_digest(d)) {
            return Err(bad_digest());
        }
        return Ok(vec![want]);
    }
    if one.is_some() {
        return Err(refuse(
            400,
            "bad_request",
            format!(
                "{} updates several images, so it takes `expect_digests` (one per image), not `expect_digest`.",
                stack.name
            ),
        ));
    }
    if tag == stack.compose_tag {
        return Err(refuse(
            403,
            "tag_not_allowed",
            format!(
                "{} updates several images together, so it needs a version tag (x.y.z-main.<time>), not {tag:?}: that way nothing compose starts changes until every image is checked.",
                stack.name
            ),
        ));
    }
    let Some(mut many) = many else {
        return Err(refuse(
            400,
            "digests_required",
            format!(
                "{} updates several images, so the request must give each one's digest in `expect_digests`.",
                stack.name
            ),
        ));
    };
    let mut out = Vec::new();
    for repo in &stack.repositories {
        let d = many.remove(repo).ok_or_else(wrong_set)?;
        if !protocol::valid_digest(&d) {
            return Err(bad_digest());
        }
        out.push(Some(d));
    }
    if !many.is_empty() {
        return Err(wrong_set());
    }
    Ok(out)
}

fn report(step: &crate::allow::Step, out: docker::RunOutput) -> StepReport {
    StepReport {
        service: step.service.clone(),
        ok: out.exit_code == Some(0),
        exit_code: out.exit_code,
        output: out.output,
    }
}

fn last_line(output: &str) -> &str {
    output.lines().last().unwrap_or("(nothing)")
}

fn ok<T: serde::Serialize>(value: &T) -> Reply {
    Reply {
        status: 200,
        body: serde_json::to_vec(value).expect("a reply always serializes"),
        sign_for: None,
    }
}

fn docker_failed(what: &str, e: &docker::DockerError) -> Reply {
    refuse(502, "docker_failed", format!("Docker couldn't {what}. {e}"))
}

/// Unix seconds as `2026-11-08T09:30:00Z`.
pub fn utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

/// Accepts connections on every listen address until the process ends. Each connection gets
/// ten seconds to send its request.
pub fn run<D: Docker + 'static>(helper: Arc<Helper<D>>) -> std::io::Result<()> {
    let active = Arc::new(AtomicUsize::new(0));
    let mut threads = Vec::new();
    for addr in helper.list().listen.clone() {
        let helper = Arc::clone(&helper);
        let active = Arc::clone(&active);
        if let Some(path) = addr.strip_prefix("unix:") {
            #[cfg(unix)]
            {
                let listener = bind_unix(path)?;
                (helper.log)(&format!("listening on unix:{path}"));
                threads.push(std::thread::spawn(move || {
                    for stream in listener.incoming().flatten() {
                        spawn_conn(&helper, &active, stream, "socket".into());
                    }
                }));
            }
            #[cfg(not(unix))]
            {
                let _ = path;
                return Err(std::io::Error::other("unix sockets need Linux or macOS"));
            }
        } else {
            let listener = std::net::TcpListener::bind(&addr)?;
            (helper.log)(&format!("listening on {addr}"));
            threads.push(std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let peer = stream
                        .peer_addr()
                        .map(|a| a.ip().to_string())
                        .unwrap_or_else(|_| "?".into());
                    spawn_conn(&helper, &active, stream, peer);
                }
            }));
        }
    }
    for t in threads {
        let _ = t.join();
    }
    Ok(())
}

#[cfg(unix)]
fn bind_unix(path: &str) -> std::io::Result<std::os::unix::net::UnixListener> {
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if meta.file_type().is_socket() {
            std::fs::remove_file(path)?;
        } else {
            return Err(std::io::Error::other(format!(
                "{path} exists and isn't a socket; not replacing it"
            )));
        }
    }
    let listener = std::os::unix::net::UnixListener::bind(path)?;
    // Owner and group: the stack reaches it through the group that owns the shared folder.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660))?;
    Ok(listener)
}

trait Conn: Read + Write + Send + 'static {
    fn set_timeouts(&self) -> std::io::Result<()>;
}

impl Conn for std::net::TcpStream {
    fn set_timeouts(&self) -> std::io::Result<()> {
        self.set_read_timeout(Some(Duration::from_secs(10)))?;
        self.set_write_timeout(Some(Duration::from_secs(10)))
    }
}

#[cfg(unix)]
impl Conn for std::os::unix::net::UnixStream {
    fn set_timeouts(&self) -> std::io::Result<()> {
        self.set_read_timeout(Some(Duration::from_secs(10)))?;
        self.set_write_timeout(Some(Duration::from_secs(10)))
    }
}

fn spawn_conn<D: Docker + 'static>(
    helper: &Arc<Helper<D>>,
    active: &Arc<AtomicUsize>,
    mut stream: impl Conn,
    peer: String,
) {
    if active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
        active.fetch_sub(1, Ordering::SeqCst);
        let r = refuse(503, "busy", "The helper is busy. Try again in a moment.");
        let _ = stream.set_timeouts();
        let _ = http::write_reply(&mut stream, r.status, &[], &r.body);
        return;
    }
    let helper = Arc::clone(helper);
    let active = Arc::clone(active);
    std::thread::spawn(move || {
        if stream.set_timeouts().is_ok() {
            helper.serve_connection(&mut stream, &peer);
        }
        active.fetch_sub(1, Ordering::SeqCst);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_times() {
        assert_eq!(utc(0), "1970-01-01T00:00:00Z");
        assert_eq!(utc(1_794_130_200), "2026-11-08T09:30:00Z");
        assert_eq!(utc(951_782_400), "2000-02-29T00:00:00Z");
    }
}
