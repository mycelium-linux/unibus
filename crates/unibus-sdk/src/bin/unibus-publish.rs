//! `unibus-publish` — put one JSON body on the bus from a shell.
//!
//! ```sh
//! arborist digest < snapshot.json \
//!   | unibus-publish --config ~/.config/unibus/arborist.json --kind arborist.scan
//! ```
//!
//! The embedded hosts reach the bus through [`unibus_sdk::Session`], which is a
//! long-lived session with a queue. This is the other shape: one message, and
//! the exit code says whether it was written. A scheduled job has nothing to
//! poll and nobody to report to but its own exit status.
//!
//! Exit: `0` sent, `1` refused or unreachable, `2` bad usage or input.

use std::io::{IsTerminal, Read, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use unibus::client::ClientConfig;
use unibus_sdk::{Publish, publish_once};

const USAGE: &str = "\
unibus-publish — put one JSON body on the bus

USAGE
  unibus-publish --config CLIENT.json --kind KIND [--destination ID] < BODY.json

  --config PATH        a ClientConfig (installation, producer, hub endpoint, zones)
  --kind KIND          the message kind, e.g. arborist.scan
  --destination ID     address one producer instead of every subscriber
  --dry-run            validate and print the frame id without connecting
  -h, --help           this page

The body arrives on stdin and must be a JSON object under 64 KiB — the same
rules an embedded host's `Publish` is held to, checked by the same code.
";

fn main() -> Result<()> {
    match run() {
        Ok(()) => Ok(()),
        Err(error) => {
            eprintln!("unibus-publish: {error:#}");
            std::process::exit(match error.downcast_ref::<Usage>() {
                Some(_) => 2,
                None => 1,
            });
        }
    }
}

/// A caller mistake, as opposed to the bus refusing us.
#[derive(Debug)]
struct Usage(String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

fn run() -> Result<()> {
    let mut config_path: Option<PathBuf> = None;
    let mut kind: Option<String> = None;
    let mut destination: Option<String> = None;
    let mut dry_run = false;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--config" => {
                index += 1;
                config_path = Some(PathBuf::from(value(&args, index, "--config")?));
            }
            "--kind" => {
                index += 1;
                kind = Some(value(&args, index, "--kind")?.to_owned());
            }
            "--destination" => {
                index += 1;
                destination = Some(value(&args, index, "--destination")?.to_owned());
            }
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => return Err(Usage(format!("unknown argument {other:?}")).into()),
        }
        index += 1;
    }

    let config_path = config_path.ok_or_else(|| Usage("--config is required".into()))?;
    let kind = kind.ok_or_else(|| Usage("--kind is required".into()))?;

    let body = read_body()?;
    let publish = Publish {
        destination,
        kind: kind.clone(),
        body,
    };
    publish
        .validate()
        .map_err(|error| Usage(format!("invalid body: {error}")))?;

    if dry_run {
        println!(
            "{{\"dry_run\":true,\"kind\":{}}}",
            serde_json::to_string(&kind)?
        );
        return Ok(());
    }

    let source =
        std::fs::read(&config_path).with_context(|| format!("read {}", config_path.display()))?;
    let mut config: ClientConfig = serde_json::from_slice(&source)
        .with_context(|| format!("parse {}", config_path.display()))?;
    // A `token_file` written relative to the config is relative to the config,
    // not to whoever's working directory the cron picked.
    config.resolve_token_file(&config_path);

    let request_id = publish_once(config, publish)?;
    println!("{request_id}");
    Ok(())
}

fn value<'a>(args: &'a [String], index: usize, flag: &str) -> Result<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| Usage(format!("{flag} needs a value")).into())
}

/// The body is the caller's JSON, and it is an object or it is refused: the
/// router flattens it into the envelope, and a body that is not an object has
/// nowhere to go.
fn read_body() -> Result<serde_json::Value> {
    if std::io::stdin().is_terminal() {
        return Err(Usage("no body on stdin".into()).into());
    }
    let mut bytes = Vec::new();
    std::io::stdin()
        .read_to_end(&mut bytes)
        .context("read stdin")?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Err(Usage("the body on stdin was empty".into()).into());
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| Usage(format!("parse the body: {error}")))?;
    if !value.is_object() {
        return Err(Usage("the body must be a JSON object".into()).into());
    }
    let _ = std::io::stdout().flush();
    Ok(value)
}
