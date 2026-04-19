#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod ai;
mod app;
mod clone;
mod clone_auth;
mod config;
mod forge;
mod git;
mod git_url;
mod gix_clone;
mod journal;
mod logging;
mod mcp;
mod preflight;
mod providers;
mod secrets;
mod ui;
mod workspace_profile;

use app::MergeFoxApp;

fn main() -> eframe::Result<()> {
    // mergeFox is now git2-free: the backend is gix (read path) + the
    // system `git` binary (write / network path). There are no process-
    // global caches to tune here — gix owns its own object store knobs
    // per-`Repository`, and the system git is tuned via ~/.gitconfig.

    // Logging must come first so early renderer / init failures are
    // captured. The guard flushes the file appender on drop; keep it
    // alive for the full `main` scope.
    let _log_guard = logging::init();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--mcp-stdio") {
        match mcp::server::repo_path_from_args(&args) {
            Ok(Some(repo_path)) => {
                if let Err(err) = mcp::server::run_stdio(&repo_path) {
                    eprintln!("mergefox --mcp-stdio: {err:#}");
                    std::process::exit(1);
                }
                return Ok(());
            }
            Ok(None) => return Ok(()),
            Err(err) => {
                eprintln!("mergefox: {err:#}");
                mcp::server::print_help();
                std::process::exit(2);
            }
        }
    }

    let preferred = preferred_renderer();
    run(preferred).or_else(|err| {
        if matches!(preferred, eframe::Renderer::Glow) {
            tracing::warn!(error = %err, "glow init failed; retrying with wgpu");
            run(eframe::Renderer::Wgpu)
        } else {
            Err(err)
        }
    })
}

fn run(renderer: eframe::Renderer) -> eframe::Result<()> {
    eframe::run_native(
        "mergefox",
        native_options(renderer),
        Box::new(|cc| Ok(Box::new(MergeFoxApp::new(cc)))),
    )
}

fn native_options(renderer: eframe::Renderer) -> eframe::NativeOptions {
    // Default window: smaller than before (was 1100x700).
    //
    // The `IOSurface` GPU swap chain scales with the window's pixel area;
    // a 1100x700 @ 2x retina is ~3.1 megapixels of backbuffer. 960x620 is
    // ~2.4 megapixels — about 25% smaller GPU resident set just from the
    // default geometry. Users can resize freely; this only affects the
    // first-run / unconfigured-state size.
    //
    // We keep the minimum at 700x450 because anything narrower causes the
    // sidebar + main panel + diff view to not fit at once.
    eframe::NativeOptions {
        renderer,
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([960.0, 620.0])
            .with_min_inner_size([700.0, 450.0])
            .with_icon(std::sync::Arc::new(ui::icon::app_icon()))
            .with_title("mergeFox"),
        ..Default::default()
    }
}

fn preferred_renderer() -> eframe::Renderer {
    match std::env::var("MERGEFOX_RENDERER")
        .ok()
        .as_deref()
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("wgpu") => eframe::Renderer::Wgpu,
        Some("glow") => eframe::Renderer::Glow,
        _ => eframe::Renderer::Glow,
    }
}
