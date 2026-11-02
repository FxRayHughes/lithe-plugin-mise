//! Drives the built backend over stdio against a scripted fake `mise`.

use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const FAKE_MISE: &str = r#"#!/bin/sh
dir="$FAKE_MISE_DIR"
printf '%s|%s|%s\n' "$PWD" "$MISE_OFFLINE" "$*" >> "$dir/calls.log"
if [ -f "$dir/hang" ]; then
  echo $$ > "$dir/hang.pid"
  exec sleep 30
fi
if [ -f "$PWD/.untrusted" ]; then
  echo "mise ERROR error parsing config file: $PWD/mise.toml" >&2
  echo "mise ERROR Config files in $PWD/mise.toml are not trusted." >&2
  echo "Trust them with \`mise trust\`." >&2
  exit 1
fi
case "$*" in
  "ls --json --local") cat "$dir/local.json" ;;
  "ls --json --installed") cat "$dir/installed.json" ;;
  "ls --json --global") cat "$dir/global.json" ;;
  bin-paths*) cat "$dir/bin-paths.txt" ;;
  *) echo "unexpected: $*" >&2; exit 3 ;;
esac
"#;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir()
            .join(format!(
                "mise-provider-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ))
            .canonicalize_or_create();
        let fixture = Self { root };
        let installs = fixture.path("home/.local/share/mise/installs");
        for (tool, version, program) in [
            ("java", "21.0.2", "bin/java"),
            ("java", "17.0.2", "bin/java"),
            ("maven", "3.9.16", "apache-maven-3.9.16/bin/mvn"),
            ("pnpm", "12.4.1", "pnpm"),
        ] {
            fixture.executable(&installs.join(tool).join(version).join(program));
        }
        std::os::unix::fs::symlink("./21.0.2", installs.join("java/latest")).unwrap();
        // A broken install without its program is never reported.
        fs::create_dir_all(installs.join("java/22.0.0")).unwrap();
        fixture.write("home/.config/mise/config.toml", "[tools]\njava = \"21\"\n");
        fixture.write(
            "project/mise.toml",
            "[tools]\njava = \"17\"\nnode = \"99\"\n",
        );
        fixture.write("project/app/.tool-versions", "maven 3\n");
        fixture.write("untrusted/.untrusted", "");
        fixture.write("untrusted/mise.toml", "[env]\nA = \"b\"\n");
        let home = fixture.path("home");
        let install =
            |tool: &str, version: &str| installs.join(tool).join(version).display().to_string();
        let source = |file: &str| json!({"type": "mise.toml", "path": fixture.path(file)});
        fixture.write_json(
            "fake/installed.json",
            json!({
                "java": [
                    {"version": "17.0.2", "install_path": install("java", "17.0.2"), "installed": true},
                    {"version": "21.0.2", "install_path": install("java", "21.0.2"), "installed": true},
                    {"version": "latest", "install_path": install("java", "latest"), "installed": true},
                    {"version": "22.0.0", "install_path": install("java", "22.0.0"), "installed": true}
                ],
                "maven": [{"version": "3.9.16", "install_path": install("maven", "3.9.16"), "installed": true}],
                "pnpm": [{"version": "12.4.1", "install_path": install("pnpm", "12.4.1"), "installed": true}],
                "rust": [{"version": "stable", "install_path": install("rust", "stable"), "installed": true}]
            }),
        );
        fixture.write_json(
            "fake/global.json",
            json!({"java": [{"version": "21.0.2", "requested_version": "21", "install_path": install("java", "21.0.2"),
                "source": source("home/.config/mise/config.toml"), "installed": true}]}),
        );
        fixture.write_json(
            "fake/local.json",
            json!({
                "java": [{"version": "17.0.2", "requested_version": "17", "install_path": install("java", "17.0.2"),
                    "source": source("project/mise.toml"), "installed": true}],
                "maven": [{"version": "3.9.16", "requested_version": "3", "install_path": install("maven", "3.9.16"),
                    "source": {"type": ".tool-versions", "path": fixture.path("project/app/.tool-versions")}, "installed": true}],
                "node": [
                    {"version": "99", "requested_version": "99", "install_path": installs.join("node/99"),
                        "source": source("project/mise.toml"), "installed": false},
                    {"version": "20", "requested_version": "20", "install_path": installs.join("node/20"),
                        "source": {"type": "environment", "key": "MISE_NODE_VERSION", "value": "20"}, "installed": false}
                ],
                "go": [{"version": "1.0", "requested_version": "1.0", "install_path": installs.join("go/1.0"),
                    "source": source("project/unrelated.toml"), "installed": false}]
            }),
        );
        fixture.write(
            "fake/bin-paths.txt",
            &format!(
                "{}\n",
                installs
                    .join("maven/3.9.16/apache-maven-3.9.16/bin")
                    .display()
            ),
        );
        let mise = fixture.path("fake/mise");
        fixture.write("fake/mise", FAKE_MISE);
        fs::set_permissions(&mise, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(home.is_dir());
        fixture
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, contents: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    fn write_json(&self, relative: &str, value: Value) {
        self.write(relative, &value.to_string());
    }

    fn executable(&self, path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn calls(&self) -> String {
        fs::read_to_string(self.path("fake/calls.log")).unwrap_or_default()
    }

    fn start(&self, extra: &[String]) -> Backend {
        let mut arguments = vec!["--stdio".to_owned(), "--offline=true".to_owned()];
        arguments.extend(extra.iter().cloned());
        if !extra.iter().any(|argument| argument.starts_with("--mise=")) {
            arguments.push(format!("--mise={}", self.path("fake/mise").display()));
        }
        let mut child = Command::new(env!("CARGO_BIN_EXE_mise-provider"))
            .args(&arguments)
            .env_clear()
            .env("HOME", self.path("home"))
            .env("PATH", "/usr/bin:/bin")
            .env("FAKE_MISE_DIR", self.path("fake"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Backend {
            child,
            input: Some(input),
            output,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

trait CanonicalizeOrCreate {
    fn canonicalize_or_create(self) -> PathBuf;
}

impl CanonicalizeOrCreate for PathBuf {
    fn canonicalize_or_create(self) -> PathBuf {
        fs::create_dir_all(&self).unwrap();
        self.canonicalize().unwrap()
    }
}

struct Backend {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Backend {
    fn send(&mut self, message: Value) {
        let body = message.to_string();
        let input = self.input.as_mut().unwrap();
        write!(input, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
        input.flush().unwrap();
    }

    fn receive(&mut self) -> Value {
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(
                self.output.read_line(&mut line).unwrap() > 0,
                "backend closed stdout"
            );
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            if let Some(value) = line.strip_prefix("Content-Length: ") {
                length = value.parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        self.output.read_exact(&mut body).unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    fn initialize(&mut self) -> Value {
        self.send(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"processId": null,
            "capabilities": {"experimental": {"litheToolchains": {"versions": [1], "providers": ["mise"]}}}}}));
        let reply = self.receive();
        self.send(json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}));
        reply
    }

    fn query(&mut self, id: u64, workspace: Value, tools: &[&str]) -> Value {
        self.send(request(id, workspace, tools));
        self.receive()
    }

    fn stop(mut self) -> i32 {
        self.send(json!({"jsonrpc": "2.0", "id": 999, "method": "shutdown"}));
        assert_eq!(self.receive()["result"], Value::Null);
        self.send(json!({"jsonrpc": "2.0", "method": "exit"}));
        drop(self.input.take());
        wait(&mut self.child, Duration::from_secs(5)).expect("backend exits after exit")
    }
}

fn wait(child: &mut Child, limit: Duration) -> Option<i32> {
    let deadline = Instant::now() + limit;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().unwrap() {
            return Some(status.code().unwrap_or(-1));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let _ = child.kill();
    None
}

fn request(id: u64, workspace: Value, tools: &[&str]) -> Value {
    let context = if workspace.is_null() {
        Value::Null
    } else {
        json!({"contextID": "ctx", "generation": 1})
    };
    json!({"jsonrpc": "2.0", "id": id, "method": "toolchains/query", "params": {
        "operationID": format!("op-{id}"), "generation": 3, "providerID": "mise",
        "authority": {"kind": "local"}, "context": context, "workspace": workspace, "tools": tools}})
}

fn workspace(fixture: &Fixture, root: &str, directory: &str) -> Value {
    json!({"rootID": "root-1", "root": fixture.path(root), "directory": directory})
}

fn by_tool<'a>(items: &'a Value, tool: &str) -> Vec<&'a Value> {
    items
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["tool"] == tool)
        .collect()
}

fn installed(fixture: &Fixture, tail: &str) -> String {
    fixture
        .path("home/.local/share/mise/installs")
        .join(tail)
        .display()
        .to_string()
}

#[test]
fn negotiation_only_declares_an_offered_provider() {
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[]);
    let reply = backend.initialize();
    assert_eq!(
        reply["result"]["capabilities"]["experimental"]["litheToolchains"],
        json!({"version": 1, "providers": ["mise"]})
    );
    assert_eq!(backend.stop(), 0);

    let mut backend = fixture.start(&[]);
    backend.send(
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"capabilities":
        {"experimental": {"litheToolchains": {"versions": [2], "providers": ["mise"]}}}}}),
    );
    let reply = backend.receive();
    assert!(reply["result"]["capabilities"]
        .get("experimental")
        .is_none());
    let refused = backend.query(2, Value::Null, &["java"]);
    assert_eq!(refused["error"]["code"], -32601);
    assert_eq!(backend.stop(), 0);
    assert_eq!(fixture.calls(), "", "no mise call before negotiation");
}

#[test]
fn global_query_reports_installations_and_global_statements() {
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[]);
    backend.initialize();
    let reply = backend.query(7, Value::Null, &["java", "maven", "pnpm", "node"]);
    let result = &reply["result"];
    assert_eq!(
        result.as_object().unwrap().keys().collect::<Vec<_>>(),
        [
            "generation",
            "installations",
            "operationID",
            "providerID",
            "statements",
            "watch"
        ]
    );
    assert_eq!(result["operationID"], "op-7");
    assert_eq!(result["generation"], 3);
    let java = by_tool(&result["installations"], "java");
    // `latest` is an alias of 21.0.2 and 22.0.0 has no bin/java.
    assert_eq!(java.len(), 2, "{java:?}");
    assert!(java
        .iter()
        .all(|item| item["version"] != "latest" && item["version"] != "22.0.0"));
    let maven = by_tool(&result["installations"], "maven");
    assert_eq!(
        maven[0]["home"],
        installed(&fixture, "maven/3.9.16/apache-maven-3.9.16")
    );
    assert_eq!(
        by_tool(&result["installations"], "pnpm")[0]["home"],
        installed(&fixture, "pnpm/12.4.1")
    );
    assert!(
        by_tool(&result["installations"], "rust").is_empty(),
        "unrequested tools stay out"
    );
    let statements = result["statements"].as_array().unwrap();
    assert_eq!(statements.len(), 1);
    assert_eq!(statements[0]["scope"], "global");
    assert_eq!(statements[0]["reader"], "native");
    assert_eq!(
        statements[0]["installation"],
        installed(&fixture, "java/21.0.2")
    );
    let watch: Vec<&str> = result["watch"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(watch.contains(
        &fixture
            .path("home/.config/mise/config.toml")
            .to_str()
            .unwrap()
    ));
    assert!(watch.contains(
        &fixture
            .path("home/.local/share/mise/installs")
            .to_str()
            .unwrap()
    ));
    let calls = fixture.calls();
    assert!(!calls.contains("--local"), "{calls}");
    assert!(
        calls.lines().all(
            |line| line.starts_with(fixture.path("home").to_str().unwrap()) && line.contains("|1|")
        ),
        "{calls}"
    );
    assert!(
        !calls.contains("trust") && !calls.contains("install "),
        "{calls}"
    );
    assert_eq!(backend.stop(), 0);
}

#[test]
fn workspace_query_reports_project_requests_with_sources_and_readers() {
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[]);
    backend.initialize();
    let reply = backend.query(
        8,
        workspace(&fixture, "project", "app"),
        &["java", "maven", "node", "go"],
    );
    let statements = &reply["result"]["statements"];
    let project: Vec<&Value> = statements
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["scope"] == "project")
        .collect();
    let java = project.iter().find(|s| s["tool"] == "java").unwrap();
    assert_eq!(
        java["source"],
        fixture.path("project/mise.toml").display().to_string()
    );
    assert_eq!(java["installation"], installed(&fixture, "java/17.0.2"));
    let maven = project.iter().find(|s| s["tool"] == "maven").unwrap();
    assert_eq!(maven["reader"], "compatible");
    assert_eq!(
        maven["installation"],
        installed(&fixture, "maven/3.9.16/apache-maven-3.9.16")
    );
    let node: Vec<&&Value> = project.iter().filter(|s| s["tool"] == "node").collect();
    assert_eq!(
        node.len(),
        1,
        "the environment request has no file and is skipped"
    );
    assert_eq!(node[0]["request"], "99");
    assert!(node[0].get("installation").is_some() && node[0]["installation"].is_null());
    assert!(
        project.iter().all(|s| s["tool"] != "go"),
        "unmatched source is dropped"
    );
    let homes: Vec<&Value> = reply["result"]["installations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| &i["home"])
        .collect();
    for statement in statements.as_array().unwrap() {
        if let Some(home) = statement["installation"].as_str() {
            assert!(homes.iter().any(|h| *h == home), "{home} must be listed");
        }
    }
    let calls = fixture.calls();
    assert!(
        calls.contains(&format!(
            "{}|1|ls --json --local",
            fixture.path("project/app").display()
        )),
        "{calls}"
    );
    assert_eq!(backend.stop(), 0);
}

