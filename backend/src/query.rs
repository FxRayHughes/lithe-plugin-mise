//! Maps mise's JSON output to one `toolchains/query` result.
//!
//! mise stays the only authority on versions: nothing here reads its config
//! files or interprets a request such as `lts`.

use crate::mise::{self, Options, RunError};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const MAX_INSTALLATIONS: usize = 512;
const MAX_STATEMENTS: usize = 256;
const MAX_WATCH: usize = 256;
const MAX_PATH_CHARACTERS: usize = 4096;
const MAX_MESSAGE_BYTES: usize = 1000;

/// Exactly the manifest's `watchFiles`; a test compares the two.
pub const WATCH_FILES: &[(&str, &str)] = &[
    ("mise.toml", "native"),
    ("mise.local.toml", "native"),
    ("mise.*.toml", "native"),
    (".mise.toml", "native"),
    (".mise.local.toml", "native"),
    (".mise.*.toml", "native"),
    ("mise/config.toml", "native"),
    ("mise/config.local.toml", "native"),
    ("mise/conf.d/*.toml", "native"),
    (".mise/config.toml", "native"),
    (".mise/config.local.toml", "native"),
    (".mise/conf.d/*.toml", "native"),
    (".config/mise.toml", "native"),
    (".config/mise.local.toml", "native"),
    (".config/mise/config.toml", "native"),
    (".config/mise/config.local.toml", "native"),
    (".config/mise/mise.toml", "native"),
    (".config/mise/conf.d/*.toml", "native"),
    (".tool-versions", "compatible"),
    (".java-version", "compatible"),
    (".node-version", "compatible"),
    (".nvmrc", "compatible"),
    (".python-version", "compatible"),
    (".sdkmanrc", "compatible"),
];

/// Tool ID, the names mise may list it under, and the programs that prove an
/// installation is complete. Core rejects a whole reply over one home without
/// its executable, so a broken install must not be reported.
const TOOLS: &[(&str, &[&str], &[&str])] = &[
    ("go", &["go", "core:go"], &["go"]),
    ("gradle", &["gradle", "aqua:gradle/gradle"], &["gradle"]),
    ("java", &["java", "core:java"], &["java"]),
    ("maven", &["maven", "aqua:apache/maven"], &["mvn"]),
    ("node", &["node", "nodejs", "core:node"], &["node"]),
    ("pnpm", &["pnpm", "aqua:pnpm/pnpm"], &["pnpm"]),
    ("python", &["python", "core:python"], &["python3", "python"]),
];

#[derive(Debug)]
pub struct Failure {
    pub code: i64,
    pub semantic: &'static str,
    pub message: String,
    pub reason: Option<&'static str>,
}

impl Failure {
    pub fn new(code: i64, semantic: &'static str, message: impl Into<String>) -> Self {
        let mut message = message.into();
        if message.len() > MAX_MESSAGE_BYTES {
            let mut end = MAX_MESSAGE_BYTES;
            while !message.is_char_boundary(end) {
                end -= 1;
            }
            message.truncate(end);
        }
        Self {
            code,
            semantic,
            message,
            reason: None,
        }
    }

    fn unavailable(reason: &'static str, message: impl Into<String>) -> Self {
        let mut failure = Self::new(-32073, "plugin.toolchain.providerUnavailable", message);
        failure.reason = Some(reason);
        failure
    }

    fn invalid(message: &str) -> Self {
        Self::new(-32602, "plugin.toolchain.invalidParams", message)
    }
}

pub fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.starts_with(|c: char| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:-".contains(c))
}

pub struct Query {
    operation: String,
    generation: u64,
    /// The opened root and the directory being resolved, both canonical.
    workspace: Option<(PathBuf, PathBuf)>,
    tools: BTreeSet<String>,
}

