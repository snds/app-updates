//! The rules' tests, ported from Lockdown's updater (slice 59) with Lockdown's feed as the example.

use super::*;

fn v(s: &str) -> Version {
    Version::parse(s).unwrap()
}

/// A feed like Lockdown's, for these tests.
const FEED: Feed = Feed {
    app: "Lockdown",
    main_endpoint: "https://github.com/snds/lockdown/releases/download/main-build/latest.json",
    main_page: "https://github.com/snds/lockdown/releases/tag/main-build",
    stable_endpoint: None,
    stable_page: None,
    hosts: &GITHUB_HOSTS,
};

/// A minisign public key in the updater's form, made for these tests only; its secret half was
/// never kept.
const TEST_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQyOUE3MTU4MUNBMTA3RkQKUldUOUI2RWNXSEdhMHBLeWVGS0xVTEZFZy83Q0xiL3VjN1NDdWxLQ25MSUxOYTJLZDNaUUhtSHkK";

#[test]
fn main_builds_order_by_commit_time_and_before_the_release() {
    let a = v("0.1.0-main.20261107093000");
    let b = v("0.1.0-main.20261107120000");
    let c = v("0.1.0-main.20261201000000");
    assert!(is_newer(&a, &b));
    assert!(is_newer(&b, &c));
    assert!(!is_newer(&b, &a));
    assert!(!is_newer(&b, &b), "the same build is never an update");
    assert!(is_newer(&v("0.1.0-main.9"), &v("0.1.0-main.10")));
    assert!(is_newer(&c, &v("0.1.0")));
    assert!(is_newer(&c, &v("0.2.0")));
    assert!(is_newer(&v("0.1.0"), &v("0.1.1-main.20270101000000")));
    assert!(!is_newer(&v("0.2.0"), &v("0.1.0-main.20991231235959")));
}

#[test]
fn the_channel_comes_from_the_version() {
    assert_eq!(Channel::of(&v("0.1.0-main.20261107093000")), Channel::Main);
    assert_eq!(Channel::of(&v("0.1.0")), Channel::Stable);
    assert_eq!(Channel::of(&v("0.1.0-beta.1")), Channel::Stable);
    assert_eq!(Channel::of(&v("0.1.0-mainline.1")), Channel::Stable);
    assert_eq!(Channel::Main.endpoint(&FEED), Some(FEED.main_endpoint));
    assert_eq!(Channel::Stable.endpoint(&FEED), None);
}

#[test]
fn the_plan_needs_a_valid_key_and_a_built_channel() {
    let main = v("0.1.0-main.20261107093000");
    assert_eq!(plan(&FEED, &main, None, None), Err(Off::NoKey));
    assert_eq!(
        plan(&FEED, &main, Some("not a key"), None),
        Err(Off::BadKey)
    );
    assert_eq!(
        plan(
            &FEED,
            &main,
            Some(&data_encoding::BASE64.encode(b"untrusted comment: x\nRWQ")),
            None
        ),
        Err(Off::BadKey)
    );
    assert_eq!(
        plan(&FEED, &v("0.1.0"), Some(TEST_KEY), None),
        Err(Off::StableNotBuilt)
    );
    let source = plan(&FEED, &main, Some(TEST_KEY), None).unwrap();
    assert_eq!(source.endpoint, FEED.main_endpoint);
    assert_eq!(source.pubkey, TEST_KEY);
    assert_eq!(source.page, FEED.main_page);
    assert!(allowed(&source.endpoint, &source.hosts).is_ok());
    assert!(allowed(&source.page, &source.hosts).is_ok());
    for words in [Off::NoKey, Off::BadKey, Off::StableNotBuilt].map(Off::words) {
        assert!(words.starts_with("Updates are off in this build"));
    }
}

#[test]
fn a_built_stable_channel_is_followed_by_a_plain_version() {
    let feed = Feed {
        stable_endpoint: Some(
            "https://github.com/snds/releases/releases/download/x-stable/latest.json",
        ),
        stable_page: Some("https://github.com/snds/releases/releases/tag/x-stable"),
        ..FEED
    };
    let source = plan(&feed, &v("1.2.0"), Some(TEST_KEY), None).unwrap();
    assert_eq!(Some(source.endpoint.as_str()), feed.stable_endpoint);
    assert_eq!(Some(source.page.as_str()), feed.stable_page);
}

#[test]
fn a_test_source_replaces_the_channel_only_with_a_valid_key() {
    let stable = v("0.1.0");
    let test = TestSource {
        endpoint: "http://127.0.0.1:8765/latest.json".into(),
        pubkey: TEST_KEY.into(),
    };
    let source = plan(&FEED, &stable, None, Some(test.clone())).unwrap();
    assert_eq!(source.hosts, vec!["127.0.0.1".to_string()]);
    assert!(allowed("http://127.0.0.1:8765/b.app.tar.gz", &source.hosts).is_ok());
    assert!(allowed(FEED.main_endpoint, &source.hosts).is_err());
    let bad = TestSource {
        pubkey: "nope".into(),
        ..test
    };
    assert_eq!(plan(&FEED, &stable, None, Some(bad)), Err(Off::BadKey));
    #[cfg(not(feature = "update-test"))]
    assert_eq!(TestSource::from_env("LOCKDOWN"), None);
}

