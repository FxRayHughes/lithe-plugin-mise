//! Lithe toolchain provider backed by mise.
//!
//! The process speaks JSON-RPC with `Content-Length` framing on stdio. Each
//! query runs on its own thread so `$/cancelRequest`, `shutdown` and end of
//! input stay responsive while a `mise` child is running.

mod mise;
mod query;

use mise::Options;
use query::{Failure, Query};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROVIDER_ID: &str = "mise";
const EXPERIMENTAL_KEY: &str = "litheToolchains";
const MAX_FRAME_BYTES: usize = 4 * 1024 * 1024;
const MAX_ACTIVE_QUERIES: usize = 16;
const REPLY_BYTE_LIMIT: usize = 1024 * 1024;

type Active = Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>;

fn main() {
    let options = match parse_arguments(std::env::args().skip(1)) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("mise-provider: {message}");
            std::process::exit(2);
        }
    };
    std::process::exit(serve(options));
}

fn parse_arguments(arguments: impl Iterator<Item = String>) -> Result<Options, String> {
    let mut stdio = false;
    let mut options = Options::default();
    for argument in arguments {
        if argument == "--stdio" {
            stdio = true;
        } else if let Some(value) = argument.strip_prefix("--mise=") {
            options.configured = (!value.is_empty()).then(|| value.into());
        } else if let Some(value) = argument.strip_prefix("--offline=") {
            options.offline = match value {
                "true" => true,
                "false" => false,
                _ => return Err(format!("invalid --offline value: {value}")),
            };
        } else if let Some(value) = argument.strip_prefix("--child-timeout-ms=") {
            let milliseconds: u64 = value
                .parse()
                .map_err(|_| format!("invalid --child-timeout-ms value: {value}"))?;
            options.child_timeout = Duration::from_millis(milliseconds);
        } else {
            return Err(format!("unknown argument: {argument}"));
        }
    }
    if stdio {
        Ok(options)
    } else {
        Err("--stdio is required".to_owned())
    }
}

fn serve(options: Options) -> i32 {
    let mut input = BufReader::new(io::stdin());
    let output = Arc::new(Mutex::new(io::stdout()));
    let active: Active = Arc::default();
    let options = Arc::new(options);
    let mut negotiated = false;
    let mut shutting_down = false;
    let status = loop {
        let frame = match read_frame(&mut input) {
            Ok(Some(frame)) => frame,
            // The host went away: nothing may outlive it.
            Ok(None) => break 1,
            Err(error) => {
                eprintln!("mise-provider: {error}");
                break 1;
            }
        };
        let Ok(message) = serde_json::from_slice::<Value>(&frame) else {
            eprintln!("mise-provider: ignored a frame that is not JSON");
            continue;
        };
        let method = message["method"].as_str().unwrap_or("");
        let id = message.get("id").cloned().filter(|id| !id.is_null());
        match (method, id) {
            ("exit", _) => break if shutting_down { 0 } else { 1 },
            ("$/cancelRequest", _) => {
                let key = message["params"]["id"].to_string();
                if let Some(flag) = active.lock().unwrap().get(&key) {
                    flag.store(true, Ordering::SeqCst);
                }
            }
            ("initialize", Some(id)) => {
                negotiated = offered(&message["params"]);
                let capabilities = if negotiated {
                    json!({"experimental": {EXPERIMENTAL_KEY: {"version": 1, "providers": [PROVIDER_ID]}}})
                } else {
                    json!({})
                };
                let result = json!({
                    "capabilities": capabilities,
                    "serverInfo": {"name": "mise-provider", "version": env!("CARGO_PKG_VERSION")},
                });
                write_result(&output, &id, result);
            }
            ("shutdown", Some(id)) => {
                shutting_down = true;
                stop_all(&active);
                write_result(&output, &id, Value::Null);
            }
            ("toolchains/query", Some(id)) => {
                if !negotiated || shutting_down {
                    let failure = Failure::new(
                        -32601,
                        "plugin.toolchain.methodUnavailable",
                        "toolchains/query is not negotiated.",
                    );
                    write_failure(&output, &id, &message["params"], &failure);
                    continue;
                }
                start_query(id, message, &output, &active, &options);
            }
            (_, Some(id)) => {
                let failure = Failure::new(
                    -32601,
                    "plugin.toolchain.methodUnavailable",
                    format!("Unsupported method: {method}"),
                );
                write_failure(&output, &id, &Value::Null, &failure);
            }
            // Other notifications such as `initialized` need no answer.
            (_, None) => {}
        }
    };
    stop_all(&active);
    status
}

/// Whether the host's offer contains protocol version 1 and this provider.
fn offered(params: &Value) -> bool {
    let offer = &params["capabilities"]["experimental"][EXPERIMENTAL_KEY];
    offer["versions"]
        .as_array()
        .is_some_and(|versions| versions.iter().any(|version| version == 1))
        && offer["providers"]
            .as_array()
            .is_some_and(|providers| providers.iter().any(|provider| provider == PROVIDER_ID))
}

