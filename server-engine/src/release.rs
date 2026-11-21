//! A server app's release, read from its publish channel and checked before anything is pulled
//! (LS18).
//!
//! The channel is the one the desktop apps use (the unified updates plan, U1 to U3): a `latest.json`
//! on GitHub Releases, signed with minisign by the app's own key, in the form Tauri's updater
//! reads. A server app's `latest.json` has one entry in `platforms` per service (container), named
//! `server-<service>`. Its `url` is the service's image, pinned by digest. Its `signature` is a
//! minisign signature, base64'd as `tauri signer sign` writes it, over the release statement
//! ([`statement`]): the app, the version, the service and the image. So a signature can't be
//! moved to another app, another version or another image.

use std::time::Duration;

use base64::Engine;
use semver::Version;
use serde::Deserialize;

/// The hosts a GitHub release download comes from: the same list as the desktop kit's
/// (`app_updates::GITHUB_HOSTS`). Every redirect is checked against it.
pub const GITHUB_HOSTS: [&str; 4] = [
    "github.com",
    "objects.githubusercontent.com",
    "release-assets.githubusercontent.com",
    "github-releases.githubusercontent.com",
];

/// The prefix of a server service's key in `platforms`.
pub const SERVICE_PREFIX: &str = "server-";

/// The largest `latest.json` read.
const MANIFEST_MAX: u64 = 256 * 1024;

/// The bytes a release's signature covers for one service. CI writes exactly these to a file and
/// signs it with `tauri signer sign` (`service-app statement` prints them).
pub fn statement(app: &str, version: &str, service: &str, image: &str) -> String {
    format!("sy-server-release/1\napp={app}\nversion={version}\nservice={service}\nimage={image}\n")
}

/// The bytes the release's own signature covers: whether it needs a step by hand, and since which
/// version one has been needed. Signed so that nobody can turn a by-hand release into an automatic
/// one. CI signs it the same way as a service's statement (`service-app meta-statement`).
pub fn meta_statement(app: &str, version: &str, by_hand: bool, by_hand_at: &str) -> String {
    format!(
        "sy-server-release-meta/1\napp={app}\nversion={version}\nby_hand={}\nby_hand_at={by_hand_at}\n",
        u8::from(by_hand)
    )
}

/// One `platforms` entry, as Tauri's `latest.json` has it.
#[derive(Debug, Clone, Deserialize)]
pub struct Platform {
    pub url: String,
    pub signature: String,
}

/// A channel's `latest.json`, as Tauri's updater reads it.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    pub version: String,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub pub_date: Option<String>,
    pub platforms: std::collections::BTreeMap<String, Platform>,
    /// Whether this release needs a step by hand on the server. Read only when `signature`
    /// verifies over [`meta_statement`].
    #[serde(default, rename = "byHand")]
    pub by_hand: Option<bool>,
    /// The newest version that needed a step by hand.
    #[serde(default, rename = "byHandAt")]
    pub by_hand_at: Option<String>,
    /// The signature over [`meta_statement`].
    #[serde(default)]
    pub signature: Option<String>,
}

/// A release that passed every check: each service's image, pinned by digest and signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    /// Service name to its image (`registry/name@sha256:<hex>`).
    pub images: std::collections::BTreeMap<String, String>,
    /// From the signed meta statement: `Some(false)` may install by itself, `Some(true)` needs a
    /// step by hand, `None` (no meta signature) may be accepted by hand but never automatically.
    pub by_hand: Option<bool>,
    pub by_hand_at: Option<String>,
    pub notes: Option<String>,
}

/// Whether `key` is a minisign public key in the form the updater reads: base64 of the two-line
/// text `tauri signer generate` writes. The same test as the desktop kit's `key_is_valid`.
pub fn key_ok(key: &str) -> bool {
    public_key(key).is_ok()
}

fn public_key(key: &str) -> Result<minisign_verify::PublicKey, String> {
    let text = base64::engine::general_purpose::STANDARD
        .decode(key.trim().as_bytes())
        .map_err(|_| "the app's key isn't base64")?;
    let text = String::from_utf8(text).map_err(|_| "the app's key isn't text")?;
    minisign_verify::PublicKey::decode(&text)
        .map_err(|_| "the app's key isn't a minisign key".into())
}

