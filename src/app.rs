use std::collections::HashMap;
use std::panic;
use std::path::PathBuf;
use std::thread;

use eframe::egui;

use crate::icons;
use crate::pipeline::{self, CountUpdate, DiscoveredFile};
use crate::progress::{FileResult, FileStatus, ProgressMsg};
use crate::settings::{
    ForceFormat, MipHandling, OutputFormatMode, OutputMode, Quality, ResizeMode, Settings,
};
use crate::squisher::{self, MergeReport};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Resizer,
    Squisher,
}

enum SquishMsg {
    Log(String),
    Progress { done: usize, total: usize },
    Done(Result<MergeReport, String>),
}

struct SquisherState {
    packs: Vec<PathBuf>,
    output: Option<PathBuf>,
    output_not_empty: bool,
    also_resize: bool,
    running: bool,
    log: Vec<String>,
    report: Option<MergeReport>,
    rx: Option<crossbeam_channel::Receiver<SquishMsg>>,
    progress: (usize, usize),
    pending_overflow: Vec<String>,
}

impl SquisherState {
    fn new() -> Self {
        Self {
            packs: Vec::new(),
            output: None,
            output_not_empty: false,
            also_resize: false,
            running: false,
            log: Vec::new(),
            report: None,
            rx: None,
            progress: (0, 0),
            pending_overflow: Vec::new(),
        }
    }

    fn clear(&mut self) {
        *self = Self::new();
    }

    fn set_output(&mut self, dir: PathBuf) {
        self.output_not_empty = std::fs::read_dir(&dir)
            .map(|mut e| e.next().is_some())
            .unwrap_or(false);
        self.output = Some(dir);
    }
}

pub struct App {
    mode: Mode,
    settings: Settings,
    discovered: Vec<DiscoveredFile>,
    status: HashMap<PathBuf, FileStatus>,
    log: Vec<String>,
    results: Vec<FileResult>,
    errors: Vec<(PathBuf, String)>,
    running: bool,
    files_done: usize,
    rx: Option<crossbeam_channel::Receiver<ProgressMsg>>,
    elapsed_secs: Option<f64>,
    skip_text: String,
    squisher: SquisherState,
    count_rx: Option<crossbeam_channel::Receiver<CountUpdate>>,
    count_index: HashMap<PathBuf, usize>,
    counting: bool,
}

impl App {
    pub fn new() -> Self {
        let settings = Settings::load();
        let skip_text = settings.skip_substrings.clone();
        let mut app = Self {
            mode: Mode::Resizer,
            settings,
            discovered: Vec::new(),
            status: HashMap::new(),
            log: Vec::new(),
            results: Vec::new(),
            errors: Vec::new(),
            running: false,
            files_done: 0,
            rx: None,
            elapsed_secs: None,
            skip_text,
            squisher: SquisherState::new(),
            count_rx: None,
            count_index: HashMap::new(),
            counting: false,
        };
        let existing: Vec<PathBuf> = app
            .settings
            .input_folders
            .iter()
            .filter(|d| d.is_dir())
            .cloned()
            .collect();
        if !existing.is_empty() {
            app.rescan(existing);
        }
        app
    }

    fn rescan(&mut self, dirs: Vec<PathBuf>) {
        self.discovered = pipeline::scan_folders(&dirs);
        self.status.clear();
        for f in &self.discovered {
            self.status.insert(f.path.clone(), FileStatus::Pending);
        }
        self.settings.input_folders = dirs;
        self.start_counting();
    }

