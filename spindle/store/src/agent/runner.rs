//! The remote runner: commands an embedded session asks this host to
//! run, each over a copy of the call's workspace sent with it, in this host's
//! sandbox (or directly when it has none and isolation allows), with the output and
//! the changed files returned. The embedded store records them as the call's result
//! and new workspace, so a rewind still covers what the command did; nothing a run
//! leaves stays on this host.
use super::{Isolation, Result};
use crate::protocol::{base64_decode, base64_encode};
use dext_native::strict::{Input, RunHost, RunOptions, SealedEntry, StrictExecutor};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Largest workspace a run request carries (its files' bytes).
pub const MAX_RUN_TREE: usize = 48 * 1024 * 1024;

/// This host's command runner, as remote runs use it.
pub struct Runner {
    executor: StrictExecutor,
}

struct Files(BTreeMap<String, Vec<u8>>);

impl RunHost for Files {
    fn load(&mut self, path: &str, out: &mut dyn std::io::Write) -> std::io::Result<()> {
        let bytes = self
            .0
            .get(path)
            .ok_or_else(|| std::io::Error::other("no such input file"))?;
        out.write_all(bytes)
    }
}

impl Runner {
    /// The sandbox when this host can run one; otherwise the host runner, unless
    /// `isolation` requires the sandbox.
    pub fn open(isolation: Isolation) -> Result<Self> {
        let executor = match (isolation, super::open_isolated()) {
            (Isolation::Off, _) => StrictExecutor::host(),
            (_, Ok(sandbox)) => sandbox,
            (Isolation::Required, Err(why)) => {
                return Err(format!("isolation is required but unavailable: {why}"));
            }
            (_, Err(_)) => StrictExecutor::host(),
        };
        Ok(Self { executor })
    }

    /// What an embedded session labels this runner's runs with: its identity (the
    /// sandbox's digest, or the host runner's) and whether it isolates commands.
    pub fn identity(&self) -> Value {
        let helper: String = self
            .executor
            .helper_digest()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        json!({
            "helper": helper,
            "isolated": self.executor.isolated(),
            "implementation": self.executor.implementation(),
        })
    }

    /// Runs one command: `{command, timeout_s, network, files: [{path, base64,
    /// executable}], links: [{path, target}], directories}` gives `{output, success,
    /// writes, deletes, links, directories, failure, helper}`; changes are against the
    /// files sent.
    pub fn run(&self, request: &Value, interrupt: &AtomicBool) -> Result<Value> {
        let command = request["command"]
            .as_str()
            .filter(|command| !command.trim().is_empty())
            .ok_or("`command` is required")?;
        let timeout = request["timeout_s"].as_u64().unwrap_or(30);
        if !(1..=dext_native::limits::MAX_RUN_SECS).contains(&timeout) {
            return Err(format!(
                "`timeout_s` must be 1-{}",
                dext_native::limits::MAX_RUN_SECS
            ));
        }
        let mut tree = BTreeMap::new();
        let mut files = BTreeMap::new();
        let mut total = 0usize;
        for file in request["files"].as_array().into_iter().flatten() {
            let path = file["path"].as_str().ok_or("each file has a `path`")?;
            let bytes = base64_decode(file["base64"].as_str().unwrap_or_default())
                .ok_or_else(|| format!("`{path}` is not standard base64"))?;
            total += bytes.len();
            if total > MAX_RUN_TREE {
                return Err(format!(
                    "the workspace is over {} MiB",
                    MAX_RUN_TREE / 1024 / 1024
                ));
            }
            tree.insert(
                path.to_string(),
                Input::File {
                    size: bytes.len() as u64,
                    executable: file["executable"] == true,
                },
            );
            files.insert(path.to_string(), bytes);
        }
        for link in request["links"].as_array().into_iter().flatten() {
            let (Some(path), Some(target)) = (link["path"].as_str(), link["target"].as_str())
            else {
                return Err("each link has `path` and `target`".into());
            };
            tree.insert(path.to_string(), Input::Link(target.to_string()));
        }
        for directory in request["directories"].as_array().into_iter().flatten() {
            let path = directory.as_str().ok_or("`directories` lists paths")?;
            tree.insert(path.to_string(), Input::Directory);
        }
        let bash = self
            .executor
            .program(dext_native::strict::Program::Bash)
            .ok_or("bash is not available on the runner's host")?;
        let argv = vec![
            bash,
            "--noprofile".into(),
            "--norc".into(),
            "-c".into(),
            command.to_string(),
        ];
        let options = RunOptions {
            network: request["network"] == true,
            ..RunOptions::default()
        };
        let helper = self.identity()["helper"].clone();
        let mut host = Files(files);
        let sealed = match self.executor.execute_tree(
            &tree,
            &argv,
            Duration::from_secs(timeout),
            interrupt,
            options,
            &[],
            false,
            &mut host,
        ) {
            Ok(sealed) => sealed,
            Err(error) => {
                let (reason, output) = match dext_native::strict::run_failure(&error) {
                    Some(failure) => (
                        failure.reason.clone(),
                        format!(
                            "{}{}",
                            String::from_utf8_lossy(&failure.stdout),
                            String::from_utf8_lossy(&failure.stderr)
                        ),
                    ),
                    None => (
                        format!("the run could not be completed: {error}"),
                        String::new(),
                    ),
                };
                return Ok(json!({"output": output, "success": false, "writes": [],
                    "deletes": [], "links": [], "directories": [], "failure": reason,
                    "helper": helper}));
            }
        };
        let output = format!(
            "{}{}",
            String::from_utf8_lossy(sealed.stdout()),
            String::from_utf8_lossy(sealed.stderr())
        );
        let mut writes = vec![];
        let mut links = vec![];
        let mut directories = vec![];
        for (path, entry) in sealed.entries() {
            match entry {
                // Unchanged files keep their bytes and mode: nothing to send back.
                SealedEntry::Unchanged { .. } => {}
                SealedEntry::File { executable, .. } => {
                    let bytes = sealed
                        .read(path)
                        .map_err(|error| format!("cannot read `{path}`: {error}"))?;
                    writes.push(json!({"path": path, "base64": base64_encode(&bytes),
                        "executable": executable}));
                }
                SealedEntry::Link(target) => {
                    if !matches!(tree.get(path), Some(Input::Link(was)) if was == target) {
                        links.push(json!({"path": path, "target": target}));
                    }
                }
                SealedEntry::Directory => {
                    if !matches!(tree.get(path), Some(Input::Directory)) {
                        directories.push(json!(path));
                    }
                }
            }
        }
        let deletes: Vec<&String> = tree
            .keys()
            .filter(|path| !sealed.entries().contains_key(*path))
            .collect();
        Ok(json!({
            "output": output,
            "success": sealed.success(),
            "writes": writes,
            "deletes": deletes,
            "links": links,
            "directories": directories,
            "failure": null,
            "helper": helper,
        }))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_run_returns_its_output_and_what_it_changed() {
        let runner = Runner::open(Isolation::Off).unwrap();
        assert_eq!(runner.identity()["isolated"], false);
        let request = json!({
            "command": "cat a.txt; cp a.txt b.txt && rm a.txt && rmdir gone && mkdir d && ln -s b.txt l",
            "timeout_s": 30,
            "files": [
                {"path": "a.txt", "base64": base64_encode(b"alpha"), "executable": false},
                {"path": "keep.txt", "base64": base64_encode(b"kept"), "executable": false},
            ],
            "links": [],
            "directories": ["gone"],
        });
        let result = runner.run(&request, &AtomicBool::new(false)).unwrap();
        assert_eq!(result["success"], true, "{result}");
        assert_eq!(result["output"], "alpha");
        assert_eq!(result["failure"], Value::Null);
        assert_eq!(
            result["writes"],
            json!([{"path": "b.txt", "base64": base64_encode(b"alpha"), "executable": false}])
        );
        let mut deletes: Vec<&str> = result["deletes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| path.as_str().unwrap())
            .collect();
        deletes.sort();
        assert_eq!(deletes, ["a.txt", "gone"]);
        assert_eq!(result["links"], json!([{"path": "l", "target": "b.txt"}]));
        assert_eq!(result["directories"], json!(["d"]));
        assert_eq!(result["helper"], runner.identity()["helper"]);
    }

    #[test]
    fn bad_requests_are_refused_before_anything_runs() {
        let runner = Runner::open(Isolation::Off).unwrap();
        for request in [
            json!({"command": ""}),
            json!({"command": "true", "timeout_s": 0}),
            json!({"command": "true", "files": [{"path": "a", "base64": "!"}]}),
        ] {
            assert!(
                runner.run(&request, &AtomicBool::new(false)).is_err(),
                "{request}"
            );
        }
        let failed = runner
            .run(
                &json!({"command": "exit 3", "timeout_s": 5}),
                &AtomicBool::new(false),
            )
            .unwrap();
        assert_eq!(failed["success"], false);
    }
}