/// Checks `signature` (base64 of a minisign signature file, as `latest.json` carries it) over
/// `data` with `key`. Tauri's updater accepts minisign's pre-hashed and legacy signatures, and so
/// does this.
pub fn verify(key: &str, data: &[u8], signature: &str) -> Result<(), String> {
    let pk = public_key(key)?;
    let text = base64::engine::general_purpose::STANDARD
        .decode(signature.trim().as_bytes())
        .map_err(|_| "a signature isn't base64".to_owned())?;
    let text = String::from_utf8(text).map_err(|_| "a signature isn't text".to_owned())?;
    let sig = minisign_verify::Signature::decode(&text)
        .map_err(|_| "a signature isn't a minisign signature".to_owned())?;
    pk.verify(data, &sig, true)
        .map_err(|_| "a signature doesn't verify with the app's key".to_owned())
}

/// Checks that `image` is pinned by digest: `registry/name@sha256:<64 hex>`, with no tag. A tag
/// (`latest` above all) can move after it was signed; a digest can't.
pub fn image_ok(image: &str) -> Result<(), String> {
    let (name, digest) = image
        .split_once('@')
        .ok_or_else(|| format!("{image}: not pinned by digest"))?;
    let hex = digest
        .strip_prefix("sha256:")
        .ok_or_else(|| format!("{image}: not a sha256 digest"))?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(format!("{image}: not a sha256 digest"));
    }
    let last = name.rsplit('/').next().unwrap_or(name);
    if last.contains(':') {
        return Err(format!("{image}: carries a tag as well as its digest"));
    }
    let fine = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "._-/:".contains(c);
    if name.is_empty() || !name.chars().all(fine) || name.starts_with(['/', '-', '.']) {
        return Err(format!("{image}: not an image name"));
    }
    Ok(())
}

/// Reads `manifest` for `app` and checks it all: a version, every service it names signed by
/// `key` over [`statement`], every image pinned by digest. Any failure refuses the whole release.
pub fn check(app: &str, key: &str, manifest: &Manifest) -> Result<Release, String> {
    let version = Version::parse(manifest.version.trim_start_matches('v'))
        .map_err(|_| format!("{}: not a version", manifest.version))?;
    let mut images = std::collections::BTreeMap::new();
    for (k, p) in &manifest.platforms {
        let Some(service) = k.strip_prefix(SERVICE_PREFIX) else {
            continue;
        };
        if service.is_empty()
            || !service
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        {
            return Err(format!("{k}: not a service name"));
        }
        image_ok(&p.url)?;
        let text = statement(app, &version.to_string(), service, &p.url);
        verify(key, text.as_bytes(), &p.signature).map_err(|e| format!("{service}: {e}"))?;
        images.insert(service.to_owned(), p.url.clone());
    }
    if images.is_empty() {
        return Err("the release names no server service".into());
    }
    // The flags count only when signed. A meta signature that is there but doesn't verify means the
    // release was changed after it was signed: refuse all of it.
    let (by_hand, by_hand_at) = match manifest.signature.as_deref().map(str::trim) {
        None | Some("") => (None, None),
        Some(sig) => {
            let flag = manifest
                .by_hand
                .ok_or("the release is signed but doesn't say whether it needs a step by hand")?;
            let at = manifest.by_hand_at.clone().unwrap_or_default();
            let text = meta_statement(app, &version.to_string(), flag, &at);
            verify(key, text.as_bytes(), sig).map_err(|e| format!("the release's own {e}"))?;
            (Some(flag), Some(at).filter(|a| !a.is_empty()))
        }
    };
    Ok(Release {
        version,
        images,
        by_hand,
        by_hand_at,
        notes: manifest.notes.clone().filter(|n| !n.trim().is_empty()),
    })
}

