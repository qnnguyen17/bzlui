use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use trie_rs::map::{Trie, TrieBuilder};

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub rule_type: String,
}

#[derive(Debug)]
pub enum DetailUpdate {
    Text(Result<String, String>),
    Runnable(bool),
    Testable(bool),
}

#[derive(Debug, Clone, Copy)]
pub enum BzlCommand {
    Build,
    Run,
    Test,
}

impl BzlCommand {
    fn arg(self) -> &'static str {
        match self {
            BzlCommand::Build => "build",
            BzlCommand::Run => "run",
            BzlCommand::Test => "test",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BzlCommand::Build => "Build",
            BzlCommand::Run => "Run",
            BzlCommand::Test => "Test",
        }
    }
}

#[derive(Debug)]
pub enum RunUpdate {
    Line(String),
    Exited(Option<i32>),
    SpawnFailed(String),
}

pub type PackageTrie = Trie<String, Vec<Rule>>;

pub fn load_workspace(workspace_dir: &Path) -> io::Result<PackageTrie> {
    let output = Command::new("bzl")
        .args(["query", "//...", "--output", "label_kind"])
        .current_dir(workspace_dir)
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(io::Error::new(
            io::ErrorKind::Other,
            format!("bzl query failed: {stderr}"),
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut packages: HashMap<Vec<String>, Vec<Rule>> = HashMap::new();

    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }

        // Format: "<rule_type> rule //<package>:<name>"
        let Some((rule_type, rest)) = line.split_once(" rule //") else {
            continue;
        };

        let Some((package, name)) = rest.split_once(':') else {
            continue;
        };

        // Prepend "" as a root sentinel so every key is non-empty.
        // //:foo -> [""], //tools/otel:bar -> ["", "tools", "otel"]
        let mut path_components: Vec<String> = vec![String::new()];
        if !package.is_empty() {
            path_components.extend(package.split('/').map(String::from));
        }

        packages.entry(path_components).or_default().push(Rule {
            name: name.to_string(),
            rule_type: rule_type.to_string(),
        });
    }

    let mut builder = TrieBuilder::new();
    for (key, rules) in packages {
        builder.push(key, rules);
    }

    Ok(builder.build())
}

pub fn spawn_rule_detail_queries(
    workspace_dir: PathBuf,
    package: String,
    rule: String,
) -> mpsc::UnboundedReceiver<DetailUpdate> {
    let target = format!("//{package}:{rule}");
    let (tx, rx) = mpsc::unbounded_channel();

    {
        let tx = tx.clone();
        let workspace_dir = workspace_dir.clone();
        let target = target.clone();
        tokio::spawn(async move {
            let result = query_rule_text(&workspace_dir, &target).await;
            let _ = tx.send(DetailUpdate::Text(result));
        });
    }

    {
        let tx = tx.clone();
        let workspace_dir = workspace_dir.clone();
        let target = target.clone();
        tokio::spawn(async move {
            let runnable = query_is_runnable(&workspace_dir, &target)
                .await
                .unwrap_or(false);
            let _ = tx.send(DetailUpdate::Runnable(runnable));
        });
    }

    tokio::spawn(async move {
        let testable = query_is_testable(&workspace_dir, &target)
            .await
            .unwrap_or(false);
        let _ = tx.send(DetailUpdate::Testable(testable));
    });

    rx
}

async fn query_rule_text(workspace_dir: &Path, target: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("bzl")
        .args(["query", target, "--output", "build"])
        .current_dir(workspace_dir)
        .output()
        .await
        .map_err(|e| format!("failed to spawn bzl: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("bzl query failed: {stderr}"));
    }

    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

async fn query_is_runnable(workspace_dir: &Path, target: &str) -> io::Result<bool> {
    let output = tokio::process::Command::new("bzl")
        .args([
            "cquery",
            target,
            "--output=starlark",
            "--starlark:expr=providers(target)['DefaultInfo'].files_to_run.executable != None",
        ])
        .current_dir(workspace_dir)
        .output()
        .await?;

    if !output.status.success() {
        return Ok(false);
    }

    Ok(String::from_utf8_lossy(&output.stdout).trim() == "True")
}

async fn query_is_testable(workspace_dir: &Path, target: &str) -> io::Result<bool> {
    let output = tokio::process::Command::new("bzl")
        .args(["query", &format!("tests({target})")])
        .current_dir(workspace_dir)
        .output()
        .await?;

    if !output.status.success() {
        return Ok(false);
    }

    Ok(!String::from_utf8_lossy(&output.stdout).trim().is_empty())
}

pub fn spawn_bzl_command(
    workspace_dir: PathBuf,
    command: BzlCommand,
    target: String,
) -> (mpsc::UnboundedReceiver<RunUpdate>, JoinHandle<()>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let handle = tokio::spawn(async move {
        let mut child = match tokio::process::Command::new("bzl")
            .arg(command.arg())
            .arg(&target)
            .current_dir(&workspace_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(RunUpdate::SpawnFailed(e.to_string()));
                return;
            }
        };

        let stdout = child.stdout.take().expect("stdout piped");
        let stderr = child.stderr.take().expect("stderr piped");
        let mut stdout_lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut stdout_done = false;
        let mut stderr_done = false;

        while !(stdout_done && stderr_done) {
            tokio::select! {
                line = stdout_lines.next_line(), if !stdout_done => {
                    match line {
                        Ok(Some(s)) => { let _ = tx.send(RunUpdate::Line(s)); }
                        Ok(None) | Err(_) => { stdout_done = true; }
                    }
                }
                line = stderr_lines.next_line(), if !stderr_done => {
                    match line {
                        Ok(Some(s)) => { let _ = tx.send(RunUpdate::Line(s)); }
                        Ok(None) | Err(_) => { stderr_done = true; }
                    }
                }
            }
        }

        let code = child.wait().await.ok().and_then(|s| s.code());
        let _ = tx.send(RunUpdate::Exited(code));
    });
    (rx, handle)
}
