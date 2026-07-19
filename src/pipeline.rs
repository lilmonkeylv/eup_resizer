use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Result, anyhow};
use image::RgbaImage;
use image::imageops::FilterType;
use intel_tex_2::{RSurface, RgSurface, RgbaSurface, bc1, bc3, bc4, bc5, bc7};
use rayon::prelude::*;

use crate::progress::{FileResult, ProgressMsg};
use crate::settings::{MipHandling, OutputFormatMode, OutputMode, Quality, ResizeMode, Settings};
use crate::ytd_io::{self, ParsedYtd, RawTexture, TextureFormat, TexturePatch};

#[derive(Debug, Clone)]
pub struct DiscoveredFile {
    pub path: PathBuf,
    pub size: u64,
    pub texture_count: Option<usize>,
    pub parse_error: Option<String>,
}

fn is_ytd_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ytd"))
        == Some(true)
}

fn ytd_entries_in(folder: &Path) -> Vec<(PathBuf, u64)> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };

    let mut files = Vec::new();
    let mut dirs = Vec::new();
    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(entry.path());
            continue;
        }
        let path = entry.path();
        if is_ytd_file(&path) {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            files.push((path, size));
        }
    }

    files.extend(
        dirs.par_iter()
            .flat_map(|dir| ytd_entries_in(dir))
            .collect::<Vec<_>>(),
    );
    files
}

fn count_textures_fast(path: &Path) -> Result<usize> {
    ytd_io::count_textures_from_path(path).or_else(|_| ytd_io::count_textures(&fs::read(path)?))
}

