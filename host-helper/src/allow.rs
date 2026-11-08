//! The allow-list: the only stacks, services, images and tags the helper will touch.
//!
//! ```toml
//! listen = ["0.0.0.0:7480"]          # inside the helper's container; nothing is published
//! keys_dir = "/config/keys"          # <stack>.key per stack, mode 600
//!
//! [stack.lockdown]
//! directory = "/mnt/user/appdata/lockdown"     # where its compose.yaml is
//! services = ["lockdown"]
//! image = "ghcr.io/snds/lockdown-server"
//! # compose_tag = "main"           # the tag compose.yaml names (default "main")
//! # tags = ["main"]                # tags it may pull by name (default: just compose_tag)
//! # main_versions = true           # and any x.y.z-main.<time> tag (default true)
//! # project = "lockdown"           # the compose project name, if not the folder's
//! # compose_files = ["compose.yaml", "compose.override.yaml"]   # default: compose's own lookup
//! ```
//!
//! The file holds no secrets: each stack's key is in its own file under `keys_dir`. Every
//! value is checked when the helper starts; a list with anything wrong doesn't start.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Component, Path, PathBuf};

use serde::Deserialize;

use crate::protocol::{self, Key};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FileShape {
    listen: Vec<String>,
    keys_dir: PathBuf,
    #[serde(default)]
    docker: Option<PathBuf>,
    #[serde(default)]
    stack: BTreeMap<String, StackShape>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StackShape {
    directory: PathBuf,
    services: Vec<String>,
    image: String,
    #[serde(default)]
    compose_tag: Option<String>,
    #[serde(default)]
    tags: Option<Vec<String>>,
    #[serde(default)]
    main_versions: Option<bool>,
    #[serde(default)]
    project: Option<String>,
    #[serde(default)]
    compose_files: Option<Vec<String>>,
}

/// The checked allow-list.
#[derive(Debug)]
pub struct AllowList {
    /// `ip:port` addresses, or `unix:/path` sockets.
    pub listen: Vec<String>,
    /// The `docker` program (default `docker`, found on `PATH`).
    pub docker: PathBuf,
    pub stacks: BTreeMap<String, Stack>,
}

/// One managed stack.
#[derive(Debug)]
pub struct Stack {
    pub name: String,
    /// The compose project's folder: compose runs there, as it would for Sean.
    pub directory: PathBuf,
    /// The compose files, when the list names them; otherwise compose finds them itself
    /// (`compose.yaml` and `compose.override.yaml`), as `docker compose` in that folder would.
    pub compose_files: Vec<String>,
    /// The project name, when the list names one.
    pub project: Option<String>,
    pub services: Vec<String>,
    /// The repository it may pull (`ghcr.io/snds/lockdown-server`), without a tag.
    pub repository: String,
    /// The tag `compose.yaml` names (`main`). A pulled version is tagged as this locally, so
    /// compose starts it.
    pub compose_tag: String,
    pub tags: Vec<String>,
    pub main_versions: bool,
    pub key: Key,
}

impl Stack {
    /// Whether `tag` may be pulled for this stack.
    pub fn allows_tag(&self, tag: &str) -> bool {
        protocol::valid_tag(tag)
            && (self.tags.iter().any(|t| t == tag)
                || (self.main_versions && protocol::is_main_version(tag)))
    }
}

/// Why the allow-list can't be used, in plain words.
#[derive(Debug, PartialEq, Eq)]
pub struct ListError(pub String);

impl fmt::Display for ListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ListError {}

fn err<T>(msg: impl Into<String>) -> Result<T, ListError> {
    Err(ListError(msg.into()))
}

impl AllowList {
    /// Reads and checks the list at `path`, and each stack's key.
    pub fn load(path: &Path) -> Result<Self, ListError> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            ListError(format!(
                "The allow-list {} can't be read: {e}.",
                path.display()
            ))
        })?;
        Self::parse(&text, read_key)
    }

    /// Checks `text`, reading each stack's key with `key_for(stack, key file path)`.
    pub fn parse(
        text: &str,
        key_for: impl Fn(&str, &Path) -> Result<Key, ListError>,
    ) -> Result<Self, ListError> {
        let shape: FileShape = toml::from_str(text)
            .map_err(|e| ListError(format!("The allow-list isn't valid: {e}")))?;
        if shape.listen.is_empty() {
            return err("The allow-list needs at least one `listen` address.");
        }
        for addr in &shape.listen {
            check_listen(addr)?;
        }
        if !absolute_and_plain(&shape.keys_dir) {
            return err("`keys_dir` must be an absolute path without `..`.");
        }
        let docker = shape.docker.unwrap_or_else(|| PathBuf::from("docker"));
        if shape.stack.is_empty() {
            return err("The allow-list names no stacks, so the helper would refuse everything.");
        }
        let mut stacks = BTreeMap::new();
        let mut keys_seen: Vec<String> = Vec::new();
        for (name, s) in shape.stack {
            let at = |what: &str| format!("Stack {name:?}: {what}");
            if !protocol::valid_name(&name) {
                return err(at("its name must be lower-case letters, digits, - and _."));
            }
            if !absolute_and_plain(&s.directory) {
                return err(at("`directory` must be an absolute path without `..`."));
            }
            if s.services.is_empty() {
                return err(at("`services` names no services."));
            }
            for svc in &s.services {
                if !protocol::valid_name(svc) {
                    return err(at(&format!("the service name {svc:?} isn't allowed.")));
                }
            }
            if !protocol::valid_repository(&s.image) {
                return err(at(
                    "`image` must be a repository on a registry, without a tag, like ghcr.io/snds/lockdown-server.",
                ));
            }
            let compose_tag = s.compose_tag.unwrap_or_else(|| "main".into());
            if !protocol::valid_tag(&compose_tag) {
                return err(at("`compose_tag` isn't a valid tag."));
            }
            let tags = s.tags.unwrap_or_else(|| vec![compose_tag.clone()]);
            for tag in &tags {
                if !protocol::valid_tag(tag) {
                    return err(at(&format!("the tag {tag:?} isn't valid.")));
                }
            }
            if let Some(p) = &s.project
                && !protocol::valid_name(p)
            {
                return err(at("`project` must be lower-case letters, digits, - and _."));
            }
            let compose_files = s.compose_files.unwrap_or_default();
            for f in &compose_files {
                let plain = !f.is_empty()
                    && !f.starts_with(['-', '.'])
                    && f.bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
                if !plain {
                    return err(at(&format!(
                        "the compose file {f:?} must be a plain file name in `directory`."
                    )));
                }
            }
            let key = key_for(&name, &shape.keys_dir.join(format!("{name}.key")))?;
            let hex = key.to_hex();
            if keys_seen.contains(&hex) {
                return err(at(
                    "it shares its key with another stack. Each stack needs its own (keygen).",
                ));
            }
            keys_seen.push(hex);
            stacks.insert(
                name.clone(),
                Stack {
                    name,
                    directory: s.directory,
                    compose_files,
                    project: s.project,
                    services: s.services,
                    repository: s.image,
                    compose_tag,
                    tags,
                    main_versions: s.main_versions.unwrap_or(true),
                    key,
                },
            );
        }
        Ok(AllowList {
            listen: shape.listen,
            docker,
            stacks,
        })
    }
}

