//! Test-only HTTP process. This is never an inference backend or release binary.
#[cfg(unix)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    use std::{
        collections::HashMap,
        io::{Read, Write},
        os::unix::net::UnixListener,
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|value| value == "--fixture-ui") {
        return run_fixture_ui(&args[1..]);
    }
    if args
        .first()
        .is_some_and(|value| value == "--fixture-parent")
    {
        return run_fixture_parent(&args[1..]);
    }
    let switches = [
        "--no-ui",
        "--no-ui-mcp-proxy",
        "--jinja",
        "--no-context-shift",
    ];
    let paired: Vec<_> = args
        .iter()
        .filter(|argument| !switches.contains(&argument.as_str()))
        .collect();
    if !paired.len().is_multiple_of(2)
        || switches
            .iter()
            .any(|flag| !args.iter().any(|argument| argument == flag))
    {
        return Err("bounded profile switches required".into());
    }
    let args: HashMap<&str, &str> = paired
        .chunks_exact(2)
        .map(|pair| (pair[0].as_str(), pair[1].as_str()))
        .collect();
    if args.get("--n-gpu-layers") != Some(&"0")
        || args.get("--threads") != Some(&"2")
        || args.get("--parallel") != Some(&"1")
    {
        return Err("CPU bounds missing".into());
    }
    let model = std::fs::read(args.get("--model").ok_or("model")?)?;
    if model == b"GGUF-exit" {
        std::process::exit(17);
    }
    let key = std::fs::read_to_string(args.get("--api-key-file").ok_or("key")?)?;
    let listener = UnixListener::bind(args.get("--host").ok_or("socket")?)?;
    for stream in listener.incoming() {
        let mut stream = stream?;
        stream.set_read_timeout(Some(std::time::Duration::from_secs(2)))?;
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        while !request.windows(4).any(|part| part == b"\r\n\r\n") {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            request.extend_from_slice(&buffer[..count]);
            if request.len() > 64 * 1024 {
                return Err("request limit".into());
            }
        }
        let header_end = request
            .windows(4)
            .position(|part| part == b"\r\n\r\n")
            .ok_or("headers")?
            + 4;
        let content_length = String::from_utf8_lossy(&request[..header_end])
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse::<usize>())
            })
            .transpose()?
            .unwrap_or(0);
        if header_end + content_length > 64 * 1024 {
            return Err("request body limit".into());
        }
        while request.len() < header_end + content_length {
            let count = stream.read(&mut buffer)?;
            if count == 0 {
                return Err("truncated body".into());
            }
            request.extend_from_slice(&buffer[..count]);
        }
        let request = String::from_utf8_lossy(&request);
        let authorized = request
            .to_ascii_lowercase()
            .contains(&format!("authorization: bearer {}", key.trim()));
        let stop = authorized && request.starts_with("GET /fixture-stop ");
        let (status, content_type, body) = if !authorized {
            ("401 Unauthorized", "application/json", "{}".to_string())
        } else if stop {
            ("200 OK", "application/json", "{}".into())
        } else if request.starts_with("GET /health ") {
            if model == b"GGUF-loading" {
                (
                    "503 Service Unavailable",
                    "application/json",
                    "{\"status\":\"loading\"}".into(),
                )
            } else {
                ("200 OK", "application/json", "{\"status\":\"ok\"}".into())
            }
        } else if request.starts_with("POST /v1/chat/completions/input_tokens ") {
            ("200 OK", "application/json", "{\"input_tokens\":4}".into())
        } else if request.starts_with("POST /v1/chat/completions ") {
            let first = serde_json::json!({"id":"fixture-response","choices":[{"index":0,"delta":{"content":"test fixture response"},"finish_reason":null}]});
            let last = serde_json::json!({"id":"fixture-response","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":4,"completion_tokens":3}});
            (
                "200 OK",
                "text/event-stream",
                format!("data: {first}\n\ndata: {last}\n\ndata: [DONE]\n\n"),
            )
        } else {
            ("404 Not Found", "application/json", "{}".into())
        };
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = stream.write_all(response.as_bytes());
        if stop {
            break;
        }
    }
    Ok(())
}
#[cfg(unix)]
fn run_fixture_parent(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use orange_model_runtime::supervisor::{CpuProfile, ManagedCpuModel};
    use sha2::{Digest, Sha256};
    use std::io::Write;
    if args.len() != 3 {
        return Err("fixture parent: root binary model".into());
    }
    let model = std::fs::read(&args[2])?;
    let profile = CpuProfile {
        socket_directory: None,
        executable: args[1].clone().into(),
        executable_sha256: hex::encode(Sha256::digest(std::fs::read(&args[1])?)),
        model: args[2].clone().into(),
        model_sha256: hex::encode(Sha256::digest(&model)),
        model_bytes: model.len() as u64,
        model_version: "fixture-only".into(),
        context_tokens: 2048,
        startup_timeout: std::time::Duration::from_secs(5),
    };
    tokio::runtime::Runtime::new()?.block_on(async {
        let _managed = ManagedCpuModel::launch(std::path::Path::new(&args[0]), profile).await?;
        println!("FIXTURE_READY");
        std::io::stdout().flush()?;
        std::future::pending::<()>().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}
#[cfg(not(unix))]
fn main() {
    eprintln!("Unix-only test fixture");
    std::process::exit(2);
}

#[cfg(unix)]
fn run_fixture_ui(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    use std::io::Write;
    use tokio::io::{AsyncBufReadExt, BufReader};
    if args.len() != 2 {
        return Err("fixture UI: host profile".into());
    }
    tokio::runtime::Runtime::new()?.block_on(async {
        let mut host = tokio::process::Command::new(&args[0])
            .arg(&args[1])
            .env_clear()
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()?;
        let _input = host.stdin.take().ok_or("control input")?;
        let mut lines = BufReader::new(host.stdout.take().ok_or("control output")?).lines();
        let ready = lines.next_line().await?.ok_or("host did not start")?;
        let ready: serde_json::Value = serde_json::from_str(&ready)?;
        if ready["record"]["phase"] != "ready" {
            return Err("host not ready".into());
        }
        println!("FIXTURE_UI_READY");
        std::io::stdout().flush()?;
        std::future::pending::<()>().await;
        Ok::<_, Box<dyn std::error::Error>>(())
    })
}