fn start_query(
    id: Value,
    message: Value,
    output: &Arc<Mutex<io::Stdout>>,
    active: &Active,
    options: &Arc<Options>,
) {
    let key = id.to_string();
    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let mut queries = active.lock().unwrap();
        let refusal = if queries.contains_key(&key) {
            Some(Failure::new(
                -32602,
                "plugin.toolchain.invalidParams",
                "This request ID is already running.",
            ))
        } else if queries.len() >= MAX_ACTIVE_QUERIES {
            Some(Failure::new(
                -32075,
                "plugin.toolchain.limitExceeded",
                "Too many toolchain queries are running.",
            ))
        } else {
            None
        };
        if let Some(failure) = refusal {
            drop(queries);
            write_failure(output, &id, &message["params"], &failure);
            return;
        }
        queries.insert(key.clone(), cancelled.clone());
    }
    let output = output.clone();
    let active = active.clone();
    let options = options.clone();
    std::thread::spawn(move || {
        let params = &message["params"];
        let outcome = Query::parse(params)
            .and_then(|query| query.run(&options, &cancelled))
            .and_then(|result| {
                // Core refuses an oversized reply as a whole; say so ourselves.
                if result.to_string().len() + 128 > REPLY_BYTE_LIMIT {
                    Err(Failure::new(
                        -32075,
                        "plugin.toolchain.limitExceeded",
                        "The mise reply exceeds the 1 MiB limit.",
                    ))
                } else {
                    Ok(result)
                }
            });
        // Whoever removes the entry owns the one completion of this request.
        active.lock().unwrap().remove(&key);
        match outcome {
            Ok(result) => write_result(&output, &id, result),
            Err(failure) => write_failure(&output, &id, params, &failure),
        }
    });
}

/// Cancels every query and waits, within a bound, for their children to die.
fn stop_all(active: &Active) {
    for flag in active.lock().unwrap().values() {
        flag.store(true, Ordering::SeqCst);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while !active.lock().unwrap().is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn read_frame(input: &mut impl BufRead) -> Result<Option<Vec<u8>>, String> {
    let mut length = None;
    let mut started = false;
    loop {
        let mut line = String::new();
        let count = input
            .take(8 * 1024)
            .read_line(&mut line)
            .map_err(|error| format!("cannot read a frame header: {error}"))?;
        if count == 0 {
            return if started {
                Err("input ended inside a frame header".to_owned())
            } else {
                Ok(None)
            };
        }
        started = true;
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            break;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| format!("malformed frame header: {line}"))?;
        if name.eq_ignore_ascii_case("content-length") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| format!("malformed Content-Length: {value}"))?,
            );
        }
    }
    let length = length.ok_or("a frame has no Content-Length")?;
    if length > MAX_FRAME_BYTES {
        return Err(format!("a {length} byte frame exceeds the limit"));
    }
    let mut body = vec![0; length];
    input
        .read_exact(&mut body)
        .map_err(|error| format!("input ended inside a frame body: {error}"))?;
    Ok(Some(body))
}

fn write_message(output: &Mutex<io::Stdout>, message: &Value) {
    let body = message.to_string();
    let mut output = output.lock().unwrap();
    // A closed stdout means the host left; the read loop then ends the process.
    let _ = write!(output, "Content-Length: {}\r\n\r\n{body}", body.len());
    let _ = output.flush();
}

fn write_result(output: &Mutex<io::Stdout>, id: &Value, result: Value) {
    write_message(
        output,
        &json!({"jsonrpc": "2.0", "id": id, "result": result}),
    );
}

fn write_failure(output: &Mutex<io::Stdout>, id: &Value, params: &Value, failure: &Failure) {
    let operation = params["operationID"]
        .as_str()
        .filter(|value| query::is_identifier(value))
        .unwrap_or("unknown");
    let mut data = json!({
        "code": failure.semantic,
        "operationID": operation,
        "providerID": PROVIDER_ID,
    });
    if let Some(reason) = failure.reason {
        data["details"] = json!({"reason": reason});
    }
    let error = json!({"code": failure.code, "message": failure.message, "data": data});
    write_message(output, &json!({"jsonrpc": "2.0", "id": id, "error": error}));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(bytes: &[u8]) -> Result<Option<Vec<u8>>, String> {
        read_frame(&mut BufReader::new(bytes))
    }

    #[test]
    fn frames_accept_any_header_case_and_extra_headers() {
        let body = frames(b"content-type: x\r\nCONTENT-LENGTH: 2\r\n\r\n{}").unwrap();
        assert_eq!(body.unwrap(), b"{}");
    }

    #[test]
    fn clean_end_of_input_is_not_an_error() {
        assert_eq!(frames(b"").unwrap(), None);
    }

    #[test]
    fn broken_frames_are_errors() {
        assert!(frames(b"Content-Length: 5\r\n\r\n{}").is_err());
        assert!(frames(b"Content-Length: nope\r\n\r\n").is_err());
        assert!(frames(b"X: 1\r\n\r\n").is_err());
        assert!(frames(b"Content-Length: 99999999\r\n\r\n").is_err());
    }

    #[test]
    fn arguments_expand_from_the_manifest_templates() {
        let parse = |values: &[&str]| parse_arguments(values.iter().map(|v| v.to_string()));
        let options = parse(&["--stdio", "--mise=", "--offline=true"]).unwrap();
        assert!(options.configured.is_none() && options.offline);
        let options = parse(&["--stdio", "--mise=/x/mise", "--offline=false"]).unwrap();
        assert_eq!(options.configured.unwrap().to_str(), Some("/x/mise"));
        assert!(!options.offline);
        assert!(parse(&["--mise=/x"]).is_err());
        assert!(parse(&["--stdio", "--offline=maybe"]).is_err());
        assert!(parse(&["--stdio", "--other"]).is_err());
    }

    #[test]
    fn negotiation_needs_version_one_and_this_provider() {
        let offer = |versions: Value, providers: Value| json!({"capabilities": {"experimental": {"litheToolchains": {"versions": versions, "providers": providers}}}});
        assert!(offered(&offer(json!([1, 2]), json!(["mise"]))));
        assert!(!offered(&offer(json!([2]), json!(["mise"]))));
        assert!(!offered(&offer(json!([1]), json!(["other"]))));
        assert!(!offered(&json!({"capabilities": {}})));
    }
}