impl Query {
    pub fn parse(params: &Value) -> Result<Self, Failure> {
        let operation = params["operationID"]
            .as_str()
            .filter(|value| is_identifier(value))
            .ok_or_else(|| Failure::invalid("operationID is missing or invalid."))?;
        let generation = params["generation"]
            .as_u64()
            .filter(|value| *value >= 1)
            .ok_or_else(|| Failure::invalid("generation is missing or invalid."))?;
        if params["providerID"] != crate::PROVIDER_ID {
            return Err(Failure::invalid("This backend only serves provider mise."));
        }
        if params["authority"]["kind"] != "local" {
            return Err(Failure::new(
                -32071,
                "plugin.toolchain.authorityUnsupported",
                "mise only answers for the local machine.",
            ));
        }
        let tools: BTreeSet<String> = params["tools"]
            .as_array()
            .filter(|tools| !tools.is_empty() && tools.len() <= 64)
            .ok_or_else(|| Failure::invalid("tools must list 1 to 64 IDs."))?
            .iter()
            .map(|tool| tool.as_str().map(str::to_owned))
            .collect::<Option<_>>()
            .ok_or_else(|| Failure::invalid("tools must be strings."))?;
        let workspace = match &params["workspace"] {
            Value::Null if params.get("workspace").is_some() => None,
            Value::Object(workspace) => Some(parse_workspace(workspace)?),
            _ => return Err(Failure::invalid("workspace must be an object or null.")),
        };
        Ok(Self {
            operation: operation.to_owned(),
            generation,
            workspace,
            tools,
        })
    }

    pub fn run(&self, options: &Options, cancelled: &AtomicBool) -> Result<Value, Failure> {
        let program = mise::locate(options).ok_or_else(|| {
            let message = match &options.configured {
                Some(path) => format!(
                    "设置里的 mise 路径不可用：{0}。/ The configured mise path is unusable: {0}.",
                    path.display()
                ),
                None => "找不到 mise。请安装 mise，或在插件设置里指定路径。/ mise was not found. Install it or set its path in the plugin settings.".to_owned(),
            };
            Failure::unavailable("miseNotFound", message)
        })?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|home| home.is_absolute())
            .ok_or_else(|| Failure::unavailable("queryFailed", "HOME is not set."))?;
        let run = |arguments: &[&OsStr], directory: &Path| {
            mise::run(&program, arguments, directory, options, cancelled).map_err(run_failure)
        };
        let listing = |flag: &str, directory: &Path| -> Result<Value, Failure> {
            let arguments = [OsStr::new("ls"), OsStr::new("--json"), OsStr::new(flag)];
            serde_json::from_slice(&run(&arguments, directory)?).map_err(|_| {
                Failure::unavailable("invalidOutput", "mise printed output that is not JSON.")
            })
        };
        // An untrusted project fails every mise command run inside it, so only
        // the project listing runs there; everything global runs from $HOME.
        let project = match &self.workspace {
            Some((_, directory)) => Some(listing("--local", directory)?),
            None => None,
        };
        let installed = listing("--installed", &home)?;
        let global = listing("--global", &home)?;

        let entries = self.installed_entries(&installed);
        // A bounded command line; later entries fall back to `<install>/bin`.
        let arguments: Vec<String> = entries
            .iter()
            .take(MAX_INSTALLATIONS)
            .map(|entry| format!("{}@{}", entry.name, entry.version))
            .collect();
        let bin_directories = if arguments.is_empty() {
            Vec::new()
        } else {
            let mut command = vec![OsStr::new("bin-paths")];
            command.extend(arguments.iter().map(OsStr::new));
            match run(&command, &home) {
                Ok(output) => String::from_utf8_lossy(&output)
                    .lines()
                    .map(PathBuf::from)
                    .collect(),
                // Layout discovery is an optimization over the install directory,
                // but a stop request still has to win.
                Err(failure) if failure.reason.is_none() => return Err(failure),
                Err(_) => Vec::new(),
            }
        };
        let homes = self.resolve_homes(entries, &bin_directories);

        let mut statements = Vec::new();
        if let Some(project) = &project {
            statements.extend(self.statements(project, "project", &homes));
        }
        statements.extend(self.statements(&global, "global", &homes));
        statements.truncate(MAX_STATEMENTS);

