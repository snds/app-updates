//! The updater plugin's own signature check (`Update::verify`, the check `download` and `install`
//! run), driven through Tauri's mock runtime with fixtures signed by a throwaway key whose secret
//! half was never kept. Ported from Lockdown's `updater/shell.rs`. CI also signs a bundle with a
//! key made in the job and runs `ci_signed_bundle_verifies` on it.

use tauri::test::{MockRuntime, mock_builder, mock_context, noop_assets};
use tauri_plugin_updater::{Updater, UpdaterExt};

use crate::is_newer;

const MAIN_ENDPOINT: &str =
    "https://github.com/snds/lockdown/releases/download/main-build/latest.json";

const KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQyOUE3MTU4MUNBMTA3RkQKUldUOUI2RWNXSEdhMHBLeWVGS0xVTEZFZy83Q0xiL3VjN1NDdWxLQ25MSUxOYTJLZDNaUUhtSHkK";
const OTHER_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEMxNDIxMjg4OUJFQjMyQTYKUldTbU11dWJpQkpDd1J5YTFEMm02b0VLUDAxbzZrd1JnZ2R2VU9MNVVCakVGejRLdTFOczZJcFoK";
const BUNDLE: &[u8] = b"lockdown update test bundle\n";
/// Signed with `--app-version 0.1.0-main.20261107120000`.
const SIGNED_FOR_VERSION: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUOUI2RWNXSEdhMGdwcVpnTzNCWlJtRDliNlMvRHlBaUNiNlV2aGJsMWJwQ3lkZ1c4emZKSE92djJPVXJuSTBDQThaRHRBNXNreC8xM1FubEtuLy90VUswWGtBYmVGYWdvPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk0MDE3MTU3CWZpbGU6YnVuZGxlLmJpbgl2ZXJzaW9uOjAuMS4wLW1haW4uMjAyNjExMDcxMjAwMDAKYUxkU0ZyemhubE1ET3RxR2dEd3hBT1VjckJMMmRKQS9nQVQ1RFhmUDgvb1pHSi9FQVJ0a1NwMVBvRHVUM3JIY2FZelM4eU1zeU44bjhiNzR1TldmQ2c9PQo=";
/// Signed with no version (an older CLI's signature).
const SIGNED_WITHOUT_VERSION: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVUOUI2RWNXSEdhMHVtZmFmQlZyVkRVSDNscm1uaGp5b0V4dFlwdndVZEU1aTdlYXFoWmhGVXZmeUladWlrSk1uYjZyRURzcVNuS09WWUo0WUxiUEs0Y3p2dGlHd0tkMndjPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzk0MDE3MTU4CWZpbGU6YnVuZGxlLmJpbgpCYnc2NytpemxpUkZnY3VEcDF1OSs2TEJIS0Zlc3R3MEw2Qi9VZlpwS2x5cjhZQk1OWHcwMmRpTnUzSjhJaThUZkhqYWczbzdzSlB1K3RjRVdQcjBCZz09Cg==";
const OFFERED: &str = "0.1.0-main.20261107120000";

fn updater(current: &str, key: &str) -> Updater {
    let mut context = mock_context::<MockRuntime, _>(noop_assets());
    context.package_info_mut().version = current.parse().unwrap();
    context.config_mut().plugins.0.insert(
        "updater".into(),
        serde_json::json!({ "pubkey": "", "requireSignedVersion": true }),
    );
    let app = mock_builder()
        .plugin(tauri_plugin_updater::Builder::new().build())
        .build(context)
        .unwrap();
    app.updater_builder()
        .pubkey(key)
        .endpoints(vec![MAIN_ENDPOINT.parse().unwrap()])
        .unwrap()
        .version_comparator(|current, release| is_newer(&current, &release.version))
        .build()
        .unwrap()
}

fn manifest(version: &str, signature: &str) -> serde_json::Value {
    let entry = serde_json::json!({
        "url": "https://github.com/snds/lockdown/releases/download/main-build/Lockdown.app.tar.gz",
        "signature": signature,
    });
    serde_json::json!({
        "version": version,
        "platforms": {
            "darwin-aarch64": entry, "darwin-x86_64": entry,
            "windows-x86_64": entry, "linux-x86_64": entry,
        },
    })
}

fn check(
    current: &str,
    key: &str,
    version: &str,
    signature: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let update = updater(current, key)
        .restore_update(manifest(version, signature))
        .map_err(|e| e.to_string())?
        .ok_or("not an update")?;
    update.verify(bytes).map_err(|e| e.to_string())
}

const CURRENT: &str = "0.1.0-main.20261107093000";

#[test]
fn a_bundle_signed_with_the_key_for_its_version_is_accepted() {
    check(CURRENT, KEY, OFFERED, SIGNED_FOR_VERSION, BUNDLE).unwrap();
}

#[test]
fn a_changed_bundle_is_refused() {
    let mut changed = BUNDLE.to_vec();
    changed[0] ^= 1;
    assert!(check(CURRENT, KEY, OFFERED, SIGNED_FOR_VERSION, &changed).is_err());
}

#[test]
fn another_key_is_refused() {
    assert!(check(CURRENT, OTHER_KEY, OFFERED, SIGNED_FOR_VERSION, BUNDLE).is_err());
}

#[test]
fn an_unsigned_or_garbled_signature_is_refused() {
    for bad in ["", "not base64", "dW50cnVzdGVkIGNvbW1lbnQ6IG5vdGhpbmcK"] {
        assert!(
            check(CURRENT, KEY, OFFERED, bad, BUNDLE).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn a_signature_for_another_version_or_none_is_refused() {
    // The manifest isn't signed: a newer number beside an older
    // bundle's signature must not install that bundle.
    let e = check(
        CURRENT,
        KEY,
        "0.1.0-main.20261201000000",
        SIGNED_FOR_VERSION,
        BUNDLE,
    )
    .unwrap_err();
    assert!(e.contains("version"), "{e}");
    assert!(check(CURRENT, KEY, OFFERED, SIGNED_WITHOUT_VERSION, BUNDLE).is_err());
}

#[test]
fn the_same_or_an_older_version_is_not_an_update() {
    for current in [OFFERED, "0.1.0-main.20261201000000", "0.1.0"] {
        let found = updater(current, KEY)
            .restore_update(manifest(OFFERED, SIGNED_FOR_VERSION))
            .unwrap();
        assert!(found.is_none(), "{current} took {OFFERED}");
    }
}

/// A bundle CI signed with a throwaway key made in the job: the folder
/// in `APP_UPDATES_TEST_SIGNED` holds `bundle`, `bundle.sig`,
/// `key.pub` and `version`. Without it this test has nothing to check.
#[test]
fn ci_signed_bundle_verifies() {
    let Some(dir) = std::env::var_os("APP_UPDATES_TEST_SIGNED") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let read = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .unwrap()
            .trim()
            .to_string()
    };
    let bundle = std::fs::read(dir.join("bundle")).unwrap();
    let (key, signature, version) = (read("key.pub"), read("bundle.sig"), read("version"));
    check(CURRENT, &key, &version, &signature, &bundle).unwrap();
    let mut changed = bundle.clone();
    changed.push(0);
    assert!(check(CURRENT, &key, &version, &signature, &changed).is_err());
    assert!(check(CURRENT, KEY, &version, &signature, &bundle).is_err());
    println!(
        "the updater's own check accepted the CI-signed bundle and refused it changed or under another key"
    );
}