    fn start_counting(&mut self) {
        self.count_index.clear();
        for (i, f) in self.discovered.iter().enumerate() {
            self.count_index.insert(f.path.clone(), i);
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        self.count_rx = Some(rx);
        self.counting = true;
        let paths: Vec<PathBuf> = self.discovered.iter().map(|f| f.path.clone()).collect();
        thread::spawn(move || {
            pipeline::count_textures_in_background(&paths, &tx);
        });
    }

    fn poll_counts(&mut self) {
        let Some(rx) = &self.count_rx else { return };
        loop {
            match rx.try_recv() {
                Ok(update) => {
                    if let Some(&i) = self.count_index.get(&update.path) {
                        if let Some(f) = self.discovered.get_mut(i) {
                            f.texture_count = update.texture_count;
                            f.parse_error = update.parse_error;
                        }
                    }
                }
                Err(crossbeam_channel::TryRecvError::Empty) => break,
                Err(crossbeam_channel::TryRecvError::Disconnected) => {
                    self.counting = false;
                    break;
                }
            }
        }
    }

    fn total_current_size(&self) -> u64 {
        self.discovered.iter().map(|f| f.size).sum()
    }

    fn start_batch(&mut self) {
        self.settings.skip_substrings = self.skip_text.clone();
        self.settings.save();

        let files: Vec<PathBuf> = self.discovered.iter().map(|f| f.path.clone()).collect();
        for f in &files {
            self.status.insert(f.clone(), FileStatus::Processing);
        }
        self.results.clear();
        self.errors.clear();
        self.log.clear();
        self.files_done = 0;
        self.elapsed_secs = None;
        self.running = true;

        let (tx, rx) = crossbeam_channel::unbounded();
        self.rx = Some(rx);
        let settings = self.settings.clone();
        thread::spawn(move || {
            pipeline::run_batch(&files, settings, tx);
        });
    }

    fn clear_resizer(&mut self) {
        self.discovered.clear();
        self.status.clear();
        self.log.clear();
        self.results.clear();
        self.errors.clear();
        self.files_done = 0;
        self.elapsed_secs = None;
        self.rx = None;
        self.count_rx = None;
        self.count_index.clear();
        self.counting = false;
        self.settings.input_folders.clear();
        self.settings.output_folder = None;
    }

    fn poll_progress(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }
        for msg in messages {
            match msg {
                ProgressMsg::FileStarted { path } => {
                    self.status.insert(path, FileStatus::Processing);
                }
                ProgressMsg::FileFinished { result } => {
                    self.status.insert(result.path.clone(), FileStatus::Done);
                    self.files_done += 1;
                    let pct = if result.old_size > 0 {
                        100.0 * (1.0 - result.new_size as f64 / result.old_size as f64)
                    } else {
                        0.0
                    };
                    self.log.push(format!(
                        "{}: {} -> {} ({:+.1}%) [{}/{} textures resized]",
                        result
                            .path
                            .file_name()
                            .unwrap_or_default()
                            .to_string_lossy(),
                        human_size(result.old_size),
                        human_size(result.new_size),
                        pct,
                        result.textures_resized,
                        result.textures_total,
                    ));
                    for w in &result.warnings {
                        self.log.push(format!("  warning: {w}"));
                    }
                    self.results.push(result);
                }
                ProgressMsg::FileErrored { path, error } => {
                    self.status.insert(path.clone(), FileStatus::Error);
                    self.files_done += 1;
                    self.log.push(format!(
                        "{}: ERROR - {error}",
                        path.file_name().unwrap_or_default().to_string_lossy()
                    ));
                    self.errors.push((path, error));
                }
                ProgressMsg::BatchFinished { elapsed_secs } => {
                    self.running = false;
                    self.elapsed_secs = Some(elapsed_secs);
                }
            }
        }
    }

    fn start_squish(&mut self) {
        if self.squisher.output.is_none() || self.squisher.packs.len() < 2 {
            return;
        }
        let warnings = squisher::preview_overflow_warnings(&self.squisher.packs);
        if !warnings.is_empty() {
            self.squisher.pending_overflow = warnings;
            return;
        }
        self.start_squish_now();
    }

