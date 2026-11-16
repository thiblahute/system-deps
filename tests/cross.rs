//! Builds a crate for another target with the prebuilt binaries, as a real build does: the
//! bundle that is picked must be the target's, not the one of the machine running the build script.
#![cfg(unix)]

use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
};

const TARGETS: [&str; 2] = ["x86_64-linux-android", "aarch64-linux-android"];

fn installed(target: &str) -> bool {
    Command::new("rustc")
        .args(["--print", "target-libdir", "--target", target])
        .output()
        .ok()
        .map(|o| Path::new(String::from_utf8_lossy(&o.stdout).trim()).is_dir())
        .unwrap_or(false)
}

/// An archive with one library, whose link path tells which archive it came from.
fn archive(dir: &Path, name: &str) -> PathBuf {
    let tree = dir.join(format!("{name}-tree"));
    let pc = tree.join("lib/pkgconfig");
    fs::create_dir_all(&pc).unwrap();
    fs::write(
        pc.join("xfixture.pc"),
        format!(
            "libdir=/bundle-{name}/lib\n\nName: xfixture\nDescription: x\nVersion: 1.0.0\nLibs: -L${{libdir}} -lxfixture\n"
        ),
    )
    .unwrap();
    let file = dir.join(format!("{name}.tar.gz"));
    let status = Command::new("tar")
        .arg("czf")
        .arg(&file)
        .arg("-C")
        .arg(&tree)
        .arg(".")
        .status()
        .unwrap();
    assert!(status.success());
    file
}

fn sha256(file: &Path) -> String {
    for cmd in [&["sha256sum"][..], &["shasum", "-a", "256"][..]] {
        if let Ok(out) = Command::new(cmd[0]).args(&cmd[1..]).arg(file).output() {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout)
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .to_owned();
            }
        }
    }
    panic!("no sha256sum or shasum");
}

/// Link paths the fixture's build script found, when built for `target` (or for the host).
fn link_paths(name: &str, target: Option<&str>) -> Vec<String> {
    let dir = std::env::temp_dir().join(format!("system_deps_cross_{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    let android = archive(&dir, "android");
    let other = archive(&dir, "other");

    fs::write(dir.join("src/lib.rs"), "").unwrap();
    fs::write(
        dir.join("build.rs"),
        r#"fn main() {
            let deps = system_deps::Config::new().probe().unwrap();
            for path in deps.all_link_paths() {
                println!("cargo:warning=XDEPS-LINK={}", path.display());
            }
        }"#,
    )
    .unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        format!(
            r#"[workspace]

[package]
name = "xfixture"
version = "0.1.0"
edition = "2021"

[build-dependencies]
system-deps = {{ path = "{root}", features = ["binary", "gz"] }}

[package.metadata.system-deps.xfixture]
name = "xfixture"
version = "1"

[package.metadata.system-deps.xfixture.'cfg(target_os = "android")']
url = "file://{android}"
checksum = "{android_sum}"

[package.metadata.system-deps.xfixture.'cfg(not(target_os = "android"))']
url = "file://{other}"
checksum = "{other_sum}"
"#,
            root = env!("CARGO_MANIFEST_DIR"),
            android = android.display(),
            android_sum = sha256(&android),
            other = other.display(),
            other_sum = sha256(&other),
        ),
    )
    .unwrap();

    let mut cmd = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()));
    cmd.arg("check")
        .arg("--manifest-path")
        .arg(dir.join("Cargo.toml"))
        .env("CARGO_TARGET_DIR", dir.join("target"))
        .env("PKG_CONFIG_ALLOW_CROSS", "1");
    if let Some(target) = target {
        cmd.args(["--target", target]);
    }
    for (key, _) in std::env::vars() {
        if key.starts_with("SYSTEM_DEPS_") || key == "TARGET" || key.starts_with("CARGO_BUILD") {
            cmd.env_remove(key);
        }
    }
    let out = cmd.output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "{}", stderr);
    stderr
        .lines()
        .filter_map(|l| l.split("XDEPS-LINK=").nth(1))
        .map(|p| p.trim().to_owned())
        .collect()
}

#[test]
fn the_target_s_bundle_is_picked_when_cross_compiling() {
    let mut ran = 0;
    for target in TARGETS {
        if !installed(target) {
            assert!(
                std::env::var_os("SYSTEM_DEPS_REQUIRE_CROSS").is_none(),
                "the target {} is not installed",
                target
            );
            continue;
        }
        ran += 1;
        assert_eq!(
            link_paths(target, Some(target)),
            ["/bundle-android/lib"],
            "{target}"
        );
    }
    if ran == 0 {
        eprintln!("no Android target is installed: nothing was cross compiled");
    }
}

#[test]
fn the_host_s_bundle_is_picked_without_a_target() {
    assert_eq!(link_paths("host", None), ["/bundle-other/lib"]);
}