#[test]
fn updates_come_only_from_github_over_https() {
    let hosts: Vec<String> = GITHUB_HOSTS.iter().map(|h| h.to_string()).collect();
    for ok in [
        FEED.main_endpoint,
        "https://objects.githubusercontent.com/github-production-release-asset/1/2?x=y",
        "https://release-assets.githubusercontent.com/github-production-release-asset/1/2",
        "https://GitHub.com/snds/lockdown/releases/download/main-build/Lockdown.app.tar.gz",
        "https://github.com/snds/releases/releases/download/synapse-main/latest.json",
    ] {
        assert_eq!(allowed(ok, &hosts), Ok(()), "{ok}");
    }
    for bad in [
        "http://github.com/snds/lockdown/releases/download/main-build/latest.json",
        "https://github.com.evil.example/latest.json",
        "https://evil.example/github.com/latest.json",
        "https://github.com@evil.example/latest.json",
        "https://user:pass@github.com/latest.json",
        "https://github.com:8443/latest.json",
        "https://raw.githubusercontent.com/snds/lockdown/main/latest.json",
        "ftp://github.com/latest.json",
        "file:///etc/passwd",
        "not a url",
        "http://127.0.0.1:8765/latest.json",
    ] {
        assert!(allowed(bad, &hosts).is_err(), "{bad} was let through");
    }
}

#[test]
fn plain_http_is_only_for_a_test_source_on_this_computer() {
    let hosts = vec!["127.0.0.1".to_string(), "example.org".to_string()];
    assert!(allowed("http://127.0.0.1:9/latest.json", &hosts).is_ok());
    assert!(allowed("http://example.org/latest.json", &hosts).is_err());
}

#[test]
fn windows_updates_itself_only_from_a_per_user_install() {
    use std::path::Path;
    let local = Path::new(r"C:\Users\sean\AppData\Local");
    assert!(windows_per_user(
        Path::new(r"C:\Users\sean\AppData\Local\Lockdown\lockdown-desktop.exe"),
        Some(local)
    ));
    assert!(windows_per_user(
        Path::new(r"c:\users\SEAN\appdata\local\Programs\Lockdown\lockdown-desktop.exe"),
        Some(Path::new(r"C:\Users\sean\AppData\Local\"))
    ));
    assert!(!windows_per_user(
        Path::new(r"C:\Program Files\Lockdown\lockdown-desktop.exe"),
        Some(local)
    ));
    assert!(!windows_per_user(
        Path::new(r"C:\Users\sean\AppData\LocalLow\Lockdown\lockdown-desktop.exe"),
        Some(local)
    ));
    assert!(!windows_per_user(Path::new(r"C:\x.exe"), None));
    assert!(!windows_per_user(
        Path::new(r"C:\x.exe"),
        Some(Path::new(""))
    ));
}

#[test]
fn the_reasons_name_the_app() {
    assert_eq!(Install::InPlace.words("Vale"), None);
    assert_eq!(
        Install::LinuxPackage.words("Lockdown").as_deref(),
        Some("Lockdown can't update itself when installed from a Linux package.")
    );
    for mode in [Install::MachineWide, Install::NotWritable] {
        assert!(mode.words("Fontface").unwrap().starts_with("Fontface "));
    }
}

#[test]
fn the_status_moves_cleanly_between_states() {
    let version = v("0.1.0-main.20261107093000");
    let source = plan(&FEED, &version, Some(TEST_KEY), None);
    let mut s = UpdateStatus::at_start(&version, &source, true);
    assert!(s.enabled);
    assert_eq!(s.state, UpdateState::Idle);
    assert_eq!(s.channel, Channel::Main);
    s.to(UpdateState::Downloading);
    s.offered = Some("0.1.0-main.20261107120000".into());
    s.percent = percent(50, Some(100));
    assert_eq!(s.percent, Some(50));
    s.to(UpdateState::Ready);
    assert_eq!(s.percent, None);
    assert_eq!(s.offered.as_deref(), Some("0.1.0-main.20261107120000"));
    s.to(UpdateState::Failed);
    s.error = Some("no connection".into());
    s.to(UpdateState::Checking);
    assert_eq!((s.error.as_deref(), s.offered.as_deref()), (None, None));
    let off = UpdateStatus::at_start(&version, &Err(Off::NoKey), true);
    assert!(!off.enabled);
    assert_eq!(off.off.as_deref(), Some(Off::NoKey.words()));
    let json = serde_json::to_value(&off).unwrap();
    assert_eq!(json["state"], "idle");
    assert_eq!(json["channel"], "main");
}

#[test]
fn progress_never_reads_done_before_the_check() {
    assert_eq!(percent(0, Some(10)), Some(0));
    assert_eq!(percent(10, Some(10)), Some(99));
    assert_eq!(percent(20, Some(10)), Some(99));
    assert_eq!(percent(5, None), None);
    assert_eq!(percent(5, Some(0)), None);
}

#[test]
fn a_key_is_read_from_the_key_not_its_comment() {
    assert!(key_is_valid(TEST_KEY));
    assert_eq!(key_id(TEST_KEY).as_deref(), Some("D29A71581CA107FD"));
    assert_eq!(key_id("bm90IGEga2V5"), None);
    assert!(!key_is_valid("bm90IGEga2V5"));
}

#[test]
fn a_local_build_says_local_and_always_checks() {
    let version = v("0.1.0");
    let s = UpdateStatus::at_start_local(&version, false);
    assert_eq!(s.channel, Channel::Local);
    assert!(s.enabled);
    assert_eq!(s.off, None);
    assert!(!s.auto_check);
    assert_eq!(s.state, UpdateState::Idle);
    let json = serde_json::to_value(&s).unwrap();
    assert_eq!(json["channel"], "local");
    assert_eq!(Channel::Local.endpoint(&FEED), None);
    assert_eq!(Channel::Local.page(&FEED), None);
}