/// Checks that `url` is somewhere a release may come from: `https` on one of `hosts`, on its usual
/// port, with no user name or password. With `loopback` (tests only), plain `http` on this
/// machine's loopback address too. The desktop kit's `allowed`, for the server.
pub fn allowed(url: &str, hosts: &[String], loopback: bool) -> Result<(), String> {
    let (scheme, rest) = url
        .split_once("://")
        .ok_or("the update address isn't a web address")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Err("the update address carries a user name".into());
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (h, after) = v6.split_once(']').ok_or("the update address has no host")?;
            (format!("[{h}]"), after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), Some(p)),
            None => (authority.to_owned(), None),
        },
    };
    let host = host.to_ascii_lowercase();
    if host.is_empty() {
        return Err("the update address has no host".into());
    }
    let is_loopback = matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]");
    match scheme {
        "http" if loopback && is_loopback => return Ok(()),
        "https" => {}
        _ => return Err("updates come only over https".into()),
    }
    if !hosts.iter().any(|h| h.eq_ignore_ascii_case(&host)) {
        return Err(format!("updates don't come from {host}"));
    }
    if port.is_some_and(|p| p != "443") {
        return Err(format!("updates from {host} use its usual port"));
    }
    Ok(())
}

/// Fetches a channel's `latest.json`, checking the address and every redirect against `hosts`.
pub fn fetch(endpoint: &str, hosts: &[String], loopback: bool) -> Result<Manifest, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut url = endpoint.to_owned();
    for _ in 0..5 {
        allowed(&url, hosts, loopback)?;
        let mut resp = agent
            .get(&url)
            .call()
            .map_err(|e| format!("the update channel: {e}"))?;
        let status = resp.status().as_u16();
        if (300..400).contains(&status) {
            url = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .ok_or("the update channel redirected nowhere")?
                .to_owned();
            continue;
        }
        if status != 200 {
            return Err(format!("the update channel answered {status}"));
        }
        let body = resp
            .body_mut()
            .with_config()
            .limit(MANIFEST_MAX)
            .read_to_vec()
            .map_err(|e| format!("the update channel's answer: {e}"))?;
        return serde_json::from_slice(&body)
            .map_err(|e| format!("the update channel's latest.json: {e}"));
    }
    Err("the update channel redirected too many times".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn only_images_pinned_by_digest_are_taken() {
        assert!(image_ok(&format!("ghcr.io/snds/warpcore@{DIGEST}")).is_ok());
        assert!(image_ok(&format!("localhost:5000/warpcore@{DIGEST}")).is_ok());
        for bad in [
            "ghcr.io/snds/warpcore:latest".to_owned(),
            "ghcr.io/snds/warpcore".to_owned(),
            format!("ghcr.io/snds/warpcore:latest@{DIGEST}"),
            "ghcr.io/snds/warpcore@sha256:abc".to_owned(),
            format!("ghcr.io/snds/warpcore@{}", DIGEST.to_uppercase()),
            format!("GHCR.io/x@{DIGEST}"),
            format!("@{DIGEST}"),
        ] {
            assert!(image_ok(&bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_https_on_the_feed_hosts() {
        let hosts: Vec<String> = GITHUB_HOSTS.iter().map(|h| (*h).to_owned()).collect();
        let ok = "https://github.com/snds/releases/releases/download/warpcore-server/latest.json";
        assert!(allowed(ok, &hosts, false).is_ok());
        for bad in [
            "http://github.com/x",
            "https://evil.example/x",
            "https://user@github.com/x",
            "https://github.com:8443/x",
            "http://127.0.0.1:9/x",
            "file:///etc/passwd",
        ] {
            assert!(allowed(bad, &hosts, false).is_err(), "{bad}");
        }
        assert!(allowed("http://127.0.0.1:9/x", &hosts, true).is_ok());
        assert!(allowed("http://10.0.0.2:9/x", &hosts, true).is_err());
    }

    #[test]
    fn the_statement_names_app_version_service_and_image() {
        assert_eq!(
            statement("warpcore", "1.2.0", "web", "r/w@sha256:00"),
            "sy-server-release/1\napp=warpcore\nversion=1.2.0\nservice=web\nimage=r/w@sha256:00\n"
        );
    }

    #[test]
    fn the_meta_statement_names_app_version_and_the_by_hand_flags() {
        assert_eq!(
            meta_statement("warpcore", "1.2.0", false, "1.0.4"),
            "sy-server-release-meta/1\napp=warpcore\nversion=1.2.0\nby_hand=0\nby_hand_at=1.0.4\n"
        );
        assert!(meta_statement("warpcore", "1.2.0", true, "").contains("\nby_hand=1\n"));
    }
}