        // Installations a statement names come first, so a cap never breaks a reference.
        let referenced: BTreeSet<&str> = statements
            .iter()
            .filter_map(|statement| statement["installation"].as_str())
            .collect();
        let mut listed: BTreeMap<(bool, &str, &str), &Home> = BTreeMap::new();
        for home in homes.values() {
            let key = (
                !referenced.contains(home.home.as_str()),
                home.tool.as_str(),
                home.home.as_str(),
            );
            listed.insert(key, home);
        }
        let installations: Vec<Value> = listed
            .values()
            .take(MAX_INSTALLATIONS)
            .map(|home| json!({"tool": home.tool, "version": home.version, "home": home.home}))
            .collect();

        let watch = self.watch(&statements, &installed, &home);
        Ok(json!({
            "operationID": self.operation,
            "generation": self.generation,
            "providerID": crate::PROVIDER_ID,
            "installations": installations,
            "statements": statements,
            "watch": watch,
        }))
    }

    /// Installed entries of the requested tools, real directories before
    /// alias links, so `latest -> 25.0.2` never replaces the real version.
    fn installed_entries(&self, installed: &Value) -> Vec<Entry> {
        let mut entries = Vec::new();
        for (name, tool) in self.named_tools(installed) {
            for item in installed[name].as_array().into_iter().flatten() {
                let (Some(version), Some(path)) =
                    (item["version"].as_str(), item["install_path"].as_str())
                else {
                    continue;
                };
                if !bounded(version, 128) || !bounded(path, MAX_PATH_CHARACTERS) {
                    continue;
                }
                let raw = PathBuf::from(path);
                let Ok(canonical) = raw.canonicalize() else {
                    continue;
                };
                entries.push(Entry {
                    name: name.to_owned(),
                    tool: tool.to_owned(),
                    version: version.to_owned(),
                    alias: canonical != raw,
                    canonical,
                });
            }
        }
        entries.sort_by_key(|entry| entry.alias);
        // No cap here: the reply cap keeps referenced installations first.
        entries
    }

    /// Requested tools as `(mise name, tool ID)`, for the names in a listing.
    fn named_tools<'a>(
        &'a self,
        listing: &'a Value,
    ) -> impl Iterator<Item = (&'a str, &'static str)> {
        listing
            .as_object()
            .into_iter()
            .flat_map(|object| object.keys())
            .filter_map(|name| {
                let tool = TOOLS.iter().find(|tool| tool.1.contains(&name.as_str()))?;
                self.tools
                    .contains(tool.0)
                    .then_some((name.as_str(), tool.0))
            })
    }

    /// Maps each canonical install directory to its real tool home. Maven lives
    /// in `apache-maven-x/` below the install directory; mise knows where.
    fn resolve_homes(
        &self,
        entries: Vec<Entry>,
        bin_directories: &[PathBuf],
    ) -> BTreeMap<(String, PathBuf), Home> {
        let bins: Vec<PathBuf> = bin_directories
            .iter()
            .filter_map(|path| path.canonicalize().ok())
            .collect();
        let mut homes: BTreeMap<(String, PathBuf), Home> = BTreeMap::new();
        for entry in entries {
            let key = (entry.tool.clone(), entry.canonical.clone());
            if homes.contains_key(&key) {
                continue;
            }
            let Some(programs) = TOOLS
                .iter()
                .find(|tool| tool.0 == entry.tool)
                .map(|tool| tool.2)
            else {
                continue;
            };
            let candidates = bins
                .iter()
                .filter(|bin| bin.starts_with(&entry.canonical))
                .cloned()
                .chain([entry.canonical.join("bin"), entry.canonical.clone()]);
            let found = candidates
                .filter(|directory| {
                    programs
                        .iter()
                        .any(|program| directory.join(program).is_file())
                })
                .find_map(|directory| {
                    let home = if directory.file_name() == Some(OsStr::new("bin")) {
                        directory.parent()?.to_path_buf()
                    } else {
                        directory
                    };
                    home.to_str().map(str::to_owned)
                });
            let Some(home) = found.filter(|home| bounded(home, MAX_PATH_CHARACTERS)) else {
                continue;
            };
            // A workspace-contained home is project code, not an external install.
            if self.inside_workspace(Path::new(&home)) {
                continue;
            }
            homes.insert(
                key,
                Home {
                    tool: entry.tool,
                    version: entry.version,
                    home,
                },
            );
        }
        homes
    }

    fn inside_workspace(&self, path: &Path) -> bool {
        self.workspace
            .as_ref()
            .is_some_and(|(root, _)| path.starts_with(root))
    }

    /// One listing as statements. Each source states one requirement per tool,
    /// which for `java = ["17", "21"]` is mise's first, primary entry.
    fn statements(
        &self,
        listing: &Value,
        scope: &str,
        homes: &BTreeMap<(String, PathBuf), Home>,
    ) -> Vec<Value> {
        let mut seen = BTreeSet::new();
        let mut statements = Vec::new();
        for (name, tool) in self.named_tools(listing) {
            for item in listing[name].as_array().into_iter().flatten() {
                // `MISE_JAVA_VERSION` and bare installs have no file to name.
                let Some(source) = item["source"]["path"].as_str() else {
                    continue;
                };
                let Some(request) = item["requested_version"]
                    .as_str()
                    .or_else(|| item["version"].as_str())
                    .filter(|request| bounded(request, 512))
                else {
                    continue;
                };
                let Some(reader) = self.source_reader(source, scope) else {
                    continue;
                };
                if !seen.insert((tool, source)) {
                    continue;
                }
                let installation = (item["installed"] == true)
                    .then(|| {
                        Path::new(item["install_path"].as_str()?)
                            .canonicalize()
                            .ok()
                    })
                    .flatten()
                    .and_then(|path| homes.get(&(tool.to_owned(), path)))
                    .map(|home| home.home.clone());
                statements.push(json!({
                    "tool": tool,
                    "scope": scope,
                    "source": source,
                    "request": request,
                    "installation": installation,
                    "kind": "declaration",
                    "reader": reader,
                }));
            }
        }
        statements
    }

    /// The manifest reader of an existing absolute source file. Core rejects
    /// the whole reply over a source it cannot anchor, so such a source is dropped.
    fn source_reader(&self, source: &str, scope: &str) -> Option<&'static str> {
        let path = Path::new(source);
        if !bounded(source, MAX_PATH_CHARACTERS) || !path.is_absolute() || !path.is_file() {
            return None;
        }
        let canonical = path.canonicalize().ok()?;
        let canonical = canonical.to_str()?;
        WATCH_FILES.iter().find_map(|(pattern, reader)| {
            let anchor = source_anchor(canonical, pattern)?;
            let usable = scope == "global"
                || self.workspace.as_ref().is_some_and(|(_, directory)| {
                    directory.starts_with(&anchor)
                        && directory.components().count() - Path::new(&anchor).components().count()
                            < 64
                });
            usable.then_some(*reader)
        })
    }

    fn watch(&self, statements: &[Value], installed: &Value, home: &Path) -> Vec<String> {
        let environment = |name: &str| std::env::var_os(name).map(PathBuf::from);
        let mut watch = Vec::new();
        for statement in statements {
            if let Some(source) = statement["source"].as_str() {
                watch.push(PathBuf::from(source));
            }
        }
        let config = environment("MISE_CONFIG_DIR").unwrap_or_else(|| {
            environment("XDG_CONFIG_HOME")
                .unwrap_or_else(|| home.join(".config"))
                .join("mise")
        });
        watch.extend(environment("MISE_GLOBAL_CONFIG_FILE"));
        for name in ["config.toml", "config.local.toml", "mise.toml", "conf.d"] {
            watch.push(config.join(name));
        }
        watch.push(home.join(".tool-versions"));
        watch.push(PathBuf::from("/etc/mise/config.toml"));
        let installs = environment("MISE_INSTALLS_DIR").unwrap_or_else(|| {
            environment("MISE_DATA_DIR")
                .unwrap_or_else(|| {
                    environment("XDG_DATA_HOME")
                        .unwrap_or_else(|| home.join(".local/share"))
                        .join("mise")
                })
                .join("installs")
        });
        watch.push(installs);
        for (name, _) in self.named_tools(installed) {
            for item in installed[name].as_array().into_iter().flatten() {
                if let Some(parent) = item["install_path"]
                    .as_str()
                    .and_then(|path| Path::new(path).parent())
                {
                    watch.push(parent.to_path_buf());
                }
            }
        }
        let mut seen = BTreeSet::new();
        watch
            .into_iter()
            .filter(|path| path.is_absolute())
            .filter_map(|path| path.to_str().map(str::to_owned))
            .filter(|path| bounded(path, MAX_PATH_CHARACTERS) && seen.insert(path.clone()))
            .take(MAX_WATCH)
            .collect()
    }
}

