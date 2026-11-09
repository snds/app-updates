//! What the helper asks Docker to do, behind a trait so the tests run without Docker.
//!
//! [`Cli`] runs the `docker` program directly (never a shell), with an argument list built only
//! from allow-list values and checked tags, and a clean environment: the helper's own variables
//! never reach compose's interpolation.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::allow::{Stack, Step};

/// How long one `docker` call may run before it is stopped.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15 * 60);

/// One container of an allowed service.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Container {
    pub state: String,
    /// The image as the container names it (`ghcr.io/snds/lockdown-server:main`).
    pub config_image: String,
    /// The image's local ID (`sha256:…`).
    pub image_id: String,
}

/// An image's registry digests and version label.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ImageInfo {
    /// `repository@sha256:…` entries.
    pub repo_digests: Vec<String>,
    /// `org.opencontainers.image.version`, if set.
    pub version: Option<String>,
}

/// How a one-shot command ended: its exit status and the tail of its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunOutput {
    /// None when it was stopped by a signal.
    pub exit_code: Option<i32>,
    /// The last [`OUTPUT_LINES`] lines of standard output and error, at most [`OUTPUT_BYTES`].
    pub output: String,
}

/// How much of a step's output is kept.
pub const OUTPUT_LINES: usize = 20;
pub const OUTPUT_BYTES: usize = 4096;

/// A failed Docker call: what was being done, and Docker's last words (no secrets: Docker's
/// output never holds the helper's keys).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DockerError(pub String);

impl std::fmt::Display for DockerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Every Docker action the helper takes. Nothing else is possible.
pub trait Docker: Send + Sync {
    /// `docker pull <reference>`.
    fn pull(&self, reference: &str) -> Result<(), DockerError>;
    /// The image's digests and version label.
    fn image(&self, reference: &str) -> Result<ImageInfo, DockerError>;
    /// `docker tag <from> <to>`.
    fn tag(&self, from: &str, to: &str) -> Result<(), DockerError>;
    /// `docker compose up -d --no-deps --no-build --pull never <services>` in the stack's folder.
    fn compose_up(&self, stack: &Stack, services: &[String]) -> Result<(), DockerError>;
    /// `docker compose restart <services>` in the stack's folder.
    fn compose_restart(&self, stack: &Stack, services: &[String]) -> Result<(), DockerError>;
    /// `docker compose run --rm -T [--no-deps] -- <service> <command…>` in the stack's folder.
    /// A command that runs and fails is `Ok` with its exit status; `Err` means Docker itself
    /// couldn't run it.
    fn compose_run(&self, stack: &Stack, step: &Step) -> Result<RunOutput, DockerError>;
    /// The containers of one service.
    fn containers(&self, stack: &Stack, service: &str) -> Result<Vec<Container>, DockerError>;
}

/// The real thing: the `docker` program and its compose plugin.
pub struct Cli {
    pub program: PathBuf,
}

/// The arguments before a compose subcommand: the project name and files, if the list names
/// them. Compose runs in the stack's folder, so without them it finds the files and the name
/// just as `docker compose` there would.
pub fn compose_args(stack: &Stack) -> Vec<String> {
    let mut args = vec!["compose".to_owned()];
    if let Some(p) = &stack.project {
        args.push("--project-name".into());
        args.push(p.clone());
    }
    for f in &stack.compose_files {
        args.push("--file".into());
        args.push(f.clone());
    }
    args
}

/// `compose … up -d --no-deps --no-build --pull never -- <services>`.
pub fn up_args(stack: &Stack, services: &[String]) -> Vec<String> {
    let mut args = compose_args(stack);
    args.extend(
        [
            "up",
            "--detach",
            "--no-deps",
            "--no-build",
            "--pull",
            "never",
            "--",
        ]
        .map(str::to_owned),
    );
    args.extend(services.iter().cloned());
    args
}

/// `compose … restart -- <services>`.
pub fn restart_args(stack: &Stack, services: &[String]) -> Vec<String> {
    let mut args = compose_args(stack);
    args.extend(["restart", "--"].map(str::to_owned));
    args.extend(services.iter().cloned());
    args
}

