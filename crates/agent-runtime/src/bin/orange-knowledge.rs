//! An MCP stdio server for an explicitly selected local collection.
use orange_agent_runtime::workspace::{
    WorkspaceError, WorkspaceGrant,
    knowledge::{KnowledgeCollection, KnowledgeError, KnowledgeRequest},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
};

const LEGACY: &str = "2025-11-25";
const MODERN: &str = "2026-07-28";
const FRAME_LIMIT: u64 = 256 * 1024;

#[derive(Deserialize)]
struct Call {
    name: String,
    arguments: Value,
}
#[derive(PartialEq)]
enum Phase {
    New,
    Negotiated,
    Ready,
}

fn failure(error: KnowledgeError) -> &'static str {
    match error {
        KnowledgeError::Invalid | KnowledgeError::Workspace(WorkspaceError::Invalid) => {
            "INVALID_ARGUMENT"
        }
        KnowledgeError::Workspace(
            WorkspaceError::Forbidden
            | WorkspaceError::EnvironmentChanged
            | WorkspaceError::Revoked,
        ) => "ACCESS_DENIED",
        KnowledgeError::Stale | KnowledgeError::Workspace(WorkspaceError::VersionChanged) => {
            "STALE_CITATION"
        }
        KnowledgeError::Workspace(
            WorkspaceError::Timeout | WorkspaceError::Limit | WorkspaceError::FileTooLarge,
        ) => "RESOURCE_LIMIT",
        _ => "SOURCE_UNAVAILABLE",
    }
}
fn definitions(collection: &str) -> Result<Value, serde_json::Error> {
    let document: Value = serde_json::from_str(orange_protocol::knowledge::SCHEMA)?;
    let mut tools = Vec::new();
    for (name, input, output, description) in [
        (
            "knowledge_search",
            "SearchArguments",
            "SearchOutput",
            "Search the selected UTF-8 documents and return versioned citations.",
        ),
        (
            "knowledge_read",
            "ReadArguments",
            "ReadOutput",
            "Read the current source of an issued citation. Optional byte offsets are relative to the cited chunk.",
        ),
    ] {
        let mut args = document["$defs"][input].clone();
        args["properties"]["collection_id"]["const"] = json!(collection);
        tools.push(json!({"name":name,"description":description,"inputSchema":args,"outputSchema":document["$defs"][output],
                          "annotations":{"readOnlyHint":true,"openWorldHint":false}}));
    }
    Ok(json!({"tools":tools}))
}
fn send(value: &Value) -> io::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    out.write_all(b"\n")?;
    out.flush()
}
fn error(id: &Value, code: i32, message: &str) -> io::Result<()> {
    send(&json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}}))
}
fn reply(id: &Value, mut result: Value, modern: bool) -> io::Result<()> {
    if modern {
        result["resultType"] = json!("complete");
        result["_meta"] = json!({"io.modelcontextprotocol/serverInfo":{"name":"orange-knowledge","version":env!("CARGO_PKG_VERSION")}});
    }
    send(&json!({"jsonrpc":"2.0","id":id,"result":result}))
}
fn notify_progress(token: &Value, done: u8) -> io::Result<()> {
    send(
        &json!({"jsonrpc":"2.0","method":"notifications/progress","params":{"progressToken":token,"progress":done,"total":1}}),
    )
}
fn allowed_params(params: &Value, keys: &[&str]) -> bool {
    params
        .as_object()
        .is_some_and(|p| p.keys().all(|k| keys.contains(&k.as_str()) || k == "_meta"))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.first().is_some_and(|x| x == "--withdraw-saved") {
        if !(4..=5).contains(&args.len()) {
            return Err(
                "usage: orange-knowledge --withdraw-saved OWNER COLLECTION PRIVATE_DIRECTORY [AUTHORITY_DIRECTORY]"
                    .into(),
            );
        }
        let owner = args[1].to_str().ok_or("invalid owner")?;
        let collection = args[2].to_str().ok_or("invalid collection")?;
        orange_agent_runtime::workspace::knowledge::withdraw_closed_with_authority(
            std::path::Path::new(&args[3]),
            std::path::Path::new(args.get(4).unwrap_or(&args[3])),
            owner,
            collection,
        )?;
        println!(
            "{}",
            json!({"collection_id":collection,"state":"withdrawn"})
        );
        return Ok(());
    }
    let version = if args.first().is_some_and(|x| x == "--protocol") {
        match args.get(1).and_then(|x| x.to_str()) {
            Some(LEGACY) => LEGACY,
            Some(MODERN) => MODERN,
            _ => return Err("unsupported knowledge stdio profile".into()),
        }
    } else {
        LEGACY
    };
    if args.first().is_some_and(|x| x == "--protocol") {
        args.drain(..2);
    }
    let modern = version == MODERN;
    let persistent = args.first().is_some_and(|x| x == "--persist");
    if persistent {
        args.remove(0);
    }
    let authority = if persistent && args.first().is_some_and(|x| x == "--authority-dir") {
        let dir = PathBuf::from(args.get(1).ok_or("missing authority directory")?);
        args.drain(..2);
        Some(dir)
    } else {
        None
    };
    if !(5..=36).contains(&args.len()) {
        return Err(
            "usage: orange-knowledge [--protocol VERSION] [--persist [--authority-dir PATH]] OWNER COLLECTION WORKSPACE PRIVATE_DIRECTORY RELATIVE_FILE..."
                .into(),
        );
    }
    let owner = args[0].to_str().ok_or("invalid owner")?;
    let collection = args[1].to_str().ok_or("invalid collection")?;
    let workspace = PathBuf::from(&args[2]);
    let private = PathBuf::from(&args[3]);
    let files = args[4..]
        .iter()
        .map(|s| {
            s.to_str()
                .map(str::to_owned)
                .ok_or("invalid file selection")
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut protected = vec![private.clone()];
    if let Some(dir) = &authority
        && dir != &private
    {
        protected.push(dir.clone());
    }
    let grant = WorkspaceGrant::open(
        owner,
        collection,
        "selected-knowledge-v1",
        &workspace,
        &protected,
    )?;
    let search = if persistent {
        KnowledgeCollection::open_persistent_with_authority(
            grant.clone(),
            collection,
            files,
            &private,
            authority.as_deref().unwrap_or(&private),
        )?
    } else {
        KnowledgeCollection::open(grant.clone(), collection, files)?
    };
    let (tx, mut frames) = tokio::sync::mpsc::channel(2);
    std::thread::spawn(move || {
        let mut input = BufReader::new(io::stdin().lock());
        loop {
            let mut line = Vec::new();
            let result = Read::by_ref(&mut input)
                .take(FRAME_LIMIT + 1)
                .read_until(b'\n', &mut line);
            if matches!(result, Ok(0)) {
                break;
            }
            let done = result.is_err() || line.len() as u64 > FRAME_LIMIT;
            if tx.blocking_send(result.map(|_| line)).is_err() || done {
                break;
            }
        }
    });
    let tools = definitions(collection)?;
    let mut phase = Phase::New;
    let mut jobs = tokio::task::JoinSet::<io::Result<()>>::new();
    let mut running = None::<(Value, tokio::task::AbortHandle)>;
    loop {
        let frame = tokio::select! {
            biased;
            result=jobs.join_next(),if !jobs.is_empty()=>{
                if let Some(result)=result {
                    if let Some((_,h))=&running && h.is_finished(){running=None;}
                    match result{Ok(r)=>r?,Err(e) if e.is_cancelled()=>{},Err(e)=>return Err(e.into())}
                }
                continue;
            }
            frame=frames.recv()=>frame
        };
        let Some(line) = frame else {
            break;
        };
        let line = line?;
        if line.len() as u64 > FRAME_LIMIT {
            error(&Value::Null, -32600, "Request too large")?;
            break;
        }
        use orange_protocol::canonical::{CanonicalError, parse_tool_arguments};
        let parsed = std::str::from_utf8(&line)
            .map_err(|_| CanonicalError::InvalidJson)
            .and_then(parse_tool_arguments);
        let msg = match parsed {
            Ok(value) => value,
            Err(CanonicalError::InvalidArguments) => {
                error(&Value::Null, -32600, "Ambiguous or invalid request")?;
                continue;
            }
            Err(_) => {
                error(&Value::Null, -32700, "Parse error")?;
                continue;
            }
        };
        if !msg.as_object().is_some_and(|v| {
            v.keys()
                .all(|k| matches!(k.as_str(), "jsonrpc" | "id" | "method" | "params"))
        }) {
            error(&Value::Null, -32600, "Invalid request envelope")?;
            continue;
        }
        let id = &msg["id"];
        let Some(method) = msg["method"].as_str().filter(|_| msg["jsonrpc"] == "2.0") else {
            error(&Value::Null, -32600, "Invalid request")?;
            continue;
        };
        let empty = json!({});
        let params = msg.get("params").unwrap_or(&empty);
        if msg.get("id").is_none() {
            if method == "notifications/cancelled"
                && let Some((active, handle)) = &running
                && active == &params["requestId"]
            {
                handle.abort();
                running = None;
            }
            if method == "notifications/initialized"
                && phase == Phase::Negotiated
                && allowed_params(params, &[])
            {
                phase = Phase::Ready;
            }
            continue;
        }
        if !(id.is_i64() || id.is_u64() || id.is_string()) {
            error(&Value::Null, -32600, "Invalid request id")?;
            continue;
        }
        if running.as_ref().is_some_and(|(active, _)| active == id) {
            error(&Value::Null, -32600, "Duplicate outstanding request id")?;
            continue;
        }
        if modern {
            let meta = &params["_meta"];
            let Some(requested) = meta["io.modelcontextprotocol/protocolVersion"].as_str() else {
                error(id, -32602, "Missing per-request protocol version")?;
                continue;
            };
            if requested != MODERN {
                send(
                    &json!({"jsonrpc":"2.0","id":id,"error":{"code":-32022,"message":"Unsupported protocol version","data":{"supported":[MODERN],"requested":requested}}}),
                )?;
                continue;
            }
            if !meta["io.modelcontextprotocol/clientCapabilities"].is_object() {
                error(id, -32602, "Missing per-request client capabilities")?;
                continue;
            }
        } else if params
            .pointer("/_meta/io.modelcontextprotocol~1protocolVersion")
            .is_some()
        {
            error(
                id,
                -32602,
                "This process uses the initialization-based profile",
            )?;
            continue;
        }
        match method {
            "initialize" if !modern && phase == Phase::New => {
                if params["protocolVersion"] != LEGACY
                    || !params["capabilities"].is_object()
                    || !params["clientInfo"]["name"].is_string()
                    || !params["clientInfo"]["version"].is_string()
                {
                    error(
                        id,
                        -32602,
                        "This server requires the 2025-11-25 stdio profile",
                    )?;
                    continue;
                }
                phase = Phase::Negotiated;
                reply(
                    id,
                    json!({"protocolVersion":LEGACY,"capabilities":{"tools":{"listChanged":false}},
                                "serverInfo":{"name":"orange-knowledge","version":env!("CARGO_PKG_VERSION")}}),
                    false,
                )?;
            }
            "server/discover" if modern && allowed_params(params, &[]) => reply(
                id,
                json!({"supportedVersions":[MODERN],"capabilities":{"tools":{}},"ttlMs":0,"cacheScope":"private"}),
                true,
            )?,
            "ping" => reply(id, json!({}), modern)?,
            _ if !modern && phase != Phase::Ready => {
                error(id, -32600, "Initialize the connection first")?
            }
            "tools/list" if allowed_params(params, &[]) => {
                let mut result = tools.clone();
                if modern {
                    result["ttlMs"] = json!(0);
                    result["cacheScope"] = json!("private");
                }
                reply(id, result, modern)?;
            }
            "tools/list" => error(id, -32602, "Invalid list parameters")?,
            "tools/call" => {
                if !allowed_params(params, &["name", "arguments"]) {
                    error(id, -32602, "Invalid tool parameters")?;
                    continue;
                }
                let Ok(call) = serde_json::from_value::<Call>(params.clone()) else {
                    error(id, -32602, "Invalid tool parameters")?;
                    continue;
                };
                if !matches!(call.name.as_str(), "knowledge_search" | "knowledge_read") {
                    error(id, -32602, "Unknown tool")?;
                    continue;
                }
                if !call
                    .arguments
                    .as_object()
                    .is_some_and(|args| args.values().all(|v| !v.is_null()))
                {
                    error(id, -32602, "Invalid tool arguments")?;
                    continue;
                }
                let Ok(request) = serde_json::from_value::<KnowledgeRequest>(
                    json!({"tool":call.name,"arguments":call.arguments}),
                ) else {
                    error(id, -32602, "Invalid tool arguments")?;
                    continue;
                };
                if running.is_some() {
                    error(id, -32002, "Another tool request is in progress")?;
                    continue;
                }
                let progress = params.pointer("/_meta/progressToken").cloned();
                if progress
                    .as_ref()
                    .is_some_and(|v| !v.is_string() && !v.is_i64() && !v.is_u64())
                {
                    error(id, -32602, "Invalid progress token")?;
                    continue;
                }
                if let Some(t) = &progress {
                    notify_progress(t, 0)?;
                }
                let response_id = id.clone();
                let worker = search.clone();
                let authority = grant.clone();
                let handle=jobs.spawn(async move{
                    let receipt_id=response_id.clone();
                    let result=worker.execute(authority.owner(),authority.environment(),authority.policy(),request,move|result|{
                        let data=serde_json::to_value(result)?;
                        if let Some(token)=progress{notify_progress(&token,1)?;}
                        reply(&receipt_id,json!({"content":[{"type":"text","text":data.to_string()}],"structuredContent":data,"isError":false}),modern)
                    }).await;
                    match result{Ok(written)=>written,Err(e)=>reply(&response_id,json!({"content":[{"type":"text","text":failure(e)}],"isError":true}),modern)}
                });
                running = Some((id.clone(), handle));
            }
            _ => error(id, -32601, "Method not found")?,
        }
    }
    Ok(())
}
