use fs2::FileExt;
use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

const PNPM_PACKAGE_MANAGER: &str = "pnpm@10.6.0";
const FLEET_DASHBOARD_PACKAGE: &str = "@ironmesh/fleet-telemetry";
const EMBEDDED_PUBLIC_FILES: &[&str] = &["ironmesh-favicon.svg"];

fn main() {
    let manifest_dir =
        PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR missing"));
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR missing"));
    let web_workspace_dir = manifest_dir.join("..").join("..").join("web");
    let dashboard_dir = web_workspace_dir.join("apps").join("fleet-telemetry");
    let generated_dist_dir = out_dir.join("fleet-telemetry-dist");
    let generated_assets = out_dir.join("fleet_telemetry_assets.rs");

    emit_rerun_directives(&manifest_dir, &web_workspace_dir, &dashboard_dir);

    // This lock is shared with the other embedded Vite UIs. Cargo may execute several build
    // scripts concurrently, while pnpm installation and Vite output share one worktree.
    let _frontend_build_lock = FrontendBuildLock::acquire(&web_workspace_dir);
    install_frontend_dependencies(&web_workspace_dir);
    build_dashboard(&web_workspace_dir, &generated_dist_dir);
    generate_embedded_assets_module(&generated_dist_dir, &generated_assets);
}

fn emit_rerun_directives(manifest_dir: &Path, web_workspace_dir: &Path, dashboard_dir: &Path) {
    println!("cargo:rerun-if-changed=build.rs");
    for path in [
        manifest_dir.join("..").join("..").join("Cargo.toml"),
        web_workspace_dir.join("package.json"),
        web_workspace_dir.join("pnpm-lock.yaml"),
        web_workspace_dir.join("pnpm-workspace.yaml"),
        web_workspace_dir.join("tsconfig.base.json"),
        dashboard_dir.join("index.html"),
        dashboard_dir.join("package.json"),
        dashboard_dir.join("tsconfig.json"),
        dashboard_dir.join("vite.config.ts"),
        dashboard_dir.join("src"),
        web_workspace_dir.join("packages").join("api").join("src"),
        web_workspace_dir
            .join("packages")
            .join("api")
            .join("package.json"),
        web_workspace_dir
            .join("packages")
            .join("api")
            .join("tsconfig.json"),
        web_workspace_dir
            .join("packages")
            .join("config")
            .join("vite"),
        web_workspace_dir
            .join("packages")
            .join("config")
            .join("src"),
        web_workspace_dir
            .join("packages")
            .join("config")
            .join("package.json"),
        web_workspace_dir
            .join("packages")
            .join("config")
            .join("tsconfig.json"),
        web_workspace_dir.join("packages").join("ui").join("src"),
        web_workspace_dir
            .join("packages")
            .join("ui")
            .join("package.json"),
        web_workspace_dir
            .join("packages")
            .join("ui")
            .join("tsconfig.json"),
        manifest_dir
            .join("..")
            .join("..")
            .join("docs")
            .join("assets"),
    ] {
        println!("cargo:rerun-if-changed={}", path.display());
    }
    println!("cargo:rerun-if-env-changed=PATH");
}

fn install_frontend_dependencies(web_workspace_dir: &Path) {
    let status = run_pnpm_command(web_workspace_dir, &["install", "--frozen-lockfile"])
        .unwrap_or_else(|error| {
            panic!(
                "failed to execute `corepack {PNPM_PACKAGE_MANAGER} install --frozen-lockfile` in {}: {error}. Install a supported Node.js release with Corepack enabled and ensure `corepack` is on PATH.",
                web_workspace_dir.display()
            )
        });
    assert!(
        status.success(),
        "`corepack {PNPM_PACKAGE_MANAGER} install --frozen-lockfile` failed in {}. Resolve the locked frontend dependencies before running cargo again.",
        web_workspace_dir.display()
    );
}

fn build_dashboard(web_workspace_dir: &Path, generated_dist_dir: &Path) {
    if generated_dist_dir.exists() {
        fs::remove_dir_all(generated_dist_dir).unwrap_or_else(|error| {
            panic!(
                "failed cleaning generated fleet dashboard directory {}: {error}",
                generated_dist_dir.display()
            )
        });
    }

    let generated_dist_arg = generated_dist_dir.to_string_lossy().into_owned();
    let status = run_pnpm_command(
        web_workspace_dir,
        &[
            "--filter",
            FLEET_DASHBOARD_PACKAGE,
            "exec",
            "vite",
            "build",
            "--outDir",
            generated_dist_arg.as_str(),
        ],
    )
    .unwrap_or_else(|error| {
        panic!(
            "failed to build the fleet dashboard in {}: {error}",
            web_workspace_dir.display()
        )
    });
    assert!(
        status.success(),
        "fleet dashboard build failed in {}",
        web_workspace_dir.display()
    );
    assert!(
        generated_dist_dir.join("index.html").is_file(),
        "fleet dashboard build did not produce {}/index.html",
        generated_dist_dir.display()
    );
}