/// `compose … run --rm --no-TTY --pull never [--no-deps] -- <service> <command…>`.
pub fn run_args(stack: &Stack, step: &Step) -> Vec<String> {
    let mut args = compose_args(stack);
    args.extend(["run", "--rm", "--no-TTY", "--pull", "never"].map(str::to_owned));
    if step.no_deps {
        args.push("--no-deps".into());
    }
    args.push("--".into());
    args.push(step.service.clone());
    args.extend(step.command.iter().cloned());
    args
}

/// The last lines of `stdout` and `stderr` together, trimmed to the limits.
pub fn tail(stdout: &str, stderr: &str) -> String {
    let lines: Vec<&str> = stdout
        .lines()
        .chain(stderr.lines())
        .filter(|l| !l.trim().is_empty())
        .collect();
    let mut out = lines[lines.len().saturating_sub(OUTPUT_LINES)..].join("\n");
    if out.len() > OUTPUT_BYTES {
        let mut cut = out.len() - OUTPUT_BYTES;
        while !out.is_char_boundary(cut) {
            cut += 1;
        }
        out = out[cut..].to_owned();
    }
    out
}

struct Exit {
    code: Option<i32>,
    success: bool,
    stdout: String,
    stderr: String,
}

impl Cli {
    fn run(&self, args: &[String], dir: Option<&PathBuf>) -> Result<String, DockerError> {
        let what = args.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
        let exit = self.exec(args, dir)?;
        if exit.success {
            Ok(exit.stdout)
        } else {
            let tail: Vec<&str> = exit
                .stderr
                .lines()
                .filter(|l| !l.trim().is_empty())
                .collect();
            let tail = tail[tail.len().saturating_sub(3)..].join(" / ");
            Err(DockerError(format!("`docker {what}` failed: {tail}")))
        }
    }

    fn exec(&self, args: &[String], dir: Option<&PathBuf>) -> Result<Exit, DockerError> {
        let mut cmd = Command::new(&self.program);
        cmd.args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for name in ["PATH", "HOME", "DOCKER_HOST", "DOCKER_CONFIG"] {
            if let Some(v) = std::env::var_os(name) {
                cmd.env(name, v);
            }
        }
        if let Some(dir) = dir {
            cmd.current_dir(dir);
        }
        let what = args.iter().take(3).cloned().collect::<Vec<_>>().join(" ");
        let mut child = cmd
            .spawn()
            .map_err(|e| DockerError(format!("Docker couldn't be started ({e}).")))?;
        let mut out = child.stdout.take().expect("piped");
        let mut errs = child.stderr.take().expect("piped");
        let out_reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = (&mut out).take(1 << 20).read_to_end(&mut s);
            s
        });
        let err_reader = std::thread::spawn(move || {
            let mut s = Vec::new();
            let _ = (&mut errs).take(1 << 20).read_to_end(&mut s);
            s
        });
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > CALL_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(DockerError(format!(
                        "`docker {what}` took longer than {} minutes and was stopped.",
                        CALL_TIMEOUT.as_secs() / 60
                    )));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(DockerError(format!("Docker stopped answering ({e})."))),
            }
        };
        let stdout = String::from_utf8_lossy(&out_reader.join().unwrap_or_default()).into_owned();
        let stderr = String::from_utf8_lossy(&err_reader.join().unwrap_or_default()).into_owned();
        Ok(Exit {
            code: status.code(),
            success: status.success(),
            stdout,
            stderr,
        })
    }
}

impl Docker for Cli {
    fn pull(&self, reference: &str) -> Result<(), DockerError> {
        self.run(&["pull".into(), "--quiet".into(), reference.into()], None)
            .map(drop)
    }

    fn image(&self, reference: &str) -> Result<ImageInfo, DockerError> {
        let out = self.run(
            &[
                "image".into(),
                "inspect".into(),
                "--format".into(),
                "{{json .RepoDigests}}\t{{json .Config.Labels}}".into(),
                reference.into(),
            ],
            None,
        )?;
        parse_image(&out)
    }

    fn tag(&self, from: &str, to: &str) -> Result<(), DockerError> {
        self.run(&["tag".into(), from.into(), to.into()], None)
            .map(drop)
    }

    fn compose_up(&self, stack: &Stack, services: &[String]) -> Result<(), DockerError> {
        self.run(&up_args(stack, services), Some(&stack.directory))
            .map(drop)
    }