#[test]
fn a_community_tool_id_gets_the_same_maven_answer() {
    const COMMUNITY_MAVEN: &str = "io.github.lithe-idea.maven-build:maven";
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[]);
    backend.initialize();
    let reply = backend.query(
        8,
        workspace(&fixture, "project", "app"),
        &[COMMUNITY_MAVEN, "maven"],
    );
    let result = &reply["result"];
    let home = installed(&fixture, "maven/3.9.16/apache-maven-3.9.16");
    for tool in [COMMUNITY_MAVEN, "maven"] {
        let installations = by_tool(&result["installations"], tool);
        assert_eq!(installations.len(), 1, "{tool}: {installations:?}");
        assert_eq!(installations[0]["home"], home);
        let statement = by_tool(&result["statements"], tool)
            .into_iter()
            .find(|s| s["scope"] == "project")
            .unwrap();
        assert_eq!(statement["request"], "3");
        assert_eq!(statement["installation"], home);
    }
    let calls = fixture.calls();
    assert_eq!(
        calls.matches("maven@3.9.16").count(),
        1,
        "one install is asked about once: {calls}"
    );
    let reply = backend.query(9, Value::Null, &[COMMUNITY_MAVEN]);
    assert!(by_tool(&reply["result"]["installations"], "maven").is_empty());
    assert_eq!(
        by_tool(&reply["result"]["installations"], COMMUNITY_MAVEN).len(),
        1
    );
    assert_eq!(backend.stop(), 0);
}

