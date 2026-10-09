//! The build graph: manifest loading, validation, cycle detection, and
//! topological layering.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::instrument;

use crate::action::Action;
use crate::error::{ForgeError, Result};

/// The on-disk manifest schema: a single top-level `actions` array.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ManifestFile {
    actions: Vec<Action>,
}

/// A validated directed acyclic graph of [`Action`]s.
///
/// All paths in the contained actions are interpreted relative to
/// [`BuildGraph::base_dir`], which defaults to the directory containing the
/// manifest file.
#[derive(Debug, Clone)]
pub struct BuildGraph {
    base_dir: PathBuf,
    actions: BTreeMap<String, Action>,
}

impl BuildGraph {
    /// Loads and validates a manifest from a JSON file.
    ///
    /// Input/output paths in the manifest are resolved relative to the
    /// manifest's parent directory.
    #[instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub fn from_manifest(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                ForgeError::MissingFile(path.to_path_buf())
            } else {
                ForgeError::Io(e)
            }
        })?;
        let manifest: ManifestFile =
            serde_json::from_slice(&bytes).map_err(|e| ForgeError::Manifest(e.to_string()))?;
        let base_dir = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::from_actions(base_dir, manifest.actions)
    }

    /// Builds a graph from an in-memory action list.
    pub fn from_actions(base_dir: impl Into<PathBuf>, actions: Vec<Action>) -> Result<Self> {
        let mut map = BTreeMap::new();
        for action in actions {
            let id = action.id.clone();
            if map.insert(id.clone(), action).is_some() {
                return Err(ForgeError::DuplicateAction(id));
            }
        }
        let graph = BuildGraph {
            base_dir: base_dir.into(),
            actions: map,
        };
        graph.validate()?;
        Ok(graph)
    }

    /// The directory that relative input/output paths are resolved against.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Number of actions in the graph.
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Returns `true` when the graph has no actions.
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Looks up an action by id.
    pub fn get(&self, id: &str) -> Option<&Action> {
        self.actions.get(id)
    }

    /// All actions in deterministic (id-sorted) order.
    pub fn actions(&self) -> impl Iterator<Item = &Action> {
        self.actions.values()
    }

    /// Validates that every dependency exists and the graph is acyclic.
    pub fn validate(&self) -> Result<()> {
        for action in self.actions.values() {
            for dep in &action.deps {
                if !self.actions.contains_key(dep) {
                    return Err(ForgeError::UnknownDep {
                        action: action.id.clone(),
                        dep: dep.clone(),
                    });
                }
            }
        }
        // Cycle detection also guarantees acyclicity for the topological sort.
        let _ = self.find_cycle()?;
        Ok(())
    }

    /// Returns the ids that directly depend on `id`.
    pub fn dependents(&self, id: &str) -> Vec<String> {
        self.actions
            .values()
            .filter(|a| a.deps.iter().any(|d| d == id))
            .map(|a| a.id.clone())
            .collect()
    }

    /// Returns the transitive closure of `id` and everything that depends on it.
    pub fn transitive_dependents(&self, id: &str) -> HashSet<String> {
        let mut out = HashSet::new();
        let mut queue: VecDeque<String> = self.dependents(id).into_iter().collect();
        while let Some(current) = queue.pop_front() {
            if out.insert(current.clone()) {
                queue.extend(self.dependents(&current));
            }
        }
        out
    }

    /// Finds a cycle if one exists, returning a readable `a -> b -> a` path.
    fn find_cycle(&self) -> Result<Option<String>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Color {
            White,
            Gray,
            Black,
        }
        let mut color: HashMap<&str, Color> = self
            .actions
            .keys()
            .map(|k| (k.as_str(), Color::White))
            .collect();
        let mut stack: Vec<&str> = Vec::new();

        fn dfs<'a>(
            node: &'a str,
            graph: &'a BuildGraph,
            color: &mut HashMap<&'a str, Color>,
            stack: &mut Vec<&'a str>,
        ) -> Option<String> {
            use Color::*;
            color.insert(node, Gray);
            stack.push(node);
            if let Some(action) = graph.actions.get(node) {
                for dep in &action.deps {
                    let dep = dep.as_str();
                    match color.get(dep).copied().unwrap_or(Black) {
                        White => {
                            if let Some(cycle) = dfs(dep, graph, color, stack) {
                                return Some(cycle);
                            }
                        }
                        Gray => {
                            let start = stack.iter().position(|n| *n == dep).unwrap_or(0);
                            let mut path: Vec<&str> = stack[start..].to_vec();
                            path.push(dep);
                            return Some(path.join(" -> "));
                        }
                        Black => {}
                    }
                }
            }
            stack.pop();
            color.insert(node, Black);
            None
        }

        for key in self.actions.keys() {
            let key = key.as_str();
            if color.get(key).copied() == Some(Color::White) {
                if let Some(cycle) = dfs(key, self, &mut color, &mut stack) {
                    return Err(ForgeError::Cycle(cycle));
                }
            }
        }
        Ok(None)
    }

    /// Computes topological layers: layer 0 contains actions with no deps, each
    /// subsequent layer depends only on earlier layers.
    pub fn topo_layers(&self) -> Result<Vec<Vec<String>>> {
        // Map each action id to its number of unsatisfied dependencies.
        let mut indegree: BTreeMap<String, usize> = self
            .actions
            .keys()
            .map(|k| (k.clone(), self.actions[k].deps.len()))
            .collect();

        let mut ready: Vec<String> = indegree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(id, _)| id.clone())
            .collect();
        ready.sort();

        let mut layers: Vec<Vec<String>> = Vec::new();
        let mut resolved = 0usize;
        while !ready.is_empty() {
            layers.push(ready.clone());
            resolved += ready.len();
            let mut next: Vec<String> = Vec::new();
            for id in &ready {
                for dependent in self.dependents(id) {
                    let entry = indegree.get_mut(&dependent).expect("dependent present");
                    *entry -= 1;
                    if *entry == 0 {
                        next.push(dependent);
                    }
                }
            }
            next.sort();
            next.dedup();
            ready = next;
        }

        if resolved != self.actions.len() {
            return Err(ForgeError::Cycle(
                "graph contains a cycle (no topological order)".to_string(),
            ));
        }
        Ok(layers)
    }

    /// Returns the actions whose dependencies are all in `completed` and which
    /// are not themselves in `completed`, in deterministic order.
    pub fn ready<'a>(
        &'a self,
        completed: &'a HashSet<String>,
    ) -> impl Iterator<Item = &'a Action> + 'a {
        self.actions.values().filter(move |action| {
            !completed.contains(&action.id) && action.deps.iter().all(|dep| completed.contains(dep))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn action(id: &str, deps: &[&str]) -> Action {
        Action {
            id: id.to_string(),
            command: vec!["true".into()],
            env: BTreeMap::new(),
            inputs: vec![],
            outputs: vec![],
            deps: deps.iter().map(|s| s.to_string()).collect(),
            timeout_ms: 1000,
            tags: vec![],
        }
    }

    #[test]
    fn layers_are_computed_in_order() {
        let graph = BuildGraph::from_actions(
            ".",
            vec![
                action("c", &["a", "b"]),
                action("a", &[]),
                action("b", &["a"]),
            ],
        )
        .unwrap();
        let layers = graph.topo_layers().unwrap();
        assert_eq!(layers[0], vec!["a"]);
        assert_eq!(layers[1], vec!["b"]);
        assert_eq!(layers[2], vec!["c"]);
    }

    #[test]
    fn cycle_is_detected_and_named() {
        let err = BuildGraph::from_actions(
            ".",
            vec![
                action("a", &["b"]),
                action("b", &["c"]),
                action("c", &["a"]),
            ],
        )
        .unwrap_err();
        match err {
            ForgeError::Cycle(msg) => {
                assert!(msg.contains("a"));
                assert!(msg.contains("b"));
                assert!(msg.contains("c"));
            }
            other => panic!("expected cycle error, got {other:?}"),
        }
    }

    #[test]
    fn missing_dep_is_rejected() {
        let err = BuildGraph::from_actions(".", vec![action("a", &["ghost"])]).unwrap_err();
        assert!(matches!(err, ForgeError::UnknownDep { .. }));
    }

    #[test]
    fn duplicate_id_is_rejected() {
        let err =
            BuildGraph::from_actions(".", vec![action("a", &[]), action("a", &[])]).unwrap_err();
        assert!(matches!(err, ForgeError::DuplicateAction(_)));
    }

    #[test]
    fn ready_respects_completed_set() {
        let graph = BuildGraph::from_actions(
            ".",
            vec![action("a", &[]), action("b", &["a"]), action("c", &["a"])],
        )
        .unwrap();
        let mut done = HashSet::new();
        let first: Vec<_> = graph.ready(&done).map(|a| a.id.clone()).collect();
        assert_eq!(first, vec!["a"]);
        done.insert("a".to_string());
        let second: Vec<_> = graph.ready(&done).map(|a| a.id.clone()).collect();
        assert_eq!(second, vec!["b", "c"]);
    }

    #[test]
    fn transitive_dependents_are_correct() {
        let graph = BuildGraph::from_actions(
            ".",
            vec![
                action("a", &[]),
                action("b", &["a"]),
                action("c", &["b"]),
                action("d", &[]),
            ],
        )
        .unwrap();
        let deps = graph.transitive_dependents("a");
        assert!(deps.contains("b"));
        assert!(deps.contains("c"));
        assert!(!deps.contains("d"));
    }

    #[test]
    fn manifest_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("build.json");
        let manifest = serde_json::json!({
            "actions": [
                {
                    "id": "a",
                    "command": ["true"],
                    "env": {},
                    "inputs": [],
                    "outputs": [],
                    "deps": [],
                    "timeout_ms": 1000,
                    "tags": []
                }
            ]
        });
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        let graph = BuildGraph::from_manifest(&manifest_path).unwrap();
        assert_eq!(graph.len(), 1);
        assert_eq!(graph.base_dir(), dir.path());
    }
}