    fn compose_restart(&self, stack: &Stack, services: &[String]) -> Result<(), DockerError> {
        self.run(&restart_args(stack, services), Some(&stack.directory))
            .map(drop)
    }

    fn compose_run(&self, stack: &Stack, step: &Step) -> Result<RunOutput, DockerError> {
        let exit = self.exec(&run_args(stack, step), Some(&stack.directory))?;
        Ok(RunOutput {
            exit_code: exit.code,
            output: tail(&exit.stdout, &exit.stderr),
        })
    }

    fn containers(&self, stack: &Stack, service: &str) -> Result<Vec<Container>, DockerError> {
        let mut args = compose_args(stack);
        args.extend(["ps", "--all", "--quiet", "--", service].map(str::to_owned));
        let ids = self.run(&args, Some(&stack.directory))?;
        let ids: Vec<String> = ids
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && l.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
            .collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args: Vec<String> = [
            "inspect",
            "--type",
            "container",
            "--format",
            "{{.State.Status}}\t{{.Config.Image}}\t{{.Image}}",
        ]
        .map(str::to_owned)
        .into();
        args.extend(ids);
        parse_containers(&self.run(&args, None)?)
    }
}

/// Reads `docker inspect`'s `state<TAB>image<TAB>image id` lines.
pub fn parse_containers(out: &str) -> Result<Vec<Container>, DockerError> {
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let mut parts = line.split('\t');
            match (parts.next(), parts.next(), parts.next(), parts.next()) {
                (Some(state), Some(image), Some(id), None) => Ok(Container {
                    state: state.trim().to_owned(),
                    config_image: image.trim().to_owned(),
                    image_id: id.trim().to_owned(),
                }),
                _ => Err(DockerError(
                    "Docker's container list couldn't be read.".into(),
                )),
            }
        })
        .collect()
}

/// Reads `docker image inspect`'s `<digests JSON><TAB><labels JSON>` line.
pub fn parse_image(out: &str) -> Result<ImageInfo, DockerError> {
    let bad = || DockerError("Docker's image details couldn't be read.".into());
    let line = out.lines().find(|l| !l.trim().is_empty()).ok_or_else(bad)?;
    let (digests, labels) = line.split_once('\t').ok_or_else(bad)?;
    let repo_digests: Option<Vec<String>> = serde_json::from_str(digests).map_err(|_| bad())?;
    let labels: Option<std::collections::BTreeMap<String, String>> =
        serde_json::from_str(labels).map_err(|_| bad())?;
    Ok(ImageInfo {
        repo_digests: repo_digests.unwrap_or_default(),
        version: labels.and_then(|mut l| l.remove("org.opencontainers.image.version")),
    })
}

