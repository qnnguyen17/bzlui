use std::collections::BTreeSet;
use std::path::PathBuf;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use trie_rs::inc_search::{IncSearch, Position};

use crate::bazel::{self, BzlCommand, DetailUpdate, PackageTrie, Rule, RunUpdate};

#[derive(Debug)]
pub struct AppState {
    pub trie: PackageTrie,
    pub positions: Vec<Position>,
    pub children: Vec<String>,
    pub rules: Vec<Rule>,
    pub workspace_dir: PathBuf,
}

impl AppState {
    pub fn new(trie: PackageTrie, workspace_dir: PathBuf) -> Self {
        let mut search = trie.inc_search();
        search.query(&String::new()); // advance past "" root sentinel
        let initial_pos = Position::from(search);

        let mut app = Self {
            trie,
            positions: vec![initial_pos],
            children: Vec::new(),
            rules: Vec::new(),
            workspace_dir,
        };
        app.refresh_children();
        app
    }

    fn root_position(&self) -> Position {
        let mut search = self.trie.inc_search();
        search.query(&String::new());
        Position::from(search)
    }

    fn current_position(&self) -> Position {
        self.positions
            .last()
            .copied()
            .unwrap_or_else(|| self.root_position())
    }

    pub fn current_prefix(&self) -> Vec<String> {
        let search = IncSearch::resume(&self.trie, self.current_position());
        search.prefix()
    }

    pub fn refresh_children(&mut self) {
        let search = IncSearch::resume(&self.trie, self.current_position());
        let prefix: Vec<String> = search.prefix();
        let prefix_len = prefix.len();

        let mut child_set = BTreeSet::new();
        let results: Vec<(Vec<String>, _)> = self.trie.predictive_search(&prefix).collect();
        for (key, _) in &results {
            if key.len() > prefix_len {
                child_set.insert(key[prefix_len].clone());
            }
        }
        self.children = child_set.into_iter().collect();

        self.rules = search.value().cloned().unwrap_or_default();
    }

    pub fn go_up(&mut self) {
        if self.positions.len() > 1 {
            self.positions.pop();
            self.refresh_children();
        }
    }

    pub fn enter_selected_package(&mut self, selected_package: usize) {
        if let Some(child) = self.children.get(selected_package) {
            let mut search = IncSearch::resume(&self.trie, self.current_position());
            search.query(&child);
            self.positions.push(Position::from(search));
            self.refresh_children();
        }
    }

    pub fn spawn_rule_detail(&self, selected_rule: usize) -> Option<mpsc::UnboundedReceiver<DetailUpdate>> {
        let rule = self.rules.get(selected_rule)?;
        let prefix = self.current_prefix();
        let package = prefix[1..].join("/");
        Some(bazel::spawn_rule_detail_queries(
            self.workspace_dir.clone(),
            package,
            rule.name.clone(),
        ))
    }

    pub fn spawn_bzl_command(
        &self,
        command: BzlCommand,
        rule: &Rule,
    ) -> (mpsc::UnboundedReceiver<RunUpdate>, JoinHandle<()>, String) {
        let prefix = self.current_prefix();
        let package = prefix[1..].join("/");
        let target = format!("//{package}:{}", rule.name);
        let (rx, handle) = bazel::spawn_bzl_command(
            self.workspace_dir.clone(),
            command,
            target.clone(),
        );
        (rx, handle, target)
    }
}
