//! Only `ds_daemon` may talk to the robot. The front ends reach it through
//! `ds_proto`, so a front end that depended on `robocol`, even indirectly,
//! could reach around the daemon. This walks the resolved dependency graph
//! in `Cargo.lock`, so a direct dependency, a renamed or target-specific
//! one, and one pulled in through another crate are all caught.

use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Package name to the names of its dependencies, from `Cargo.lock`.
fn dependency_graph(lock: &str) -> HashMap<String, Vec<String>> {
    let mut graph = HashMap::new();
    for package in lock.split("[[package]]").skip(1) {
        let mut name = None;
        let mut dependencies = Vec::new();
        let mut in_dependencies = false;
        for line in package.lines().map(str::trim) {
            if in_dependencies {
                if line.starts_with(']') {
                    in_dependencies = false;
                } else if let Some(dep) = line
                    .trim_matches(|c| c == '"' || c == ',')
                    .split(' ')
                    .next()
                {
                    // An entry may carry a version and source: "serde 1.0.219 (registry+...)".
                    dependencies.push(dep.to_string());
                }
            } else if let Some(value) = line.strip_prefix("name = ") {
                name = Some(value.trim_matches('"').to_string());
            } else if line == "dependencies = [" {
                in_dependencies = true;
            }
        }
        if let Some(name) = name {
            graph.insert(name, dependencies);
        }
    }
    graph
}

fn reaches(graph: &HashMap<String, Vec<String>>, from: &str, target: &str) -> bool {
    let mut seen = HashSet::new();
    let mut todo = vec![from.to_string()];
    while let Some(package) = todo.pop() {
        if package == target {
            return true;
        }
        if seen.insert(package.clone()) {
            todo.extend(graph.get(&package).into_iter().flatten().cloned());
        }
    }
    false
}

#[test]
fn front_ends_do_not_depend_on_robocol() {
    let lock_path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("Cargo.lock");
    let lock = std::fs::read_to_string(&lock_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", lock_path.display()));
    let graph = dependency_graph(&lock);

    // The daemon does use robocol: proof the graph was read at all.
    assert!(
        reaches(&graph, "ds_daemon", "robocol"),
        "Cargo.lock was not parsed"
    );
    for front_end in ["ds_proto", "ds_agent", "ds_cli"] {
        assert!(
            graph.contains_key(front_end),
            "{front_end} is not in Cargo.lock"
        );
        assert!(
            !reaches(&graph, front_end, "robocol"),
            "{front_end} depends on robocol; only ds_daemon may"
        );
    }
}
