//! Release archive layout invariants.
//!
//! cargo-dist archives each executable at the archive root. cargo-binstall
//! needs the corresponding root-relative path to install a release artifact.

use std::path::PathBuf;

fn cargo_manifest() -> toml::Value {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
    let raw = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} must be readable: {error}", path.display()));
    toml::from_str(&raw)
        .unwrap_or_else(|error| panic!("{} must be valid TOML: {error}", path.display()))
}

#[test]
fn cargo_binstall_uses_the_flat_cargo_dist_archive_layout() {
    let manifest = cargo_manifest();
    let binstall = &manifest["package"]["metadata"]["binstall"];

    assert_eq!(
        binstall["bin-dir"].as_str(),
        Some("{ bin }"),
        "Unix cargo-dist archives place the executable at the archive root"
    );

    let windows = &binstall["overrides"]["x86_64-pc-windows-msvc"];
    assert_eq!(
        windows["bin-dir"].as_str(),
        Some("{ bin }{ binary-ext }"),
        "Windows cargo-binstall must select the root executable with its extension"
    );
}

#[test]
fn release_contains_no_unsupported_host_or_handwritten_installer_artifacts() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));

    for artifact in ["hooks.json", "plugin.json", "install.sh", "install.ps1"] {
        assert!(
            !root.join(artifact).exists(),
            "{artifact} is a removed root artifact: supported hosts use their own manifests and cargo-dist owns installers"
        );
    }

    for manifest in [".claude-plugin/plugin.json", ".codex-plugin/plugin.json"] {
        assert!(
            root.join(manifest).is_file(),
            "{manifest} is a supported-host manifest and must ship"
        );
    }

    let workflow = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("release workflow must be readable");
    assert!(
        !workflow.contains("cp install.sh install.ps1 artifacts/"),
        "cargo-dist generates the release installers; do not upload handwritten copies"
    );
}

#[test]
fn tag_release_refreshes_and_verifies_the_dashboard_before_dist_build() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workflow = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("release workflow must be readable");

    let frozen_install = workflow
        .find("pnpm install --frozen-lockfile")
        .expect("tag releases must install dashboard dependencies from the locked manifest");
    let dashboard_build = workflow
        .find("pnpm run build")
        .expect("tag releases must build the dashboard before packaging");
    let byte_comparison = workflow
        .find("readFileSync('app/dist/index.html').equals(readFileSync('assets/dashboard.html'))")
        .expect("tag releases must compare the built dashboard and embedded asset byte-for-byte");
    let dist_build = workflow
        .find("dist build ${{ needs.plan.outputs.tag-flag }}")
        .expect("release workflow must invoke cargo-dist");

    assert!(
        frozen_install < dashboard_build
            && dashboard_build < byte_comparison
            && byte_comparison < dist_build,
        "tag releases must frozen-install, build, and byte-compare the dashboard before cargo-dist builds artifacts"
    );
    assert!(
        workflow.contains("node -e \"const { readFileSync } = require('node:fs');"),
        "use Node's built-in byte comparison so the release gate works on Windows, macOS, and Linux"
    );
}

#[test]
fn contributing_uses_the_path_installed_binary_for_plugin_testing() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let contributing = std::fs::read_to_string(root.join("CONTRIBUTING.md"))
        .expect("CONTRIBUTING.md must be readable");

    assert!(
        contributing.contains("cargo install --path . --force"),
        "contributors must install the local binary on PATH before testing plugin hooks"
    );
    assert!(
        !contributing.contains("hooks/bin/"),
        "plugin hooks resolve epic-harness from PATH; hooks/bin is not a maintained test path"
    );
}