fn generate_embedded_assets_module(dist_dir: &Path, generated_file: &Path) {
    let mut assets = Vec::new();
    collect_embedded_assets(dist_dir, dist_dir, &mut assets);
    assets.sort_by(|left, right| left.0.cmp(&right.0));

    let mut source = String::from(
        "pub(crate) fn asset(path: &str) -> Option<(&'static [u8], &'static str, &'static str)> {\n    match path {\n",
    );
    for (relative_path, absolute_path) in assets {
        source.push_str(&format!(
            "        {} => Some((include_bytes!({}), {}, {})),\n",
            rust_string_literal(&relative_path),
            rust_string_literal(&absolute_path.to_string_lossy()),
            rust_string_literal(content_type_for_asset(&relative_path)),
            rust_string_literal(cache_control_for_asset(&relative_path)),
        ));
    }
    source.push_str("        _ => None,\n    }\n}\n");

    fs::write(generated_file, source)
        .unwrap_or_else(|error| panic!("failed writing {}: {error}", generated_file.display()));
}

fn collect_embedded_assets(dir: &Path, root: &Path, assets: &mut Vec<(String, PathBuf)>) {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|error| panic!("failed reading {}: {error}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_embedded_assets(&path, root, assets);
        } else if path.is_file() {
            let relative_path = path
                .strip_prefix(root)
                .expect("asset must remain under generated dashboard directory")
                .to_string_lossy()
                .replace('\\', "/");
            if should_embed_asset(&relative_path) {
                assets.push((relative_path, path));
            }
        }
    }
}

fn should_embed_asset(path: &str) -> bool {
    path == "index.html" || path.starts_with("assets/") || EMBEDDED_PUBLIC_FILES.contains(&path)
}

fn content_type_for_asset(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "text/html; charset=utf-8"
    } else if path.ends_with(".css") {
        "text/css; charset=utf-8"
    } else if path.ends_with(".js") {
        "application/javascript; charset=utf-8"
    } else if path.ends_with(".json") || path.ends_with(".map") {
        "application/json; charset=utf-8"
    } else if path.ends_with(".wasm") {
        "application/wasm"
    } else if path.ends_with(".svg") {
        "image/svg+xml"
    } else if path.ends_with(".png") {
        "image/png"
    } else if path.ends_with(".jpg") || path.ends_with(".jpeg") {
        "image/jpeg"
    } else if path.ends_with(".webp") {
        "image/webp"
    } else if path.ends_with(".woff2") {
        "font/woff2"
    } else if path.ends_with(".woff") {
        "font/woff"
    } else if path.ends_with(".ttf") {
        "font/ttf"
    } else {
        "application/octet-stream"
    }
}

fn cache_control_for_asset(path: &str) -> &'static str {
    if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn rust_string_literal(value: &str) -> String {
    format!("{value:?}")
}

struct FrontendBuildLock {
    file: fs::File,
}

impl FrontendBuildLock {
    fn acquire(web_workspace_dir: &Path) -> Self {
        let lock_path = web_workspace_dir.join(".ironmesh-build.lock");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .unwrap_or_else(|error| {
                panic!(
                    "failed opening frontend build lock {}: {error}",
                    lock_path.display()
                )
            });
        FileExt::lock_exclusive(&file).unwrap_or_else(|error| {
            panic!(
                "failed acquiring frontend build lock {}: {error}",
                lock_path.display()
            )
        });
        Self { file }
    }
}

impl Drop for FrontendBuildLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

fn run_pnpm_command(
    web_workspace_dir: &Path,
    args: &[&str],
) -> Result<std::process::ExitStatus, io::Error> {
    let mut commands = vec!["corepack"];
    if cfg!(windows) {
        commands.insert(0, "corepack.cmd");
    }

    let mut last_error = None;
    for program in commands {
        match Command::new(program)
            .arg(PNPM_PACKAGE_MANAGER)
            .args(args)
            .current_dir(web_workspace_dir)
            .status()
        {
            Ok(status) => return Ok(status),
            Err(error) if error.kind() == io::ErrorKind::NotFound => last_error = Some(error),
            Err(error) => return Err(error),
        }
    }

    Err(last_error.unwrap_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "corepack executable not found")
    }))
}
