use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::ytd_io::TextureFormat as YtdFormat;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ResizeMode {
    CapResolution,
    ScalePercent,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OutputFormatMode {
    KeepOriginal,
    Force(ForceFormat),
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum ForceFormat {
    Bc1,
    Bc2,
    Bc3,
    Bc7,
}

impl ForceFormat {
    pub fn to_ytd_format(self) -> YtdFormat {
        match self {
            ForceFormat::Bc1 => YtdFormat::DXT1,
            ForceFormat::Bc2 => YtdFormat::DXT3,
            ForceFormat::Bc3 => YtdFormat::DXT5,
            ForceFormat::Bc7 => YtdFormat::BC7,
        }
    }

    pub const ALL: [ForceFormat; 4] = [
        ForceFormat::Bc1,
        ForceFormat::Bc2,
        ForceFormat::Bc3,
        ForceFormat::Bc7,
    ];

    pub fn label(self) -> &'static str {
        match self {
            ForceFormat::Bc1 => "BC1 / DXT1 (no alpha, smallest)",
            ForceFormat::Bc2 => "BC2 / DXT3 (sharp alpha)",
            ForceFormat::Bc3 => "BC3 / DXT5 (smooth alpha)",
            ForceFormat::Bc7 => "BC7 (best quality)",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum Quality {
    Fast,
    Medium,
    Slow,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum MipHandling {
    Regenerate,
    Strip,
    PreserveCount,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum OutputMode {
    SeparateFolder,
    OverwriteInPlace { backup: bool },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub resize_mode: ResizeMode,
    pub cap_resolution: u32,
    pub scale_percent: u32,
    pub output_format: OutputFormatMode,
    pub quality: Quality,
    pub mip_handling: MipHandling,
    pub min_size_floor: u32,
    pub skip_substrings: String,
    pub output_mode: OutputMode,
    pub dry_run: bool,
    #[serde(default)]
    pub input_folder: Option<PathBuf>,
    #[serde(default)]
    pub input_folders: Vec<PathBuf>,
    #[serde(default)]
    pub output_folder: Option<PathBuf>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            resize_mode: ResizeMode::CapResolution,
            cap_resolution: 1024,
            scale_percent: 50,
            output_format: OutputFormatMode::KeepOriginal,
            quality: Quality::Medium,
            mip_handling: MipHandling::Regenerate,
            min_size_floor: 64,
            skip_substrings: String::new(),
            output_mode: OutputMode::SeparateFolder,
            dry_run: false,
            input_folder: None,
            input_folders: Vec::new(),
            output_folder: None,
        }
    }
}

impl Settings {
    fn config_path() -> Option<PathBuf> {
        let dirs = directories::ProjectDirs::from("com", "eup-resizer", "eup_resizer")?;
        Some(dirs.config_dir().join("settings.json"))
    }

    pub fn load() -> Self {
        let mut settings: Self = Self::config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        if settings.input_folders.is_empty() {
            if let Some(dir) = settings.input_folder.take() {
                settings.input_folders.push(dir);
            }
        }
        settings
    }

    pub fn save(&self) {
        let Some(path) = Self::config_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, json);
        }
    }

    pub fn skip_list(&self) -> Vec<String> {
        self.skip_substrings
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect()
    }
}