pub fn scan_folders(folders: &[PathBuf]) -> Vec<DiscoveredFile> {
    let entries: Vec<(PathBuf, u64)> = folders
        .par_iter()
        .flat_map(|folder| ytd_entries_in(folder))
        .collect();
    let mut out: Vec<DiscoveredFile> = entries
        .into_iter()
        .map(|(path, size)| DiscoveredFile {
            path,
            size,
            texture_count: None,
            parse_error: None,
        })
        .collect();
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

#[derive(Debug, Clone)]
pub struct CountUpdate {
    pub path: PathBuf,
    pub texture_count: Option<usize>,
    pub parse_error: Option<String>,
}

pub fn count_textures_in_background(
    files: &[PathBuf],
    tx: &crossbeam_channel::Sender<CountUpdate>,
) {
    files.par_iter().for_each(|path| {
        let (texture_count, parse_error) = match count_textures_fast(path) {
            Ok(count) => (Some(count), None),
            Err(e) => (None, Some(e.to_string())),
        };
        let _ = tx.send(CountUpdate {
            path: path.clone(),
            texture_count,
            parse_error,
        });
    });
}

fn is_resizable_format(fmt: TextureFormat) -> bool {
    matches!(
        fmt,
        TextureFormat::DXT1
            | TextureFormat::DXT3
            | TextureFormat::DXT5
            | TextureFormat::ATI1
            | TextureFormat::ATI2
            | TextureFormat::BC7
            | TextureFormat::A8R8G8B8
            | TextureFormat::X8R8G8B8
            | TextureFormat::A8B8G8R8
            | TextureFormat::A1R5G5B5
            | TextureFormat::A8
            | TextureFormat::L8
    )
}

fn round_to_multiple_of_4(v: u32) -> u32 {
    (v.max(4) + 3) & !3
}

fn compute_target_dims(settings: &Settings, width: u16, height: u16) -> (u16, u16) {
    let (w, h) = (width as u32, height as u32);
    let floor = settings.min_size_floor.max(4);

    let (target_w, target_h) = match settings.resize_mode {
        ResizeMode::CapResolution => {
            let cap = settings.cap_resolution;
            let longest = w.max(h);
            if longest <= cap {
                (w, h)
            } else {
                let scale = cap as f64 / longest as f64;
                (
                    ((w as f64) * scale).round() as u32,
                    ((h as f64) * scale).round() as u32,
                )
            }
        }
        ResizeMode::ScalePercent => {
            let scale = settings.scale_percent as f64 / 100.0;
            (
                ((w as f64) * scale).round() as u32,
                ((h as f64) * scale).round() as u32,
            )
        }
    };

    let target_w = round_to_multiple_of_4(target_w.max(floor)).min(round_to_multiple_of_4(w));
    let target_h = round_to_multiple_of_4(target_h.max(floor)).min(round_to_multiple_of_4(h));
    (target_w as u16, target_h as u16)
}

fn bgra_u32_to_rgba_bytes(pixels: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(pixels.len() * 4);
    for &p in pixels {
        let [b, g, r, a] = p.to_le_bytes();
        out.extend_from_slice(&[r, g, b, a]);
    }
    out
}

fn is_raw_format(fmt: TextureFormat) -> bool {
    matches!(
        fmt,
        TextureFormat::A8R8G8B8
            | TextureFormat::X8R8G8B8
            | TextureFormat::A8B8G8R8
            | TextureFormat::A1R5G5B5
            | TextureFormat::A8
            | TextureFormat::L8
    )
}

fn decode_raw(fmt: TextureFormat, width: u16, height: u16, data: &[u8]) -> Result<RgbaImage> {
    let (w, h) = (width as usize, height as usize);
    let bpp = ytd_io::format_unit_bytes(fmt);
    if data.len() < w * h * bpp {
        return Err(anyhow!("pixel data too short for {fmt} at {w}x{h}"));
    }
    let mut rgba = vec![0u8; w * h * 4];
    match fmt {
        TextureFormat::A8R8G8B8 => {
            for (px, out) in data.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                out.copy_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        TextureFormat::X8R8G8B8 => {
            for (px, out) in data.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
                out.copy_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        TextureFormat::A8B8G8R8 => {
            rgba.copy_from_slice(&data[..w * h * 4]);
        }
        TextureFormat::A1R5G5B5 => {
            for (px, out) in data.chunks_exact(2).zip(rgba.chunks_exact_mut(4)) {
                let v = u16::from_le_bytes([px[0], px[1]]);
                let r5 = ((v >> 10) & 0x1F) as u8;
                let g5 = ((v >> 5) & 0x1F) as u8;
                let b5 = (v & 0x1F) as u8;
                let a = if (v >> 15) & 1 == 1 { 255 } else { 0 };
                out.copy_from_slice(&[
                    (r5 << 3) | (r5 >> 2),
                    (g5 << 3) | (g5 >> 2),
                    (b5 << 3) | (b5 >> 2),
                    a,
                ]);
            }
        }
        TextureFormat::A8 => {
            for (&a, out) in data.iter().zip(rgba.chunks_exact_mut(4)) {
                out.copy_from_slice(&[255, 255, 255, a]);
            }
        }
        TextureFormat::L8 => {
            for (&l, out) in data.iter().zip(rgba.chunks_exact_mut(4)) {
                out.copy_from_slice(&[l, l, l, 255]);
            }
        }
        other => return Err(anyhow!("no decoder for {other}")),
    }
    RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| anyhow!("decoded buffer size mismatch"))
}

fn encode_raw(fmt: TextureFormat, image: &RgbaImage) -> Result<Vec<u8>> {
    let bpp = ytd_io::format_unit_bytes(fmt);
    let mut out = vec![0u8; image.width() as usize * image.height() as usize * bpp];
    let rgba = image.as_raw();
    match fmt {
        TextureFormat::A8R8G8B8 => {
            for (px, o) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                o.copy_from_slice(&[px[2], px[1], px[0], px[3]]);
            }
        }
        TextureFormat::X8R8G8B8 => {
            for (px, o) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(4)) {
                o.copy_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        TextureFormat::A8B8G8R8 => {
            out.copy_from_slice(rgba);
        }
        TextureFormat::A1R5G5B5 => {
            for (px, o) in rgba.chunks_exact(4).zip(out.chunks_exact_mut(2)) {
                let r5 = px[0] >> 3;
                let g5 = px[1] >> 3;
                let b5 = px[2] >> 3;
                let a1 = if px[3] >= 128 { 1u16 } else { 0 };
                let v = (a1 << 15) | ((r5 as u16) << 10) | ((g5 as u16) << 5) | b5 as u16;
                o.copy_from_slice(&v.to_le_bytes());
            }
        }
        TextureFormat::A8 => {
            for (px, o) in rgba.chunks_exact(4).zip(out.iter_mut()) {
                *o = px[3];
            }
        }
        TextureFormat::L8 => {
            for (px, o) in rgba.chunks_exact(4).zip(out.iter_mut()) {
                *o = px[0];
            }
        }
        other => return Err(anyhow!("no encoder for {other}")),
    }
    Ok(out)
}

fn decode_level(fmt: TextureFormat, width: u16, height: u16, data: &[u8]) -> Result<RgbaImage> {
    if is_raw_format(fmt) {
        return decode_raw(fmt, width, height, data);
    }
    let (w, h) = (width as usize, height as usize);
    let mut pixels = vec![0u32; w * h];
    let decode_result = match fmt {
        TextureFormat::DXT1 => texture2ddecoder::decode_bc1(data, w, h, &mut pixels),
        TextureFormat::DXT3 => texture2ddecoder::decode_bc2(data, w, h, &mut pixels),
        TextureFormat::DXT5 => texture2ddecoder::decode_bc3(data, w, h, &mut pixels),
        TextureFormat::ATI1 => texture2ddecoder::decode_bc4(data, w, h, &mut pixels),
        TextureFormat::ATI2 => texture2ddecoder::decode_bc5(data, w, h, &mut pixels),
        TextureFormat::BC7 => texture2ddecoder::decode_bc7(data, w, h, &mut pixels),
        other => return Err(anyhow!("no decoder for {other}")),
    };
    decode_result.map_err(|e| anyhow!("decode failed: {e}"))?;
    let rgba = bgra_u32_to_rgba_bytes(&pixels);
    RgbaImage::from_raw(width as u32, height as u32, rgba)
        .ok_or_else(|| anyhow!("decoded buffer size mismatch"))
}

fn encode_level(fmt: TextureFormat, image: &RgbaImage, quality: Quality) -> Result<Vec<u8>> {
    if is_raw_format(fmt) {
        return encode_raw(fmt, image);
    }
    let (width, height) = image.dimensions();
    let stride = width * 4;
    let data = image.as_raw();
    let rgba_surface = RgbaSurface {
        data,
        width,
        height,
        stride,
    };

    let bytes = match fmt {
        TextureFormat::DXT1 => bc1::compress_blocks(&rgba_surface),
        TextureFormat::DXT3 => {
            let algorithm = match quality {
                Quality::Fast => texpresso::Algorithm::RangeFit,
                Quality::Medium => texpresso::Algorithm::ClusterFit,
                Quality::Slow => texpresso::Algorithm::IterativeClusterFit,
            };
            let params = texpresso::Params {
                algorithm,
                ..Default::default()
            };
            let mut out =
                vec![0u8; texpresso::Format::Bc2.compressed_size(width as usize, height as usize)];
            texpresso::Format::Bc2.compress(
                data,
                width as usize,
                height as usize,
                params,
                &mut out,
            );
            out
        }
        TextureFormat::DXT5 => bc3::compress_blocks(&rgba_surface),
        TextureFormat::ATI1 => {
            let surface = RSurface {
                data,
                width,
                height,
                stride,
            };
            bc4::compress_blocks(&surface)
        }
        TextureFormat::ATI2 => {
            let surface = RgSurface {
                data,
                width,
                height,
                stride,
            };
            bc5::compress_blocks(&surface)
        }
        TextureFormat::BC7 => {
            let has_alpha = image.pixels().any(|p| p.0[3] != 255);
            let settings = match (quality, has_alpha) {
                (Quality::Fast, true) => bc7::alpha_very_fast_settings(),
                (Quality::Fast, false) => bc7::opaque_very_fast_settings(),
                (Quality::Medium, true) => bc7::alpha_basic_settings(),
                (Quality::Medium, false) => bc7::opaque_basic_settings(),
                (Quality::Slow, true) => bc7::alpha_slow_settings(),
                (Quality::Slow, false) => bc7::opaque_slow_settings(),
            };
            bc7::compress_blocks(&settings, &rgba_surface)
        }
        other => return Err(anyhow!("no encoder for {other}")),
    };
    Ok(bytes)
}

fn build_mip_chain(
    base: RgbaImage,
    fmt: TextureFormat,
    quality: Quality,
    max_levels: u8,
) -> Result<(Vec<u8>, u8)> {
    let mut out = Vec::new();
    let mut level_count = 0u8;
    let (mut w, mut h) = base.dimensions();
    let mut current = base;

    loop {
        out.extend_from_slice(&encode_level(fmt, &current, quality)?);
        level_count += 1;
        if level_count >= max_levels || w <= 4 || h <= 4 {
            break;
        }
        w = (w / 2).max(4);
        h = (h / 2).max(4);
        current = image::imageops::resize(&current, w, h, FilterType::Lanczos3);
    }
    Ok((out, level_count))
}

struct TextureOutcome {
    patch: Option<TexturePatch>,
    resized: bool,
    warning: Option<String>,
}

impl TextureOutcome {
    fn unchanged() -> Self {
        Self {
            patch: None,
            resized: false,
            warning: None,
        }
    }

    fn skip(warning: String) -> Self {
        Self {
            patch: None,
            resized: false,
            warning: Some(warning),
        }
    }
}

fn matches_skip_list(texture_name: &str, skip_list: &[String]) -> bool {
    let name = texture_name.to_lowercase();
    skip_list
        .iter()
        .any(|needle| name.contains(needle.as_str()))
}

fn process_texture(tex: &RawTexture, settings: &Settings, skip_list: &[String]) -> TextureOutcome {
    if matches_skip_list(&tex.name, skip_list) {
        return TextureOutcome::unchanged();
    }

    let (target_w, target_h) = compute_target_dims(settings, tex.width, tex.height);
    let force_format = match settings.output_format {
        OutputFormatMode::Force(f) => Some(f.to_ytd_format()),
        OutputFormatMode::KeepOriginal => None,
    };
    let target_format = force_format.unwrap_or(tex.format);
    let needs_resize = target_w != tex.width || target_h != tex.height;
    let needs_format_change = target_format != tex.format;

    if !needs_resize && !needs_format_change {
        return TextureOutcome::unchanged();
    }

    if !is_resizable_format(tex.format) {
        return TextureOutcome::skip(format!(
            "'{}': format {} has no decoder/encoder available, left unchanged",
            tex.name, tex.format
        ));
    }
    if needs_format_change && !is_resizable_format(target_format) {
        return TextureOutcome::skip(format!(
            "'{}': target format {target_format} has no encoder available, left unchanged",
            tex.name
        ));
    }

    let base_level_len = tex.stride as usize * tex.height as usize;
    let Some(level0_data) = tex
        .pixel_data
        .get(..base_level_len.min(tex.pixel_data.len()))
    else {
        return TextureOutcome::skip(format!(
            "'{}': pixel data shorter than expected, skipped",
            tex.name
        ));
    };

    let decoded = match decode_level(tex.format, tex.width, tex.height, level0_data) {
        Ok(img) => img,
        Err(e) => return TextureOutcome::skip(format!("'{}': {e}, left unchanged", tex.name)),
    };

    let resized = if needs_resize {
        image::imageops::resize(
            &decoded,
            target_w as u32,
            target_h as u32,
            FilterType::Lanczos3,
        )
    } else {
        decoded
    };

    let max_levels = match settings.mip_handling {
        MipHandling::Strip => 1,
        MipHandling::Regenerate => u8::MAX,
        MipHandling::PreserveCount => tex.levels.max(1),
    };

    let (pixel_data, levels) =
        match build_mip_chain(resized, target_format, settings.quality, max_levels) {
            Ok(r) => r,
            Err(e) => {
                return TextureOutcome::skip(format!(
                    "'{}': mip encode failed ({e}), left unchanged",
                    tex.name
                ));
            }
        };

    let mut warning = None;
    if matches!(settings.mip_handling, MipHandling::PreserveCount) && levels < tex.levels {
        warning = Some(format!(
            "'{}': mip count reduced {} -> {} (texture now too small for full chain)",
            tex.name, tex.levels, levels
        ));
    }

    TextureOutcome {
        patch: Some(TexturePatch {
            width: target_w,
            height: target_h,
            format: target_format,
            levels,
            pixel_data,
        }),
        resized: true,
        warning,
    }
}

pub struct ResizeStats {
    pub textures_resized: usize,
    pub textures_total: usize,
    pub warnings: Vec<String>,
}

pub fn resize_ytd_bytes(
    original_bytes: &[u8],
    settings: &Settings,
) -> Result<(Vec<u8>, ResizeStats)> {
    let parsed: ParsedYtd = ytd_io::parse(original_bytes)?;
    let skip_list = settings.skip_list();

    let outcomes: Vec<TextureOutcome> = parsed
        .textures
        .par_iter()
        .map(|tex| process_texture(tex, settings, &skip_list))
        .collect();

    let mut warnings = Vec::new();
    let mut resized_count = 0usize;
    let patches: Vec<Option<TexturePatch>> = outcomes
        .into_iter()
        .map(|o| {
            if o.resized {
                resized_count += 1;
            }
            if let Some(w) = o.warning {
                warnings.push(w);
            }
            o.patch
        })
        .collect();

    let textures_total = parsed.textures.len();
    let rebuilt = ytd_io::rebuild(parsed, &patches)?;

    Ok((
        rebuilt,
        ResizeStats {
            textures_resized: resized_count,
            textures_total,
            warnings,
        },
    ))
}

fn process_file(path: &Path, settings: &Settings) -> Result<(FileResult, Vec<u8>)> {
    let original_bytes = fs::read(path)?;
    let old_size = original_bytes.len() as u64;

    let (rebuilt, textures_resized, textures_total, warnings) =
        match resize_ytd_bytes(&original_bytes, settings) {
            Ok((rebuilt, stats)) => (
                rebuilt,
                stats.textures_resized,
                stats.textures_total,
                stats.warnings,
            ),
            Err(e) => {
                let warning = format!("could not parse, copied unresized instead: {e}");
                (original_bytes, 0, 0, vec![warning])
            }
        };
    let new_size = rebuilt.len() as u64;

    Ok((
        FileResult {
            path: path.to_path_buf(),
            old_size,
            new_size,
            textures_resized,
            textures_total,
            warnings,
        },
        rebuilt,
    ))
}

fn claim_output_path(
    out_dir: &Path,
    relative: &Path,
    claimed: &Mutex<HashSet<PathBuf>>,
) -> PathBuf {
    let stem = relative
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = relative.extension().and_then(|e| e.to_str());
    let parent = relative.parent().unwrap_or(Path::new(""));
    let mut guard = claimed.lock().unwrap();

    let mut candidate = out_dir.join(relative);
    let mut n = 2u32;
    while guard.contains(&candidate) || candidate.exists() {
        let name = match ext {
            Some(e) => format!("{stem}_{n}.{e}"),
            None => format!("{stem}_{n}"),
        };
        candidate = out_dir.join(parent.join(name));
        n += 1;
    }
    guard.insert(candidate.clone());
    candidate
}

fn relative_to_input_root(path: &Path, input_folders: &[PathBuf]) -> PathBuf {
    input_folders
        .iter()
        .find_map(|root| path.strip_prefix(root).ok())
        .map(|rel| rel.to_path_buf())
        .unwrap_or_else(|| path.file_name().map(PathBuf::from).unwrap_or_default())
}

fn destination_for(
    path: &Path,
    settings: &Settings,
    claimed: &Mutex<HashSet<PathBuf>>,
) -> Result<PathBuf> {
    match &settings.output_mode {
        OutputMode::SeparateFolder => {
            let out_dir = settings
                .output_folder
                .as_ref()
                .ok_or_else(|| anyhow!("no output folder set"))?;
            let relative = relative_to_input_root(path, &settings.input_folders);
            Ok(claim_output_path(out_dir, &relative, claimed))
        }
        OutputMode::OverwriteInPlace { .. } => Ok(path.to_path_buf()),
    }
}

pub fn run_batch(
    files: &[PathBuf],
    settings: Settings,
    tx: crossbeam_channel::Sender<ProgressMsg>,
) {
    let start = Instant::now();

    if let OutputMode::SeparateFolder = settings.output_mode {
        if let Some(dir) = &settings.output_folder {
            let _ = fs::create_dir_all(dir);
        }
    }

    let claimed: Mutex<HashSet<PathBuf>> = Mutex::new(HashSet::new());

    files.par_iter().for_each(|path| {
        let _ = tx.send(ProgressMsg::FileStarted { path: path.clone() });

        match process_file(path, &settings) {
            Ok((mut result, rebuilt)) => {
                if !settings.dry_run {
                    let write_result = (|| -> Result<()> {
                        if let OutputMode::OverwriteInPlace { backup: true } = settings.output_mode
                        {
                            let backup_dir = path.parent().unwrap_or(Path::new(".")).join(".bak");
                            fs::create_dir_all(&backup_dir)?;
                            let backup_path = backup_dir.join(path.file_name().unwrap());
                            if !backup_path.exists() {
                                fs::copy(path, &backup_path)?;
                            }
                        }
                        let dest = destination_for(path, &settings, &claimed)?;
                        if let Some(parent) = dest.parent() {
                            fs::create_dir_all(parent)?;
                        }
                        fs::write(dest, &rebuilt)?;
                        Ok(())
                    })();
                    if let Err(e) = write_result {
                        let _ = tx.send(ProgressMsg::FileErrored {
                            path: path.clone(),
                            error: e.to_string(),
                        });
                        return;
                    }
                } else {
                    result.new_size = rebuilt.len() as u64;
                }
                let _ = tx.send(ProgressMsg::FileFinished { result });
            }
            Err(e) => {
                let _ = tx.send(ProgressMsg::FileErrored {
                    path: path.clone(),
                    error: e.to_string(),
                });
            }
        }
    });

    let _ = tx.send(ProgressMsg::BatchFinished {
        elapsed_secs: start.elapsed().as_secs_f64(),
    });
}