fn check_listen(addr: &str) -> Result<(), ListError> {
    if let Some(path) = addr.strip_prefix("unix:") {
        if !absolute_and_plain(Path::new(path)) {
            return err(format!("The socket {addr:?} must be an absolute path."));
        }
        return Ok(());
    }
    if addr.parse::<std::net::SocketAddr>().is_err() {
        return err(format!(
            "The listen address {addr:?} must be an IP address and port (0.0.0.0:7480 inside the helper's container) or unix:/path."
        ));
    }
    Ok(())
}

fn absolute_and_plain(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| !matches!(c, Component::ParentDir))
}

/// Reads one stack's key file. On Linux and macOS the file must be readable by its owner only.
pub fn read_key(stack: &str, path: &Path) -> Result<Key, ListError> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        ListError(format!(
            "Stack {stack:?} has no key at {} ({e}). Make one with `app-updates-host-helper keygen --stack {stack}`.",
            path.display()
        ))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)
            .map_err(|e| ListError(format!("{}: {e}", path.display())))?
            .permissions()
            .mode();
        if mode & 0o077 != 0 {
            return err(format!(
                "The key {} can be read by others (mode {:o}). Run: chmod 600 {}",
                path.display(),
                mode & 0o777,
                path.display()
            ));
        }
    }
    Key::parse(&text).map_err(|_| {
        ListError(format!(
            "The key {} isn't 64 hex characters.",
            path.display()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"
listen = ["0.0.0.0:7480", "unix:/run/host-helper/helper.sock"]
keys_dir = "/config/keys"

[stack.lockdown]
directory = "/mnt/user/appdata/lockdown"
services = ["lockdown"]
image = "ghcr.io/snds/lockdown-server"

[stack.vale]
directory = "/mnt/user/appdata/vale/apps/gateway"
services = ["gateway"]
image = "ghcr.io/snds/vale-gateway"
tags = ["main", "stable"]
main_versions = false
project = "gateway"
compose_files = ["compose.yaml", "compose.override.yaml"]
"#;

    fn keys(stack: &str, path: &Path) -> Result<Key, ListError> {
        assert!(path.ends_with(format!("{stack}.key")));
        let byte = stack.len() as u8;
        Key::parse(&format!("{byte:02x}").repeat(32)).map_err(|e| ListError(e.to_string()))
    }

    fn parse(text: &str) -> Result<AllowList, ListError> {
        AllowList::parse(text, keys)
    }

    #[test]
    fn a_good_list() {
        let list = parse(GOOD).unwrap();
        let l = &list.stacks["lockdown"];
        assert_eq!(l.compose_tag, "main");
        assert_eq!(l.tags, ["main"]);
        assert!(l.main_versions);
        assert!(l.compose_files.is_empty());
        assert!(l.allows_tag("main"));
        assert!(l.allows_tag("0.1.0-main.20261108093000"));
        assert!(!l.allows_tag("latest"));
        assert!(!l.allows_tag("0.1.0"));
        let v = &list.stacks["vale"];
        assert!(v.allows_tag("stable"));
        assert!(!v.allows_tag("0.1.0-main.20261108093000"));
        assert_eq!(v.project.as_deref(), Some("gateway"));
        assert_eq!(list.docker, PathBuf::from("docker"));
    }

    #[test]
    fn anything_odd_stops_it_starting() {
        let listen = "listen = [\"0.0.0.0:7480\", \"unix:/run/host-helper/helper.sock\"]";
        let image = "image = \"ghcr.io/snds/lockdown-server\"";
        let services = "services = [\"lockdown\"]";
        let dir = "directory = \"/mnt/user/appdata/lockdown\"";
        let cases: &[(&str, &str, &str)] = &[
            (
                image,
                "image = \"ghcr.io/snds/lockdown-server:main\"",
                "without a tag",
            ),
            (image, "image = \"lockdown-server\"", "without a tag"),
            (services, "services = []", "names no services"),
            (services, "services = [\"--all\"]", "isn't allowed"),
            (dir, "directory = \"appdata/lockdown\"", "absolute"),
            (dir, "directory = \"/mnt/user/../../etc\"", "absolute"),
            ("[stack.lockdown]", "[stack.\"Lock Down\"]", "its name"),
            (listen, "listen = []", "at least one"),
            (listen, "listen = [\"helper:7480\"]", "IP address"),
            ("project = \"gateway\"", "project = \"-p x\"", "`project`"),
            (
                "compose_files = [\"compose.yaml\", \"compose.override.yaml\"]",
                "compose_files = [\"../x.yaml\"]",
                "plain file name",
            ),
            (
                "main_versions = false",
                "main_versions = false\ncommand = \"rm -rf /\"",
                "unknown field",
            ),
            (
                "keys_dir = \"/config/keys\"",
                "keys_dir = \"keys\"",
                "keys_dir",
            ),
        ];
        for (from, to, says) in cases {
            assert!(GOOD.contains(from), "{from}");
            let e = parse(&GOOD.replacen(from, to, 1)).unwrap_err();
            assert!(e.0.contains(says), "{to}: {}", e.0);
        }
        let none = "listen = [\"0.0.0.0:7480\"]\nkeys_dir = \"/k\"\n";
        assert!(parse(none).unwrap_err().0.contains("no stacks"));
    }

    #[test]
    fn the_sample_list_is_good() {
        let list = parse(include_str!("../deploy/allow-list.example.toml")).unwrap();
        assert_eq!(list.listen, ["0.0.0.0:7480"]);
        assert_eq!(list.stacks["lockdown"].services, ["lockdown"]);
        assert_eq!(
            list.stacks["vale-gateway"].repository,
            "ghcr.io/snds/vale-gateway"
        );
    }

    #[test]
    fn two_stacks_never_share_a_key() {
        let same = |_: &str, _: &Path| Ok(Key::parse(&"ab".repeat(32)).unwrap());
        let e = AllowList::parse(GOOD, same).unwrap_err();
        assert!(e.0.contains("shares its key"));
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_must_be_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("host-helper-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("lockdown.key");
        std::fs::write(&path, Key::generate().unwrap().to_hex()).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            read_key("lockdown", &path)
                .unwrap_err()
                .0
                .contains("chmod 600")
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(read_key("lockdown", &path).is_ok());
        assert!(
            read_key("vale", &dir.join("vale.key"))
                .unwrap_err()
                .0
                .contains("keygen --stack vale")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
