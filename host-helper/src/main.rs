//! `app-updates-host-helper`: the helper's command line.
//!
//! ```text
//! app-updates-host-helper serve  --config /config/allow-list.toml
//! app-updates-host-helper check  --config /config/allow-list.toml
//! app-updates-host-helper keygen --stack lockdown --keys /config/keys [--env-file /path/to/stack/host-helper.env]
//! app-updates-host-helper call   --env-file /path/to/stack/host-helper.env status|restart|update [--tag <tag>]
//! ```

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use host_helper::allow::AllowList;
use host_helper::docker::Cli;
use host_helper::protocol::{self, ENV_KEY, ENV_STACK, Key};
use host_helper::server::{self, Helper};
use host_helper::{Client, UpdateRequest};

const USAGE: &str = "\
app-updates-host-helper: one-click Update and Reload for the servers on this host.

  serve  --config <allow-list.toml>     Run the helper.
  check  --config <allow-list.toml>     Check the allow-list and keys, then stop.
  keygen --stack <name> --keys <dir> [--env-file <file>]
         Make a stack's key: <dir>/<name>.key (mode 600). With --env-file, the
         server's settings are added to that file (mode 600) instead of printed.
  call   --env-file <file> status|restart|update [--tag <tag>]
         Call the helper as a server would, with the settings in that env file
         (HOST_HELPER_URL, HOST_HELPER_STACK, HOST_HELPER_KEY). For checking the
         wiring from a container on the stack's helper network.
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("check") => check(&args[1..]),
        Some("keygen") => keygen(&args[1..]),
        Some("call") => call(&args[1..]),
        Some("--help" | "-h" | "help") => {
            print!("{USAGE}");
            Ok(())
        }
        _ => Err(format!("Unknown command.\n\n{USAGE}")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}

/// The value after `--name`, refusing anything not in `allowed`.
fn flags(args: &[String], allowed: &[&str]) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(flag) = it.next() {
        let name = flag
            .strip_prefix("--")
            .filter(|n| allowed.contains(n))
            .ok_or_else(|| format!("Unknown option {flag}.\n\n{USAGE}"))?;
        let value = it
            .next()
            .ok_or_else(|| format!("--{name} needs a value."))?;
        out.push((name.to_owned(), value.clone()));
    }
    Ok(out)
}

fn get<'a>(flags: &'a [(String, String)], name: &str) -> Option<&'a str> {
    flags
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

fn load(args: &[String]) -> Result<AllowList, String> {
    let f = flags(args, &["config"])?;
    let path = get(&f, "config").ok_or("--config <allow-list.toml> is needed.")?;
    AllowList::load(Path::new(path)).map_err(|e| e.to_string())
}

fn check(args: &[String]) -> Result<(), String> {
    let list = load(args)?;
    println!(
        "The allow-list is good. Listening on: {}",
        list.listen.join(", ")
    );
    for s in list.stacks.values() {
        println!(
            "  {}: {} in {} (services {}; tags {}{}){}{}",
            s.name,
            s.repositories.join(" + "),
            s.directory.display(),
            s.services.join(", "),
            s.tags.join(", "),
            if s.main_versions {
                ", main-channel versions"
            } else {
                ""
            },
            s.before
                .as_ref()
                .map(|b| format!("; before: {} {}", b.service, b.command.join(" ")))
                .unwrap_or_default(),
            s.after
                .as_ref()
                .map(|a| format!("; after: {} {}", a.service, a.command.join(" ")))
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn serve(args: &[String]) -> Result<(), String> {
    let list = load(args)?;
    let docker = Cli {
        program: list.docker.clone(),
    };
    eprintln!(
        "{} starting: stacks {}",
        server::utc(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        ),
        list.stacks.keys().cloned().collect::<Vec<_>>().join(", ")
    );
    server::run(Arc::new(Helper::new(list, docker))).map_err(|e| format!("The helper stopped: {e}"))
}

fn keygen(args: &[String]) -> Result<(), String> {
    let f = flags(args, &["stack", "keys", "env-file"])?;
    let stack = get(&f, "stack").ok_or("--stack <name> is needed.")?;
    if !protocol::valid_name(stack) {
        return Err("The stack name must be lower-case letters, digits, - and _.".into());
    }
    let dir = PathBuf::from(get(&f, "keys").ok_or("--keys <dir> is needed.")?);
    std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let path = dir.join(format!("{stack}.key"));
    if path.exists() {
        return Err(format!(
            "{} already exists. To replace the key, delete it first, then update the server's env file too.",
            path.display()
        ));
    }
    let key = Key::generate().map_err(|e| format!("The random source failed: {e}"))?;
    write_private(&path, &format!("{}\n", key.to_hex()), false)?;
    let settings = format!(
        "# The host helper (one-click Update and Reload). Keep this file private.\n{ENV_STACK}={stack}\n{ENV_KEY}={}\n",
        key.to_hex()
    );
    match get(&f, "env-file") {
        Some(env) => {
            write_private(Path::new(env), &settings, true)?;
            println!(
                "Made {} and added {ENV_STACK} and {ENV_KEY} to {env}. Add HOST_HELPER_URL there too (see the README).",
                path.display()
            );
        }
        None => {
            println!(
                "Made {}. Put these lines in the server's env file:\n",
                path.display()
            );
            print!("{settings}");
        }
    }
    Ok(())
}

fn write_private(path: &Path, text: &str, append: bool) -> Result<(), String> {
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true);
    if append {
        opts.append(true).create(true);
    } else {
        opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    file.write_all(text.as_bytes())
        .map_err(|e| format!("{}: {e}", path.display()))
}

fn call(args: &[String]) -> Result<(), String> {
    // The operation is the one argument that isn't an option or an option's value.
    let mut op = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a.starts_with("--") {
            rest.push(a.clone());
            rest.extend(it.next().cloned());
        } else if op.is_none() {
            op = Some(a.as_str());
        } else {
            return Err(format!("Unexpected argument {a}.\n\n{USAGE}"));
        }
    }
    let op = op.ok_or_else(|| format!("call needs status, restart or update.\n\n{USAGE}"))?;
    let f = flags(&rest, &["env-file", "tag"])?;
    let env = get(&f, "env-file").ok_or("--env-file <file> is needed.")?;
    let text = std::fs::read_to_string(env).map_err(|e| format!("{env}: {e}"))?;
    let vars: Vec<(String, String)> = text
        .lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().trim_matches('"').to_owned()))
        .collect();
    let client = Client::from_vars(|name| {
        vars.iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
    })
    .map_err(|e| e.to_string())?;
    let out = match op {
        "status" => serde_json::to_string_pretty(&client.status().map_err(|e| e.to_string())?),
        "restart" => serde_json::to_string_pretty(&client.restart().map_err(|e| e.to_string())?),
        "update" => serde_json::to_string_pretty(
            &client
                .update(&UpdateRequest {
                    tag: get(&f, "tag").map(str::to_owned),
                    expect_digest: None,
                    ..Default::default()
                })
                .map_err(|e| e.to_string())?,
        ),
        other => {
            return Err(format!(
                "Unknown operation {other}: status, restart or update."
            ));
        }
    };
    println!("{}", out.expect("a reply always serializes"));
    Ok(())
}
