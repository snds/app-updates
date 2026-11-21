//! `service-app`: one product's service app (docs/self-hosting-service-apps.md).

use std::path::PathBuf;
use std::process::ExitCode;

use server_engine::release;
use server_engine::shell::{self, Config};

const USAGE: &str = "\
service-app: one product's service app: its updates and helper tooling.

  service-app serve --config FILE        run it (the image's default)
  service-app check-config --config FILE check the config and the key in it
  service-app healthcheck [ADDR]         exit 0 when it answers (default 127.0.0.1:7490)
  service-app statement APP VERSION SERVICE IMAGE
                                         print the bytes CI signs for one service
  service-app meta-statement APP VERSION BY_HAND BY_HAND_AT
                                         print the bytes CI signs for the release (BY_HAND: 0 or 1)
";

fn config(args: &[String]) -> Result<Config, String> {
    match args {
        [flag, path] if flag == "--config" => Config::read(&PathBuf::from(path)),
        _ => Err("expected --config FILE".into()),
    }
}

fn run(args: &[String]) -> Result<(), String> {
    match args.split_first() {
        Some((cmd, rest)) if cmd == "serve" => shell::run(&config(rest)?),
        Some((cmd, rest)) if cmd == "check-config" => {
            let c = config(rest)?;
            let feed = c.feed()?;
            if !release::key_ok(&feed.key) {
                return Err(format!(
                    "{}: not a tauri signer public key",
                    c.app.key_file.display()
                ));
            }
            println!("{}: the config and its release key are fine", c.app.id);
            Ok(())
        }
        Some((cmd, rest)) if cmd == "healthcheck" => {
            let addr = rest.first().map_or("127.0.0.1:7490", String::as_str);
            let resp = ureq::get(&format!("http://{addr}/healthz"))
                .call()
                .map_err(|e| e.to_string())?;
            if resp.status().as_u16() == 200 {
                Ok(())
            } else {
                Err(format!("answered {}", resp.status()))
            }
        }
        Some((cmd, rest)) if cmd == "statement" => match rest {
            [app, version, service, image] => {
                release::image_ok(image)?;
                print!("{}", release::statement(app, version, service, image));
                Ok(())
            }
            _ => Err("expected APP VERSION SERVICE IMAGE".into()),
        },
        Some((cmd, rest)) if cmd == "meta-statement" => match rest {
            [app, version, by_hand, at] => {
                let by_hand = match by_hand.as_str() {
                    "0" | "false" => false,
                    "1" | "true" => true,
                    _ => return Err("BY_HAND is 0 or 1".into()),
                };
                print!("{}", release::meta_statement(app, version, by_hand, at));
                Ok(())
            }
            _ => Err("expected APP VERSION BY_HAND BY_HAND_AT".into()),
        },
        _ => Err(USAGE.trim_end().into()),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("service-app: {e}");
            ExitCode::FAILURE
        }
    }
}
