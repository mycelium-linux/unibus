use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, Read, Write},
    net::SocketAddr,
};
use subtle::ConstantTimeEq;
use unibus_hue::{Config, Hue, mcp_call, mcp_tools};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    ensure!(
        args.len() == 2 && ["serve", "inspect", "stdio", "pair"].contains(&args[0].as_str()),
        "usage: unibus-hue serve|inspect|stdio|pair <config.json>"
    );
    let config: Config = serde_json::from_slice(&std::fs::read(&args[1])?)?;
    let hue = Hue::connect(&config)?;
    if args[0] == "pair" {
        hue.pair()?;
        eprintln!(
            "Hue local key saved privately; no light changes made. Enroll intended light IDs before writes."
        );
        return Ok(());
    }
    if args[0] == "inspect" {
        println!("{}", serde_json::to_string_pretty(&hue.catalog()?)?);
        return Ok(());
    }
    if args[0] == "stdio" {
        let mut input = std::io::stdin().lock();
        let mut output = std::io::stdout().lock();
        loop {
            let mut bytes = Vec::new();
            let n = input.by_ref().take(65537).read_until(b'\n', &mut bytes)?;
            if n == 0 {
                return Ok(());
            }
            ensure!(
                bytes.len() <= 65536 && bytes.last() == Some(&b'\n'),
                "MCP input exceeds line limit"
            );
            let rpc: Value = serde_json::from_slice(&bytes)?;
            let (status, reply) = rpc_reply(&hue, rpc)?;
            if status != 202 {
                serde_json::to_writer(&mut output, &reply)?;
                writeln!(output)?;
                output.flush()?;
            }
        }
    }
    let listen: SocketAddr = config.listen.parse()?;
    ensure!(
        listen.ip().is_loopback(),
        "Hue HTTP edge must bind loopback; publish via UniBus for remote access"
    );
    let token = std::fs::read_to_string(&config.provider_token_file)
        .context("read provider bearer token")?;
    let token = token.trim();
    ensure!(
        token.len() >= 32,
        "provider bearer token must contain at least 32 characters"
    );
    let expected = format!("Bearer {token}");
    let server =
        tiny_http::Server::http(listen).map_err(|e| anyhow::anyhow!("bind Hue edge: {e}"))?;
    eprintln!(
        "unibus-hue listening on {listen}; enrolled writes={}",
        config.allowed_light_ids.len()
    );
    // Serial executor + bounded replay ledger. Failed/uncertain mutations are
    // retained too: clients must observe again, not replay a possibly applied PUT.
    let mut ledger: BTreeMap<String, (Value, u16, Value)> = BTreeMap::new();
    for mut request in server.incoming_requests() {
        let auth = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("authorization"))
            .map(|h| h.value.as_str())
            .unwrap_or("");
        if !bool::from(auth.as_bytes().ct_eq(expected.as_bytes())) {
            respond(request, 401, json!({"error":"unauthorized"}))?;
            continue;
        }
        let version = request
            .headers()
            .iter()
            .find(|h| h.field.equiv("dcp-version"))
            .map(|h| h.value.as_str());
        if request.url().starts_with("/v1/") && version != Some(dcp::VERSION) {
            respond(request, 400, json!({"error":"unsupported_version"}))?;
            continue;
        }
        if request.body_length().is_some_and(|n| n > 65536) {
            respond(request, 413, json!({"error":"body_too_large"}))?;
            continue;
        }
        let method = request.method().as_str().to_owned();
        let path = request.url().split('?').next().unwrap_or("").to_owned();
        let mut bytes = Vec::new();
        request.as_reader().take(65537).read_to_end(&mut bytes)?;
        if bytes.len() > 65536 {
            respond(request, 413, json!({"error":"body_too_large"}))?;
            continue;
        }
        let result: Result<(u16, Value)> = (|| {
            match (method.as_str(), path.as_str()) {
                ("GET", "/.well-known/dcp") => Ok((200, serde_json::to_value(hue.discovery())?)),
                ("GET", "/v1/decisions") => Ok((200, serde_json::to_value(hue.catalog()?)?)),
                ("POST", "/v1/decisions/execute") => {
                    let value: Value = serde_json::from_slice(&bytes)?;
                    let execute: dcp::ExecuteRequest = serde_json::from_value(value.clone())?;
                    ensure!(
                        execute.request_id.len() <= 128 && !execute.request_id.is_empty(),
                        "invalid request id"
                    );
                    if let Some((previous, code, body)) = ledger.get(&execute.request_id) {
                        return if previous == &value {
                            Ok((*code, body.clone()))
                        } else {
                            Ok((409, json!({"error":"request_id_conflict"})))
                        };
                    }
                    ensure!(
                        ledger.len() < 4096,
                        "replay ledger full; restart edge after reconciling in-flight requests"
                    );
                    // Reserve before mutation, including network/readback errors.
                    let outcome = match hue.execute(&execute) {
                        Ok(receipt) => (
                            if receipt.outcome == dcp::Outcome::Rejected {
                                409
                            } else {
                                200
                            },
                            serde_json::to_value(receipt)?,
                        ),
                        Err(error) => {
                            eprintln!("Hue execution failed: {error:#}");
                            (
                                502,
                                json!({"error":"execution_failed","message":"Observe bridge state before retrying; mutation outcome may be uncertain."}),
                            )
                        }
                    };
                    ledger.insert(execute.request_id, (value, outcome.0, outcome.1.clone()));
                    Ok(outcome)
                }
                ("POST", "/mcp") => {
                    let rpc: Value = serde_json::from_slice(&bytes)?;
                    rpc_reply(&hue, rpc)
                }
                _ => Ok((404, json!({"error":"not_found"}))),
            }
        })();
        let (status, body) = result.unwrap_or_else(|error| {
            eprintln!("Hue edge request failed: {error:#}");
            (502, json!({"error":"provider_unavailable"}))
        });
        respond(request, status, body)?;
    }
    Ok(())
}
fn rpc_reply(hue: &Hue, rpc: Value) -> Result<(u16, Value)> {
    ensure!(rpc["jsonrpc"] == "2.0", "invalid JSON-RPC version");
    if rpc.get("id").is_none() {
        return Ok((202, Value::Null));
    }
    let id = rpc["id"].clone();
    if !["initialize", "ping", "tools/list", "tools/call"]
        .contains(&rpc["method"].as_str().unwrap_or(""))
    {
        return Ok((
            200,
            json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}}),
        ));
    }
    let result: Result<Value> = (|| {
        Ok(match rpc["method"].as_str().unwrap_or("") {
            "initialize" => {
                json!({"protocolVersion":"2025-06-18","capabilities":{"tools":{"listChanged":false}},"serverInfo":{"name":"unibus-hue","version":env!("CARGO_PKG_VERSION")}})
            }
            "ping" => json!({}),
            "tools/list" => mcp_tools(&hue.catalog()?),
            "tools/call" => {
                let name = rpc
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .context("missing tool name")?;
                let args = rpc
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                match mcp_call(hue, name, args) {
                    Ok(value) => {
                        json!({"content":[{"type":"text","text":serde_json::to_string(&value)?}],"isError":false})
                    }
                    Err(error) => {
                        eprintln!("Hue MCP tool failed: {error:#}");
                        json!({"content":[{"type":"text","text":"Hue call failed; observe current state before retrying."}],"isError":true})
                    }
                }
            }
            _ => return Err(anyhow::anyhow!("unsupported method")),
        })
    })();
    Ok((
        200,
        match result {
            Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
            Err(error) => {
                eprintln!("Hue MCP request failed: {error:#}");
                json!({"jsonrpc":"2.0","id":id,"error":{"code":-32000,"message":"Hue provider request failed"}})
            }
        },
    ))
}
fn respond(request: tiny_http::Request, status: u16, body: Value) -> Result<()> {
    let response = tiny_http::Response::from_string(if status == 202 {
        String::new()
    } else {
        serde_json::to_string(&body)?
    })
    .with_status_code(status)
    .with_header(tiny_http::Header::from_bytes("content-type", "application/json").unwrap())
    .with_header(tiny_http::Header::from_bytes("dcp-version", dcp::VERSION).unwrap())
    .with_header(tiny_http::Header::from_bytes("cache-control", "no-store").unwrap());
    if let Err(error) = request.respond(response) {
        eprintln!("Hue HTTP client disconnected: {error}");
    }
    Ok(())
}
