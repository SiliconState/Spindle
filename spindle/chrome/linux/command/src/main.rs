//! `spindle`: Spindle's calls from bash (and any program) in the browser Linux. Each call
//! goes to the runner agent of the command running it (/run/spindle.sock, one JSON line
//! each way), which asks Spindle; Spindle journals every call before it runs, so one cut
//! off is never sent again by itself (its error then says [may_have_run]). Node's
//! `spindle` module runs this command; Python's (spindle.py) asks the agent itself.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::ExitCode;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

const SOCKET: &str = "/run/spindle.sock";

const USAGE: &str = "spindle: Spindle's calls from bash (and any program), journaled by Spindle.

  spindle model [--provider P] [--model M] [--system S] [--max-tokens N] [--effort E]
                [--image FILE]... PROMPT...
                                prints the model's text (PROMPT - reads standard input);
                                --image shows it a picture (PNG, JPEG, GIF, WebP)
  spindle tool NAME [JSON]      calls a tool (read_page, list_tabs, an app's tools)
  spindle search [QUERY]        the tools there are
  spindle describe NAME         one tool and its input
  spindle limits                this command's call limits
  spindle http [-X METHOD] [-H 'NAME: VALUE']... [-d BODY | --json JSON] URL
                                a request to a site the user granted (Settings, Commands);
                                prints the body, and fails on HTTP 400 and up
  spindle call METHOD [JSON]    any call, as its reply in JSON (JSON - reads standard input)";

enum Failure {
    /// A wrong use, said as it is.
    Said(String),
    /// A call that did not go through, or failed.
    Call { message: String, may_have_run: bool },
}

type Result<T> = std::result::Result<T, Failure>;

fn failed(message: impl Into<String>) -> Failure {
    Failure::Call {
        message: message.into(),
        may_have_run: false,
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(Failure::Said(message)) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
        Err(Failure::Call {
            message,
            may_have_run,
        }) => {
            eprintln!(
                "spindle: {message}{}",
                if may_have_run { " [may_have_run]" } else { "" }
            );
            ExitCode::from(1)
        }
    }
}

fn run(args: &[String]) -> Result<u8> {
    let Some((command, rest)) = args.split_first() else {
        put(format!("{USAGE}\n").as_bytes())?;
        return Ok(2);
    };
    match (command.as_str(), rest) {
        ("-h" | "--help", _) => put(format!("{USAGE}\n").as_bytes())?,
        ("model", rest) => model(rest)?,
        ("tool", [name, rest @ ..]) => {
            let arguments = match rest.first() {
                Some(given) => parse(given, "tool")?,
                None => json!({}),
            };
            let arguments = if truthy(&arguments) {
                arguments
            } else {
                json!({})
            };
            text(&call(
                "tool",
                json!({"name": name, "arguments": arguments}),
            )?)?;
        }
        ("search", rest) => text(&call("search", json!({"query": rest.join(" ")}))?)?,
        ("describe", [name, ..]) => text(&call("describe", json!({"name": name}))?)?,
        ("limits", _) => text(&call("limits", json!({}))?)?,
        ("http", rest) if !rest.is_empty() => return http(rest),
        ("call", [method, rest @ ..]) => {
            let given = rest.first().map_or("{}", String::as_str);
            let input = if given == "-" {
                parse(&stdin()?, "call")?
            } else {
                parse(given, "call")?
            };
            put(format!("{}\n", ask(method, input)?).as_bytes())?;
        }
        _ => {
            eprintln!("{USAGE}");
            return Ok(2);
        }
    }
    Ok(0)
}

fn model(mut rest: &[String]) -> Result<()> {
    let mut options = Map::new();
    let mut images = Vec::new();
    while let Some(flag) = rest.first() {
        let name = match flag.as_str() {
            "--provider" => "provider",
            "--model" => "model",
            "--system" => "system",
            "--max-tokens" => "max_tokens",
            "--effort" => "effort",
            "--image" => "images",
            _ => break,
        };
        let Some(value) = rest.get(1) else {
            return Err(Failure::Said(format!("{flag} needs a value")));
        };
        rest = &rest[2..];
        match name {
            "images" => images.push(image(value)?),
            "max_tokens" => {
                let tokens: u64 = value
                    .parse()
                    .ok()
                    .filter(|_| value.bytes().all(|byte| byte.is_ascii_digit()))
                    .ok_or_else(|| {
                        Failure::Said(format!(
                            "spindle: --max-tokens takes a number, not '{value}'"
                        ))
                    })?;
                options.insert(name.into(), json!(tokens));
            }
            _ => {
                options.insert(name.into(), json!(value));
            }
        }
    }
    let prompt = if rest == ["-"] {
        stdin()?
    } else {
        rest.join(" ")
    };
    let mut input = Map::new();
    input.insert("prompt".into(), json!(prompt));
    input.extend(options);
    if !images.is_empty() {
        input.insert("images".into(), Value::Array(images));
    }
    let reply = call("model", Value::Object(input))?;
    match reply.get("text") {
        Some(Value::String(said)) => put(format!("{said}\n").as_bytes()),
        _ => Err(failed("the model's reply has no text")),
    }
}

fn http(mut rest: &[String]) -> Result<u8> {
    let mut options = Map::new();
    let mut headers = Map::new();
    while let [flag, value, ..] = rest {
        match flag.as_str() {
            "-H" => {
                let (name, given) = value.split_once(':').unwrap_or((value, ""));
                headers.insert(name.trim().into(), json!(given.trim()));
            }
            "-X" => {
                options.insert("method".into(), json!(value));
            }
            "-d" => {
                options.insert("body".into(), json!(value));
            }
            "--json" => {
                options.insert("json".into(), parse(value, "http --json")?);
            }
            _ => break,
        }
        rest = &rest[2..];
    }
    let [url] = rest else {
        return Err(Failure::Said(
            "spindle http: one URL, after the options".into(),
        ));
    };
    if !headers.is_empty() {
        options.insert("headers".into(), Value::Object(headers));
    }
    options.insert("url".into(), json!(url));
    let data = call("http", Value::Object(options))?;
    // The body as it came (to pipe or save); what is said about it, on a line of its own.
    let ended = if let Some(parsed) = data.get("json") {
        text(parsed)?;
        true
    } else if let Some(encoded) = data.get("body_base64") {
        let body = STANDARD
            .decode(encoded.as_str().unwrap_or_default())
            .map_err(|error| failed(format!("the body did not read: {error}")))?;
        put(&body)?;
        body.ends_with(b"\n")
    } else {
        let body = data.get("body").and_then(Value::as_str).unwrap_or_default();
        put(body.as_bytes())?;
        body.ends_with('\n')
    };
    let status = &data["status"];
    let refused = status.as_f64().is_some_and(|code| code >= 400.0);
    let mut said = Vec::new();
    if data.get("truncated").is_some_and(truthy) {
        said.push("the body was cut short".to_string());
    }
    if refused {
        said.push(format!("HTTP {status}"));
    }
    if !said.is_empty() {
        eprintln!(
            "{}spindle http: {}",
            if ended { "" } else { "\n" },
            said.join("; ")
        );
    }
    Ok(u8::from(refused))
}

/// A PNG, JPEG, GIF or WebP file, as a model call's image.
fn image(path: &str) -> Result<Value> {
    let data = fs::read(path).map_err(|error| failed(format!("image: {path}: {error}")))?;
    let kind = if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        "image/png"
    } else if data.starts_with(b"\xff\xd8\xff") {
        "image/jpeg"
    } else if data.starts_with(b"GIF8") {
        "image/gif"
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        "image/webp"
    } else {
        return Err(failed(format!(
            "image: {path} is not PNG, JPEG, GIF or WebP"
        )));
    };
    Ok(json!({"media_type": kind, "data": STANDARD.encode(&data)}))
}