#[test]
fn untrusted_workspace_fails_but_global_queries_still_work() {
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[]);
    backend.initialize();
    let reply = backend.query(9, workspace(&fixture, "untrusted", "."), &["java"]);
    let error = &reply["error"];
    assert_eq!(error["code"], -32073);
    assert_eq!(
        error["data"],
        json!({"code": "plugin.toolchain.providerUnavailable", "operationID": "op-9", "providerID": "mise",
            "details": {"reason": "untrusted"}})
    );
    let message = error["message"].as_str().unwrap();
    assert!(
        message.contains("mise trust") && message.contains("untrusted/mise.toml"),
        "{message}"
    );
    let global = backend.query(10, Value::Null, &["java"]);
    assert!(global["result"]["installations"].as_array().unwrap().len() >= 2);
    assert!(!fixture.calls().contains(" trust"));
    assert_eq!(backend.stop(), 0);
}

#[test]
fn missing_mise_is_a_provider_error_and_a_bad_setting_never_falls_back() {
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[format!("--mise={}", fixture.path("nope/mise").display())]);
    backend.initialize();
    let reply = backend.query(11, Value::Null, &["java"]);
    assert_eq!(reply["error"]["code"], -32073);
    assert_eq!(
        reply["error"]["data"]["details"],
        json!({"reason": "miseNotFound"})
    );
    assert_eq!(backend.stop(), 0);

    // Empty setting, nothing on HOME, Homebrew or PATH (/usr/bin:/bin).
    if Path::new("/opt/homebrew/bin/mise").exists() || Path::new("/usr/local/bin/mise").exists() {
        return;
    }
    let mut backend = fixture.start(&["--mise=".to_owned()]);
    backend.initialize();
    assert_eq!(
        backend.query(12, Value::Null, &["java"])["error"]["data"]["details"]["reason"],
        "miseNotFound"
    );
    assert_eq!(backend.stop(), 0);
}

