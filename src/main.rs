#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod actions;
mod ai;
mod app;
mod clone;
mod config;
mod gix_clone;
mod secrets;
mod forge;
mod git;
mod git_url;
mod journal;
mod mcp;
mod providers;
mod ui;

use app::MergeFoxApp;

fn main() -> eframe::Result<()> {
    // mergeFox is now git2-free: the backend is gix (read path) + the
    // system `git` binary (write / network path). There are no process-
    // global caches to tune here — gix owns its own object store knobs
    // per-`Repository`, and the system git is tuned via ~/.gitconfig.

    let preferred = preferred_renderer();
    run(preferred).or_else(|err| {
        if matches!(preferred, eframe::Renderer::Glow) {
            eprintln!("mergefox: glow init failed ({err}); retrying with wgpu");
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