fn parse_workspace(
    workspace: &serde_json::Map<String, Value>,
) -> Result<(PathBuf, PathBuf), Failure> {
    let root = workspace
        .get("root")
        .and_then(Value::as_str)
        .map(Path::new)
        .filter(|root| root.is_absolute())
        .ok_or_else(|| Failure::invalid("workspace.root must be absolute."))?;
    let relative = workspace
        .get("directory")
        .and_then(Value::as_str)
        .ok_or_else(|| Failure::invalid("workspace.directory is required."))?;
    let safe = relative == "."
        || (!relative.starts_with('/')
            && relative
                .split('/')
                .all(|part| !part.is_empty() && part != "." && part != ".."));
    if !safe {
        return Err(Failure::invalid(
            "workspace.directory must stay inside the root.",
        ));
    }
    let root = root
        .canonicalize()
        .map_err(|_| Failure::invalid("workspace.root does not exist."))?;
    let directory = root
        .join(relative)
        .canonicalize()
        .map_err(|_| Failure::invalid("workspace.directory does not exist."))?;
    if !directory.starts_with(&root) || !directory.is_dir() {
        return Err(Failure::invalid(
            "workspace.directory must stay inside the root.",
        ));
    }
    Ok((root, directory))
}

fn run_failure(error: RunError) -> Failure {
    match error {
        RunError::NotFound => Failure::unavailable(
            "miseNotFound",
            "无法启动 mise。/ mise could not be started.",
        ),
        RunError::Cancelled => Failure::new(-32800, "plugin.toolchain.cancelled", "The query was cancelled."),
        RunError::Timeout => Failure::new(-32072, "plugin.toolchain.timeout", "mise did not answer in time."),
        RunError::Failed(stderr) => match untrusted_config(&stderr) {
            Some(config) => Failure::unavailable(
                "untrusted",
                format!(
                    "mise 配置未受信任：{config}。请确认内容后在终端自行运行 mise trust。/ This mise config is not trusted: {config}. Review it, then run `mise trust` yourself."
                ),
            ),
            None => Failure::unavailable(
                "queryFailed",
                format!("mise failed: {}", stderr.trim()),
            ),
        },
    }
}