/// The digest of `repository` among an image's `repository@sha256:…` entries.
pub fn digest_for(info: &ImageInfo, repository: &str) -> Option<String> {
    info.repo_digests.iter().find_map(|d| {
        let (repo, digest) = d.split_once('@')?;
        (repo == repository && crate::protocol::valid_digest(digest)).then(|| digest.to_owned())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Key;

    fn stack(project: Option<&str>, files: &[&str]) -> Stack {
        Stack {
            name: "lockdown".into(),
            directory: "/mnt/user/appdata/lockdown".into(),
            compose_files: files.iter().map(|s| s.to_string()).collect(),
            project: project.map(String::from),
            services: vec!["lockdown".into()],
            repositories: vec!["ghcr.io/snds/lockdown-server".into()],
            compose_tag: "main".into(),
            tags: vec!["main".into()],
            main_versions: true,
            before: None,
            after: None,
            key: Key::parse(&"11".repeat(32)).unwrap(),
        }
    }

    #[test]
    fn compose_arguments() {
        let s = stack(None, &[]);
        assert_eq!(
            up_args(&s, &s.services),
            [
                "compose",
                "up",
                "--detach",
                "--no-deps",
                "--no-build",
                "--pull",
                "never",
                "--",
                "lockdown"
            ]
        );
        let s = stack(Some("gateway"), &["compose.yaml", "compose.override.yaml"]);
        assert_eq!(
            restart_args(&s, &s.services),
            [
                "compose",
                "--project-name",
                "gateway",
                "--file",
                "compose.yaml",
                "--file",
                "compose.override.yaml",
                "restart",
                "--",
                "lockdown"
            ]
        );
    }

    #[test]
    fn run_arguments_and_output() {
        let s = stack(None, &[]);
        let step = Step {
            service: "backup".into(),
            command: vec!["sh".into(), "/backup/backup.sh".into(), "once".into()],
            no_deps: false,
        };
        assert_eq!(
            run_args(&s, &step),
            [
                "compose",
                "run",
                "--rm",
                "--no-TTY",
                "--pull",
                "never",
                "--",
                "backup",
                "sh",
                "/backup/backup.sh",
                "once"
            ]
        );
        let step = Step {
            no_deps: true,
            ..step
        };
        assert!(run_args(&s, &step).contains(&"--no-deps".to_owned()));
        let many: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        let t = tail(&many, "boom\n");
        assert_eq!(t.lines().count(), OUTPUT_LINES);
        assert!(t.starts_with("line 12") && t.ends_with("boom"), "{t}");
        let long = "é".repeat(5000);
        let t = tail(&long, "");
        assert!(t.len() <= OUTPUT_BYTES && t.chars().all(|c| c == 'é'));
    }

    #[test]
    fn reading_dockers_answers() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let info = parse_image(&format!(
            "[\"ghcr.io/snds/other@{digest}\",\"ghcr.io/snds/lockdown-server@{digest}\"]\t{{\"org.opencontainers.image.version\":\"0.1.0-main.20261108093000\"}}\n"
        ))
        .unwrap();
        assert_eq!(info.version.as_deref(), Some("0.1.0-main.20261108093000"));
        assert_eq!(
            digest_for(&info, "ghcr.io/snds/lockdown-server"),
            Some(digest.clone())
        );
        assert_eq!(digest_for(&info, "ghcr.io/snds/lockdown"), None);
        let local = parse_image("[]\tnull\n").unwrap();
        assert_eq!(local, ImageInfo::default());
        assert!(parse_image("garbage").is_err());

        let cs = parse_containers(
            "running\tghcr.io/snds/lockdown-server:main\tsha256:abc\nexited\tx:main\tsha256:def\n",
        )
        .unwrap();
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].state, "running");
        assert_eq!(cs[1].image_id, "sha256:def");
        assert!(parse_containers("running only").is_err());
    }

    /// The real runner, against a stand-in `docker` program that prints its arguments, its
    /// folder and whether the helper's own environment leaked in.
    #[cfg(unix)]
    #[test]
    fn the_cli_runs_docker_without_a_shell_or_the_helpers_environment() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("host-helper-cli-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let fake = dir.join("docker");
        std::fs::write(
            &fake,
            "#!/bin/sh\nfor a in \"$@\"; do printf '[%s]' \"$a\" >> \"$0.log\"; done\nprintf ' pwd=%s secret=%s\\n' \"$(pwd)\" \"${HELPER_SECRET:-none}\" >> \"$0.log\"\n[ \"$1\" = fail ] && { echo 'no such image' >&2; exit 1; }\nexit 0\n",
        )
        .unwrap();
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
        // SAFETY: this test is the only one that reads HELPER_SECRET.
        unsafe { std::env::set_var("HELPER_SECRET", "leaked") };
        let cli = Cli {
            program: fake.clone(),
        };
        let mut s = stack(None, &[]);
        s.directory = dir.clone();
        cli.compose_up(&s, &s.services).unwrap();
        cli.pull("ghcr.io/snds/lockdown-server:main; rm -rf /")
            .unwrap();
        let e = cli.run(&["fail".into()], None).unwrap_err();
        assert!(e.0.contains("no such image"), "{}", e.0);
        let log = std::fs::read_to_string(dir.join("docker.log")).unwrap();
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(
            lines[0],
            format!(
                "[compose][up][--detach][--no-deps][--no-build][--pull][never][--][lockdown] pwd={} secret=none",
                dir.display()
            )
        );
        // One argument, passed as is: nothing interprets it.
        assert!(
            lines[1].starts_with("[pull][--quiet][ghcr.io/snds/lockdown-server:main; rm -rf /] ")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