    fn start_squish_now(&mut self) {
        self.squisher.pending_overflow.clear();
        let Some(output) = self.squisher.output.clone() else {
            return;
        };
        if self.squisher.packs.len() < 2 {
            return;
        }
        let packs = self.squisher.packs.clone();
        self.squisher.log.clear();
        self.squisher.report = None;
        self.squisher.progress = (0, 0);
        self.squisher.running = true;

        let (tx, rx) = crossbeam_channel::unbounded();
        self.squisher.rx = Some(rx);
        let resize_settings = if self.squisher.also_resize {
            self.settings.skip_substrings = self.skip_text.clone();
            Some(self.settings.clone())
        } else {
            None
        };

        thread::spawn(move || {
            let tx_log = tx.clone();
            let tx_progress = tx.clone();
            let outcome = panic::catch_unwind(panic::AssertUnwindSafe(|| {
                squisher::merge_packs(
                    &packs,
                    &output,
                    resize_settings.as_ref(),
                    move |line| {
                        let _ = tx_log.send(SquishMsg::Log(line));
                    },
                    move |done, total| {
                        let _ = tx_progress.send(SquishMsg::Progress { done, total });
                    },
                )
            }));
            let result = match outcome {
                Ok(r) => r.map_err(|e| e.to_string()),
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| s.to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".to_string());
                    Err(format!("internal error: {msg}"))
                }
            };
            let _ = tx.send(SquishMsg::Done(result));
        });
    }

    fn poll_squish(&mut self) {
        let Some(rx) = &self.squisher.rx else { return };
        let mut messages = Vec::new();
        while let Ok(msg) = rx.try_recv() {
            messages.push(msg);
        }
        for msg in messages {
            match msg {
                SquishMsg::Log(line) => self.squisher.log.push(line),
                SquishMsg::Progress { done, total } => self.squisher.progress = (done, total),
                SquishMsg::Done(Ok(report)) => {
                    self.squisher.log.push(format!(
                        "done: {} assets copied, {} renumbered, {} other files copied, {} warnings",
                        report.assets_copied,
                        report.assets_renumbered,
                        report.other_files_copied,
                        report.warnings.len(),
                    ));
                    for w in &report.warnings {
                        self.squisher.log.push(format!("  warning: {w}"));
                    }
                    self.squisher.report = Some(report);
                    self.squisher.running = false;
                }
                SquishMsg::Done(Err(e)) => {
                    self.squisher.log.push(format!("ERROR: {e}"));
                    self.squisher.running = false;
                }
            }
        }
    }

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if dropped.is_empty() {
            return;
        }
        if dropped.iter().all(|p| p.is_dir()) {
            self.rescan(dropped);
        } else {
            let files: Vec<DiscoveredFile> = dropped
                .into_iter()
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("ytd"))
                        == Some(true)
                })
                .map(|path| {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    DiscoveredFile {
                        path,
                        size,
                        texture_count: None,
                        parse_error: None,
                    }
                })
                .collect();
            self.status.clear();
            for f in &files {
                self.status.insert(f.path.clone(), FileStatus::Pending);
            }
            self.discovered = files;
            self.start_counting();
        }
    }

    fn squisher_ui(&mut self, ui: &mut egui::Ui) {
        egui::Panel::left("squisher_settings")
            .resizable(true)
            .default_size(320.0)
            .show(ui, |ui| {
                ui.heading("Packs (priority order - earlier wins on collision)");
                ui.add_space(4.0);

                let mut remove_idx = None;
                let squisher_running = self.squisher.running;
                ui.add_enabled_ui(!squisher_running, |ui| {
                    for (i, pack) in self.squisher.packs.iter().enumerate() {
                        ui.horizontal(|ui| {
                            ui.label(format!("{}.", i + 1));
                            ui.label(pack.display().to_string());
                            if icons::close_button(ui, 18.0).clicked() {
                                remove_idx = Some(i);
                            }
                        });
                    }
                });
                if let Some(i) = remove_idx {
                    self.squisher.packs.remove(i);
                }

                ui.add_enabled_ui(!squisher_running, |ui| {
                    if icons::icon_button(ui, |ui, s, c| icons::plus(ui, s, c), "Add Pack(s)...").clicked() {
                        if let Some(dirs) = rfd::FileDialog::new().pick_folders() {
                            self.squisher.packs.extend(dirs);
                        }
                    }
                });
                if self.squisher.packs.len() < 2 {
                    ui.label(
                        egui::RichText::new("Add at least two packs.")
                            .weak()
                            .small(),
                    );
                }

                ui.separator();
                ui.heading("Output");
                ui.add_enabled_ui(!squisher_running, |ui| {
                    if icons::icon_button(ui, |ui, s, c| icons::folder(ui, s, c), "Select Folder...").clicked() {
                        if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                            self.squisher.set_output(dir);
                        }
                    }
                });
                if let Some(p) = &self.squisher.output {
                    ui.label(p.display().to_string());
                    if self.squisher.output_not_empty {
                        ui.colored_label(
                            egui::Color32::from_rgb(240, 180, 60),
                            "Warning: this folder is not empty. Files may be overwritten or mixed in with existing content.",
                        );
                    }
                }

                ui.separator();
                ui.add_enabled_ui(!squisher_running, |ui| {
                    ui.checkbox(&mut self.squisher.also_resize, "Also resize while merging");
                });
                if self.squisher.also_resize {
                    ui.label(
                        egui::RichText::new("Uses the Resizer tab's current settings.")
                            .weak()
                            .small(),
                    );
                }

                ui.separator();
                if !self.squisher.pending_overflow.is_empty() {
                    for w in &self.squisher.pending_overflow {
                        ui.colored_label(egui::Color32::from_rgb(240, 90, 60), w);
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Squish anyway").clicked() {
                            self.start_squish_now();
                        }
                        if ui.button("Cancel").clicked() {
                            self.squisher.pending_overflow.clear();
                        }
                    });
                }
                let can_start = !self.squisher.running
                    && self.squisher.packs.len() >= 2
                    && self.squisher.output.is_some()
                    && self.squisher.pending_overflow.is_empty();
                ui.horizontal(|ui| {
                    if ui
                        .add_enabled(can_start, egui::Button::new("Squish"))
                        .clicked()
                    {
                        self.start_squish();
                    }
                    let has_data = !self.squisher.packs.is_empty()
                        || self.squisher.output.is_some()
                        || !self.squisher.log.is_empty()
                        || self.squisher.report.is_some();
                    if ui
                        .add_enabled(!self.squisher.running && has_data, egui::Button::new("Clear"))
                        .clicked()
                    {
                        self.squisher.clear();
                    }
                });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading("Merge log");
            ui.label(
                egui::RichText::new(
                    "Squishing folds packs in order - pack 1's ids always win, pack 2's win over \
                 pack 3's and 4's, and so on. Each later pack's colliding drawable/texture ids \
                 get renamed to free slots (filename-based, same trick tools like Durty Cloth \
                 Tool use) and fxmanifest.lua is merged textually. Review the log for renumbered \
                 files and manifest warnings before shipping the merged pack.",
                )
                .weak(),
            );
            ui.separator();

            if self.squisher.running {
                let (done, total) = self.squisher.progress;
                if total > 0 {
                    ui.add(egui::ProgressBar::new(done as f32 / total as f32).show_percentage());
                    ui.label(format!("{done}/{total} files"));
                } else {
                    ui.label("Scanning packs...");
                }
                ui.separator();
            }

            if let Some(report) = &self.squisher.report {
                ui.label(format!(
                    "{} assets copied, {} renumbered, {} other files copied, {} warnings",
                    report.assets_copied,
                    report.assets_renumbered,
                    report.other_files_copied,
                    report.warnings.len(),
                ));
                if let Some((resized, total)) = report.resize_stats {
                    ui.label(format!("resize pass: {resized}/{total} textures resized"));
                }
                ui.separator();
            }
            egui::ScrollArea::vertical()
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    for line in &self.squisher.log {
                        ui.label(egui::RichText::new(line).monospace().small());
                    }
                });
        });
    }
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let ctx = &ctx;
        if self.running {
            self.poll_progress();
            ctx.request_repaint();
        }
        if self.squisher.running {
            self.poll_squish();
            ctx.request_repaint();
        }
        if self.counting {
            self.poll_counts();
            ctx.request_repaint();
        }
        if self.mode == Mode::Resizer {
            self.handle_drops(ctx);
        }

        egui::Panel::top("top_panel").show(ui, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("EUP Texture Resizer");
                ui.add_space(12.0);
                ui.selectable_value(&mut self.mode, Mode::Resizer, "Resizer");
                ui.selectable_value(&mut self.mode, Mode::Squisher, "Squisher (merge packs)");
            });
            if self.mode == Mode::Resizer {
                ui.horizontal(|ui| {
                    if icons::icon_button(
                        ui,
                        |ui, s, c| icons::folder(ui, s, c),
                        "Select Input Folder(s)...",
                    )
                    .clicked()
                    {
                        if let Some(dirs) = rfd::FileDialog::new().pick_folders() {
                            self.rescan(dirs);
                        }
                    }
                    match self.settings.input_folders.len() {
                        0 => {}
                        1 => {
                            ui.label(format!("In: {}", self.settings.input_folders[0].display()));
                        }
                        n => {
                            ui.label(format!("In: {n} folders"));
                        }
                    }
                });
                let overwrite_in_place = matches!(
                    self.settings.output_mode,
                    OutputMode::OverwriteInPlace { .. }
                );
                ui.add_enabled_ui(!overwrite_in_place, |ui| {
                    ui.horizontal(|ui| {
                        if icons::icon_button(
                            ui,
                            |ui, s, c| icons::folder(ui, s, c),
                            "Select Output Folder...",
                        )
                        .clicked()
                        {
                            if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                                self.settings.output_folder = Some(dir);
                            }
                        }
                        if let Some(dir) = &self.settings.output_folder {
                            ui.label(format!("Out: {}", dir.display()));
                        }
                    });
                });
                if overwrite_in_place {
                    ui.label(
                        egui::RichText::new("Not used in \"Overwrite in place\" mode.")
                            .weak()
                            .small(),
                    );
                }
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Drop a folder or .ytd files here").weak());
            }
            ui.add_space(6.0);
        });

        if self.mode == Mode::Squisher {
            self.squisher_ui(ui);
            return;
        }

        egui::Panel::left("settings_panel").resizable(true).default_size(300.0).show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.heading("Resize");
                ui.radio_value(&mut self.settings.resize_mode, ResizeMode::CapResolution, "Cap resolution");
                if self.settings.resize_mode == ResizeMode::CapResolution {
                    ui.horizontal(|ui| {
                        for preset in [2048u32, 1024, 512, 256] {
                            if ui.selectable_label(self.settings.cap_resolution == preset, preset.to_string()).clicked() {
                                self.settings.cap_resolution = preset;
                            }
                        }
                    });
                    ui.add(egui::DragValue::new(&mut self.settings.cap_resolution).range(4..=8192).prefix("custom: "));
                }
                ui.radio_value(&mut self.settings.resize_mode, ResizeMode::ScalePercent, "Scale percentage");
                if self.settings.resize_mode == ResizeMode::ScalePercent {
                    ui.add(egui::Slider::new(&mut self.settings.scale_percent, 1..=100).suffix("%"));
                    let example = (2048.0 * self.settings.scale_percent as f64 / 100.0).round() as u32;
                    ui.label(format!("e.g. 2048 -> {example}"));
                }

                ui.separator();
                ui.heading("Output format");
                let mut force = matches!(self.settings.output_format, OutputFormatMode::Force(_));
                if ui.radio(!force, "Keep original format per texture").clicked() {
                    self.settings.output_format = OutputFormatMode::KeepOriginal;
                    force = false;
                }
                if ui.radio(force, "Force format").clicked() && !force {
                    self.settings.output_format = OutputFormatMode::Force(ForceFormat::Bc7);
                }
                if let OutputFormatMode::Force(current) = &mut self.settings.output_format {
                    egui::ComboBox::from_id_salt("force_format")
                        .selected_text(current.label())
                        .show_ui(ui, |ui| {
                            for f in ForceFormat::ALL {
                                ui.selectable_value(current, f, f.label());
                            }
                        });
                }

                ui.separator();
                ui.heading("Compression quality");
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.settings.quality, Quality::Fast, "Fast");
                    ui.selectable_value(&mut self.settings.quality, Quality::Medium, "Medium");
                    ui.selectable_value(&mut self.settings.quality, Quality::Slow, "Slow / High quality");
                });
                ui.label(egui::RichText::new("Only affects BC7 encoding; BC1/BC3/BC4/BC5 use a single fixed algorithm.").weak().small());

                ui.separator();
                ui.heading("Mipmaps");
                ui.radio_value(&mut self.settings.mip_handling, MipHandling::Regenerate, "Regenerate full chain");
                ui.radio_value(&mut self.settings.mip_handling, MipHandling::Strip, "Strip (single level)");
                ui.radio_value(&mut self.settings.mip_handling, MipHandling::PreserveCount, "Preserve original count");

                ui.separator();
                ui.heading("Limits");
                ui.horizontal(|ui| {
                    ui.label("Minimum size floor:");
                    ui.add(egui::DragValue::new(&mut self.settings.min_size_floor).range(4..=512).suffix("px"));
                });
                ui.label("Skip textures containing (comma-separated):");
                ui.text_edit_singleline(&mut self.skip_text);

                ui.separator();
                ui.heading("Output");
                let mut overwrite = matches!(self.settings.output_mode, OutputMode::OverwriteInPlace { .. });
                if ui.radio(!overwrite, "Separate output folder (non-destructive)").clicked() {
                    self.settings.output_mode = OutputMode::SeparateFolder;
                    overwrite = false;
                }
                if ui.radio(overwrite, "Overwrite in place").clicked() && !overwrite {
                    self.settings.output_mode = OutputMode::OverwriteInPlace { backup: true };
                }
                if let OutputMode::OverwriteInPlace { backup } = &mut self.settings.output_mode {
                    ui.checkbox(backup, "Back up originals to .bak\\ first");
                }

                ui.separator();
                ui.checkbox(&mut self.settings.dry_run, "Dry run (preview only, write nothing)");
            });
        });

        egui::Panel::bottom("bottom_panel")
            .min_size(200.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let can_start = !self.discovered.is_empty()
                        && !self.running
                        && (self.settings.dry_run
                            || matches!(
                                self.settings.output_mode,
                                OutputMode::OverwriteInPlace { .. }
                            )
                            || self.settings.output_folder.is_some());
                    if ui
                        .add_enabled(
                            can_start,
                            egui::Button::new(if self.settings.dry_run {
                                "Preview"
                            } else {
                                "Start"
                            }),
                        )
                        .clicked()
                    {
                        self.start_batch();
                    }
                    let has_data = !self.discovered.is_empty()
                        || !self.log.is_empty()
                        || !self.results.is_empty()
                        || !self.errors.is_empty()
                        || self.settings.output_folder.is_some();
                    if ui
                        .add_enabled(!self.running && has_data, egui::Button::new("Clear"))
                        .clicked()
                    {
                        self.clear_resizer();
                    }
                    if self.running {
                        let total = self.discovered.len().max(1);
                        ui.add(
                            egui::ProgressBar::new(self.files_done as f32 / total as f32)
                                .show_percentage(),
                        );
                        ui.label(format!("{}/{}", self.files_done, self.discovered.len()));
                    }
                });

                if let Some(elapsed) = self.elapsed_secs {
                    let total_old: u64 = self.results.iter().map(|r| r.old_size).sum();
                    let total_new: u64 = self.results.iter().map(|r| r.new_size).sum();
                    let saved = total_old.saturating_sub(total_new);
                    let pct = if total_old > 0 {
                        100.0 * saved as f64 / total_old as f64
                    } else {
                        0.0
                    };
                    ui.separator();
                    ui.label(format!(
                        "Done: {} files, saved {} ({:.1}%), {:.1}s elapsed, {} errors",
                        self.results.len(),
                        human_size(saved),
                        pct,
                        elapsed,
                        self.errors.len(),
                    ));
                }

                ui.separator();
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .max_height(150.0)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.label(egui::RichText::new(line).monospace().small());
                        }
                    });
            });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.heading(format!(
                "Files ({}, {})",
                self.discovered.len(),
                human_size(self.total_current_size())
            ));
            egui::ScrollArea::vertical().show(ui, |ui| {
                egui::Grid::new("file_grid")
                    .num_columns(4)
                    .striped(true)
                    .show(ui, |ui| {
                        ui.strong("Status");
                        ui.strong("File");
                        ui.strong("Textures");
                        ui.strong("Size");
                        ui.end_row();
                        for f in &self.discovered {
                            let status = self
                                .status
                                .get(&f.path)
                                .copied()
                                .unwrap_or(FileStatus::Pending);
                            match status {
                                FileStatus::Pending => {
                                    icons::pending(ui, 16.0, egui::Color32::GRAY)
                                }
                                FileStatus::Processing => {
                                    icons::spinner(ui, 16.0, egui::Color32::from_rgb(240, 180, 60))
                                }
                                FileStatus::Done => {
                                    icons::check(ui, 16.0, egui::Color32::from_rgb(90, 200, 120))
                                }
                                FileStatus::Error => {
                                    icons::cross(ui, 16.0, egui::Color32::from_rgb(220, 90, 90))
                                }
                            }
                            let name = f.path.file_name().unwrap_or_default().to_string_lossy();
                            if let Some(err) = &f.parse_error {
                                ui.colored_label(
                                    egui::Color32::from_rgb(220, 90, 90),
                                    format!("{name} ({err})"),
                                );
                            } else {
                                ui.label(name.as_ref());
                            }
                            match f.texture_count {
                                Some(n) => ui.label(n.to_string()),
                                None => ui.label(egui::RichText::new("...").weak()),
                            };
                            ui.label(human_size(f.size));
                            ui.end_row();
                        }
                    });
            });
        });
    }

    fn on_exit(&mut self) {
        self.settings.skip_substrings = self.skip_text.clone();
        self.settings.save();
    }
}
