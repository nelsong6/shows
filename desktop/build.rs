use std::path::Path;
use std::process::Command;

fn main() {
    embed_resource::compile("resources.rc", embed_resource::NONE);
    // A release embeds the React overlay (webserver.rs); a debug build serves it from Vite.
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        build_frontend();
    }
}

fn build_frontend() {
    let frontend = Path::new(env!("CARGO_MANIFEST_DIR")).join("../frontend");
    for input in [
        "src",
        "test",
        "public/favicon.ico",
        "index.html",
        "package.json",
        "package-lock.json",
        "tsconfig.json",
        "tsconfig.node.json",
        "vite.config.ts",
        "scripts",
    ] {
        println!("cargo:rerun-if-changed={}", frontend.join(input).display());
    }
    if !frontend.join("node_modules").exists() {
        npm(&frontend, &["ci", "--no-audit", "--no-fund"]);
    }
    if !frontend.join("public/fzt/fzt.wasm").exists() {
        npm(&frontend, &["run", "fetch-fzt"]);
    }
    // `npm run build` runs the overlay's tests first (prebuild).
    npm(&frontend, &["run", "build"]);
}

fn npm(directory: &Path, arguments: &[&str]) {
    let program = if cfg!(windows) { "npm.cmd" } else { "npm" };
    let status = Command::new(program)
        .args(arguments)
        .current_dir(directory)
        .status()
        .unwrap_or_else(|error| panic!("running npm {}: {error}", arguments.join(" ")));
    assert!(
        status.success(),
        "npm {} failed: {status}",
        arguments.join(" ")
    );
}