#[test]
fn cancellation_kills_the_running_mise_and_answers_cancelled() {
    let fixture = Fixture::new();
    fixture.write("fake/hang", "");
    let mut backend = fixture.start(&[]);
    backend.initialize();
    backend.send(request(20, Value::Null, &["java"]));
    let pid_file = fixture.path("fake/hang.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();
    backend.send(json!({"jsonrpc": "2.0", "method": "$/cancelRequest", "params": {"id": 20}}));
    let reply = backend.receive();
    assert_eq!(reply["id"], 20);
    assert_eq!(reply["error"]["code"], -32800);
    assert_eq!(
        reply["error"]["data"],
        json!({"code": "plugin.toolchain.cancelled", "operationID": "op-20", "providerID": "mise"})
    );
    let alive = Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the fake mise {pid} must be gone");
    assert_eq!(backend.stop(), 0);
}

#[test]
fn a_slow_mise_times_out_and_closing_stdin_leaves_no_child() {
    let fixture = Fixture::new();
    fixture.write("fake/hang", "");
    let mut backend = fixture.start(&["--child-timeout-ms=300".to_owned()]);
    backend.initialize();
    let reply = backend.query(30, Value::Null, &["java"]);
    assert_eq!(reply["error"]["code"], -32072);
    assert_eq!(reply["error"]["data"]["code"], "plugin.toolchain.timeout");

    // The first child may be killed before it records its PID.
    let _ = fs::remove_file(fixture.path("fake/hang.pid"));
    backend.send(request(31, Value::Null, &["java"]));
    let pid_file = fixture.path("fake/hang.pid");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !pid_file.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let pid = fs::read_to_string(&pid_file).unwrap().trim().to_owned();
    // The host disappears mid-query.
    drop(backend.input.take());
    assert!(wait(&mut backend.child, Duration::from_secs(5)).is_some());
    let alive = Command::new("kill")
        .args(["-0", &pid])
        .status()
        .unwrap()
        .success();
    assert!(!alive, "the fake mise {pid} must not outlive the backend");
}

#[test]
fn unknown_requests_and_bad_params_get_contract_errors() {
    let fixture = Fixture::new();
    let mut backend = fixture.start(&[]);
    backend.initialize();
    backend.send(json!({"jsonrpc": "2.0", "id": 40, "method": "something/else"}));
    assert_eq!(backend.receive()["error"]["code"], -32601);
    let mut bad = request(41, Value::Null, &["java"]);
    bad["params"]["authority"] = json!({"kind": "remote", "id": "r"});
    backend.send(bad);
    assert_eq!(backend.receive()["error"]["code"], -32071);
    let escape = backend.query(42, workspace(&fixture, "project", "../home"), &["java"]);
    assert_eq!(escape["error"]["code"], -32602);
    assert_eq!(escape["error"]["data"]["operationID"], "op-42");
    assert_eq!(backend.stop(), 0);
}

#[test]
fn oversized_listings_are_capped_with_referenced_installations_kept() {
    let fixture = Fixture::new();
    let installs = fixture.path("home/.local/share/mise/installs/node");
    let mut items = Vec::new();
    for index in 0..600 {
        let directory = installs.join(format!("1.{index}.0"));
        fixture.executable(&directory.join("bin/node"));
        items.push(json!({"version": format!("1.{index}.0"), "install_path": directory, "installed": true}));
    }
    fixture.write_json("fake/installed.json", json!({"node": items}));
    // The global request names the last, alphabetically late install.
    let wanted = installs.join("1.99.0");
    fixture.write_json(
        "fake/global.json",
        json!({"node": [{"version": "1.99.0", "requested_version": "1.99", "install_path": wanted,
            "source": {"type": "mise.toml", "path": fixture.path("home/.config/mise/config.toml")}, "installed": true}]}),
    );
    let mut backend = fixture.start(&[]);
    backend.initialize();
    let reply = backend.query(50, Value::Null, &["node"]);
    let installations = reply["result"]["installations"].as_array().unwrap();
    assert_eq!(installations.len(), 512);
    assert_eq!(installations[0]["home"], wanted.display().to_string());
    assert_eq!(
        reply["result"]["statements"][0]["installation"],
        wanted.display().to_string()
    );
    assert_eq!(backend.stop(), 0);
}