/// Reads the config path from mise's refusal, or `None` when it is another failure.
fn untrusted_config(stderr: &str) -> Option<String> {
    if !stderr.contains("not trusted") {
        return None;
    }
    let path = stderr.lines().find_map(|line| {
        let (_, rest) = line.split_once("Config files in ")?;
        rest.split(" are not trusted").next()
    });
    Some(path.unwrap_or("?").to_owned())
}

/// Length and control-character bounds shared by the reply's strings.
fn bounded(value: &str, maximum: usize) -> bool {
    !value.is_empty() && value.chars().count() <= maximum && !value.chars().any(char::is_control)
}

/// Same matching as Core: the pattern's components against the path's last
/// components, returning the directory the pattern is anchored at.
fn source_anchor(source: &str, pattern: &str) -> Option<String> {
    let segments: Vec<&str> = source.split('/').collect();
    let patterns: Vec<&str> = pattern.split('/').collect();
    if segments.len() <= patterns.len() {
        return None;
    }
    let tail = &segments[segments.len() - patterns.len()..];
    if !patterns.iter().zip(tail).all(|(p, v)| glob_component(p, v)) {
        return None;
    }
    let anchor = segments[..segments.len() - patterns.len()].join("/");
    Some(if anchor.is_empty() {
        "/".to_owned()
    } else {
        anchor
    })
}