/// One call's reply from the agent, as it came ({ok, data | error, may_have_run}).
fn ask(method: &str, input: Value) -> Result<Value> {
    let mut conn = UnixStream::connect(SOCKET)
        .map_err(|error| failed(format!("Spindle is not reachable from here ({error})")))?;
    let closed = |error: Option<io::Error>| Failure::Call {
        message: match error {
            Some(error) => format!("the runner closed the connection ({error})"),
            None => "the runner closed the connection".into(),
        },
        may_have_run: true,
    };
    let mut line = json!({"method": method, "input": input})
        .to_string()
        .into_bytes();
    line.push(b'\n');
    conn.write_all(&line).map_err(|error| closed(Some(error)))?;
    let mut data = Vec::new();
    let mut chunk = vec![0; 65536];
    while !data.ends_with(b"\n") {
        match conn.read(&mut chunk) {
            Ok(0) => return Err(closed(None)),
            Ok(read) => data.extend_from_slice(&chunk[..read]),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(closed(Some(error))),
        }
    }
    serde_json::from_slice(&data).map_err(|error| Failure::Call {
        message: format!("the runner's reply did not read ({error})"),
        may_have_run: true,
    })
}

/// A call's data, or its failure.
fn call(method: &str, input: Value) -> Result<Value> {
    let mut reply = ask(method, input)?;
    if !reply.get("ok").is_some_and(truthy) {
        let message = match reply.get("error") {
            Some(Value::String(error)) if !error.is_empty() => error.clone(),
            _ => "the call failed".into(),
        };
        return Err(Failure::Call {
            message,
            may_have_run: reply.get("may_have_run").is_some_and(truthy),
        });
    }
    Ok(reply
        .get_mut("data")
        .map(Value::take)
        .unwrap_or(Value::Null))
}

/// Text as it is; anything else as indented JSON.
fn text(data: &Value) -> Result<()> {
    match data {
        Value::String(said) => put(format!("{said}\n").as_bytes()),
        _ => put(format!("{:#}\n", data).as_bytes()),
    }
}

/// Writes to standard output; a reader that went away (`| head`) ends it quietly.
fn put(bytes: &[u8]) -> Result<()> {
    let mut out = io::stdout().lock();
    match out.write_all(bytes).and_then(|()| out.flush()) {
        Err(error) if error.kind() != io::ErrorKind::BrokenPipe => Err(failed(error.to_string())),
        _ => Ok(()),
    }
}

fn stdin() -> Result<String> {
    let mut bytes = Vec::new();
    io::stdin()
        .read_to_end(&mut bytes)
        .map_err(|error| failed(format!("standard input: {error}")))?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

fn parse(given: &str, what: &str) -> Result<Value> {
    serde_json::from_str(given)
        .map_err(|error| Failure::Said(format!("spindle {what}: not JSON ({error})")))
}

/// As Python reads a value as true or false (the agent and Spindle write JSON that way).
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(on) => *on,
        Value::Number(number) => number.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}
