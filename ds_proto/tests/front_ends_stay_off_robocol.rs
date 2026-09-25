//! Only `ds_daemon` may talk to the robot. The front ends reach it through
//! `ds_proto`, so a front end that quietly depended on `robocol` could reach
//! around the daemon. This holds that line in the manifests.

use std::path::Path;

#[test]
fn front_ends_do_not_depend_on_robocol() {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root");
    for krate in ["ds_proto", "ds_agent", "ds_cli"] {
        let manifest = std::fs::read_to_string(workspace.join(krate).join("Cargo.toml"))
            .unwrap_or_else(|e| panic!("read {krate}/Cargo.toml: {e}"));
        let dependencies = manifest.split("[dependencies]").nth(1).unwrap_or_default();
        assert!(
            !dependencies
                .lines()
                .any(|line| line.trim_start().starts_with("robocol")),
            "{krate} depends on robocol; only ds_daemon may"
        );
    }
}