fn glob_component(pattern: &str, value: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut remaining = value;
    for (index, part) in parts.iter().enumerate() {
        if index == 0 {
            let Some(rest) = remaining.strip_prefix(part) else {
                return false;
            };
            remaining = rest;
        } else if index == parts.len() - 1 {
            return remaining.ends_with(part);
        } else if let Some(position) = remaining.find(part) {
            remaining = &remaining[position + part.len()..];
        } else {
            return false;
        }
    }
    remaining.is_empty()
}

struct Entry {
    name: String,
    tool: String,
    version: String,
    alias: bool,
    canonical: PathBuf,
}

struct Home {
    tool: String,
    version: String,
    home: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_files_match_the_manifest() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../io.github.fxrayhughes.mise/plugin.json"))
                .unwrap();
        let declared: Vec<(String, String)> = manifest["toolchainProviders"][0]["watchFiles"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                (
                    entry["pattern"].as_str().unwrap().to_owned(),
                    entry["reader"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        let expected: Vec<(String, String)> = WATCH_FILES
            .iter()
            .map(|(pattern, reader)| (pattern.to_string(), reader.to_string()))
            .collect();
        assert_eq!(declared, expected);
    }

    #[test]
    fn tools_match_the_manifest() {
        let manifest: Value =
            serde_json::from_str(include_str!("../../io.github.fxrayhughes.mise/plugin.json"))
                .unwrap();
        let declared = manifest["toolchainProviders"][0]["tools"]
            .as_array()
            .unwrap();
        let expected: Vec<&str> = TOOLS.iter().map(|tool| tool.0).collect();
        assert_eq!(declared, &expected);
    }

    #[test]
    fn anchors_follow_the_pattern_not_the_parent_directory() {
        assert_eq!(
            source_anchor("/p/.mise/config.toml", ".mise/config.toml").unwrap(),
            "/p"
        );
        assert_eq!(source_anchor("/p/mise.toml", "mise.toml").unwrap(), "/p");
        assert_eq!(
            source_anchor("/p/mise.dev.toml", "mise.*.toml").unwrap(),
            "/p"
        );
        assert_eq!(
            source_anchor(
                "/h/.config/mise/conf.d/a.toml",
                ".config/mise/conf.d/*.toml"
            )
            .unwrap(),
            "/h"
        );
        assert!(source_anchor("/p/other.toml", "mise.toml").is_none());
        assert!(source_anchor("/mise/config.toml", ".mise/config.toml").is_none());
    }

    #[test]
    fn untrusted_path_comes_from_mises_message() {
        let stderr = "mise ERROR error parsing config file: /p/mise.toml\nmise ERROR Config files in /p/mise.toml are not trusted.\nTrust them with `mise trust`.";
        assert_eq!(untrusted_config(stderr).unwrap(), "/p/mise.toml");
        assert!(untrusted_config("mise ERROR something else").is_none());
    }

    #[test]
    fn long_messages_are_cut_on_a_character_boundary() {
        let failure = Failure::new(-32073, "x", "配".repeat(1000));
        assert!(failure.message.len() <= MAX_MESSAGE_BYTES);
    }

    #[test]
    fn workspace_directories_cannot_escape() {
        let parse = |directory: &str| {
            let workspace = json!({"root": "/tmp", "directory": directory});
            parse_workspace(workspace.as_object().unwrap())
        };
        assert!(parse(".").is_ok());
        assert!(parse("../etc").is_err());
        assert!(parse("/etc").is_err());
        assert!(parse("a//b").is_err());
    }
}
