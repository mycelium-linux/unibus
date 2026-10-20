use anyhow::{Context, Result, ensure};
use std::{
    io::{BufRead, Read, Write},
    path::Path,
};
use unibus_ipfs::{
    catalog::{self, Config},
    library,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("mcp") {
        ensure!(
            args.len() == 3,
            "usage: unibus-ipfs-library mcp LOOPBACK_ORIGIN TOKEN_FILE"
        );
        return mcp(&args[1], Path::new(&args[2]));
    }
    ensure!(
        args.len() >= 2,
        "usage: unibus-ipfs-library serve|refresh|label CONFIG [CID NAME]"
    );
    let config = Config::read(Path::new(&args[1]))?;
    match args[0].as_str() {
        "serve" => tokio::runtime::Runtime::new()?.block_on(library::serve(config)),
        "refresh" => {
            let snapshot = catalog::refresh(&config)?;
            println!(
                "{} catalog entries; {} node observations",
                snapshot.entries.len(),
                snapshot.nodes.len()
            );
            Ok(())
        }
        "label" if args.len() == 4 => catalog::label(&config, &args[2], &args[3]),
        _ => anyhow::bail!("invalid library command or arguments"),
    }
}

fn mcp(origin: &str, token_path: &Path) -> Result<()> {
    // Same loopback-origin restriction as the Kubo client, with no redirect or proxy.
    unibus_ipfs::Kubo::new(origin)?;
    let token = library::read_token(token_path)?;
    let client = reqwest::blocking::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let mut input = std::io::stdin().lock();
    let mut output = std::io::stdout().lock();
    loop {
        let mut line = String::new();
        let count = input.by_ref().take(65537).read_line(&mut line)?;
        if count == 0 {
            return Ok(());
        }
        ensure!(count <= 65536, "MCP request exceeds limit");
        let request: serde_json::Value = serde_json::from_str(&line)?;
        let response = client
            .post(format!("{}/mcp", origin.trim_end_matches('/')))
            .bearer_auth(&token)
            .json(&request)
            .send()?
            .error_for_status()?;
        if request.get("id").is_none() {
            continue;
        }
        let mut bytes = Vec::new();
        response.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1024 * 1024, "MCP reply exceeds limit");
        let reply: serde_json::Value =
            serde_json::from_slice(&bytes).context("invalid MCP reply")?;
        serde_json::to_writer(&mut output, &reply)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
}
