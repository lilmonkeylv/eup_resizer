#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod icons;
mod pipeline;
mod progress;
mod settings;
mod squisher;
mod ytd_io;

use eframe::egui;

const ICON_PNG: &[u8] = include_bytes!("../icon.png");

fn app_icon() -> egui::IconData {
    match image::load_from_memory(ICON_PNG) {
        Ok(img) => {
            let rgba = img.to_rgba8();
            let (width, height) = rgba.dimensions();
            egui::IconData {
                rgba: rgba.into_raw(),
                width,
                height,
            }
        }
        Err(e) => {
            eprintln!("[icon] failed to decode icon.png, using fallback: {e}");
            let size = 32;
            egui::IconData {
                rgba: vec![26u8, 26, 30, 255].repeat((size * size) as usize),
                width: size,
                height: size,
            }
        }
    }
}

#[cfg(windows)]
fn raise_process_priority() {
    use windows_sys::Win32::System::Threading::{
        ABOVE_NORMAL_PRIORITY_CLASS, GetCurrentProcess, SetPriorityClass,
    };
    unsafe {
        SetPriorityClass(GetCurrentProcess(), ABOVE_NORMAL_PRIORITY_CLASS);
    }
}

#[cfg(not(windows))]
fn raise_process_priority() {}

fn configure_thread_pool() {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4);
    let worker_threads = cores.saturating_sub(1).max(1);
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(worker_threads)
        .build_global();
}

fn apply_dark_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    let accent = egui::Color32::from_rgb(90, 169, 230);
    let selected_bg = egui::Color32::from_rgb(40, 104, 158);
    let selected_text = egui::Color32::from_rgb(232, 243, 255);

    visuals.panel_fill = egui::Color32::from_rgb(26, 26, 30);
    visuals.window_fill = egui::Color32::from_rgb(26, 26, 30);
    visuals.extreme_bg_color = egui::Color32::from_rgb(18, 18, 21);
    visuals.faint_bg_color = egui::Color32::from_rgb(32, 32, 37);
    visuals.hyperlink_color = accent;

    visuals.selection.bg_fill = selected_bg;
    visuals.selection.stroke = egui::Stroke::new(1.0, selected_text);
    visuals.widgets.active.bg_fill = selected_bg;
    visuals.widgets.active.fg_stroke = egui::Stroke::new(2.0, selected_text);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_rgb(44, 44, 50);

    ctx.set_visuals(visuals);
}

fn main() -> eframe::Result<()> {
    raise_process_priority();
    configure_thread_pool();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 720.0])
            .with_min_inner_size([800.0, 500.0])
            .with_icon(app_icon()),
        ..Default::default()
    };

    eframe::run_native(
        "EUP Texture Resizer",
        native_options,
        Box::new(|cc| {
            apply_dark_theme(&cc.egui_ctx);
            Ok(Box::new(app::App::new()))
        }),
    )
}
