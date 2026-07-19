// RSC7/YTD struct offsets, virtual-address tagging scheme, and pixel-size
// formula referenced from https://github.com/VIRUXE/rpf-rs (src/ytd.rs, Unlicense).

use std::io::{Read, Write};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::read::DeflateDecoder;
use flate2::write::DeflateEncoder;
use rpf_archive::archive::{RSC7_MAGIC, resource_size_from_flags, resource_version_from_flags};

pub use rpf_archive::ytd::TextureFormat;

const RSC_HEADER_LEN: usize = 16;
const TEXTURE_STRUCT_LEN: usize = 0x90;
const SYSTEM_TAG: u64 = 0x5000_0000;
const GRAPHICS_TAG: u64 = 0x6000_0000;

#[derive(Debug, Clone)]
pub struct RawTexture {
    pub name: String,
    #[allow(dead_code)]
    pub name_hash: u32,
    pub width: u16,
    pub height: u16,
    #[allow(dead_code)]
    pub depth: u16,
    pub format: TextureFormat,
    pub levels: u8,
    pub stride: u16,
    pub pixel_data: Vec<u8>,
    struct_offset: usize,
}

pub struct ParsedYtd {
    pub textures: Vec<RawTexture>,
    system: Vec<u8>,
    sys_flags: u32,
    gfx_version_nibble: u32,
}

pub struct TexturePatch {
    pub width: u16,
    pub height: u16,
    pub format: TextureFormat,
    pub levels: u8,
    pub pixel_data: Vec<u8>,
}

fn resolve<'a>(system: &'a [u8], graphics: &'a [u8], va: u64, len: usize) -> Option<&'a [u8]> {
    if va == 0 {
        return None;
    }
    if (va & SYSTEM_TAG) == SYSTEM_TAG && (va & GRAPHICS_TAG) != GRAPHICS_TAG {
        let off = (va - SYSTEM_TAG) as usize;
        system.get(off..off + len)
    } else if (va & GRAPHICS_TAG) == GRAPHICS_TAG {
        let off = (va - GRAPHICS_TAG) as usize;
        graphics.get(off..off + len)
    } else {
        None
    }
}

fn string_at(system: &[u8], va: u64) -> Option<String> {
    if (va & SYSTEM_TAG) != SYSTEM_TAG || (va & GRAPHICS_TAG) == GRAPHICS_TAG {
        return None;
    }
    let off = (va - SYSTEM_TAG) as usize;
    let slice = system.get(off..)?;
    let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
    Some(String::from_utf8_lossy(&slice[..end]).into_owned())
}

fn u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn u64_le(b: &[u8], off: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[off..off + 8]);
    u64::from_le_bytes(a)
}

fn calc_pixel_data_size(stride: u16, height: u16, levels: u8) -> usize {
    let mut total = 0usize;
    let mut length = stride as usize * height as usize;
    for _ in 0..levels {
        total += length;
        length /= 4;
    }
    total
}

pub(crate) fn format_unit_bytes(format: TextureFormat) -> usize {
    match format {
        TextureFormat::DXT1 | TextureFormat::ATI1 => 8,
        TextureFormat::DXT3 | TextureFormat::DXT5 | TextureFormat::ATI2 | TextureFormat::BC7 => 16,
        TextureFormat::A8R8G8B8 | TextureFormat::X8R8G8B8 | TextureFormat::A8B8G8R8 => 4,
        TextureFormat::A1R5G5B5 => 2,
        TextureFormat::A8 | TextureFormat::L8 => 1,
        TextureFormat::Unknown => 4,
    }
}

fn compute_stride(width: u16, format: TextureFormat) -> u16 {
    if format.is_block_compressed() {
        let blocks_wide = width.div_ceil(4).max(1);
        (blocks_wide as usize * format_unit_bytes(format)) as u16
    } else {
        (width as usize * format_unit_bytes(format)) as u16
    }
}

pub fn count_textures(data: &[u8]) -> Result<usize> {
    if data.len() < RSC_HEADER_LEN {
        bail!("YTD data too short ({} bytes)", data.len());
    }
    let magic = u32_le(data, 0);
    if magic != RSC7_MAGIC {
        bail!("not an RSC7 file (magic = 0x{magic:08X})");
    }
    let body = &data[RSC_HEADER_LEN..];

    let mut header = [0u8; 0x40];
    let mut decoder = DeflateDecoder::new(body);
    if decoder.read_exact(&mut header).is_err() {
        if body.len() < 0x40 {
            bail!("system section too small for a TextureDictionary");
        }
        header.copy_from_slice(&body[..0x40]);
    }

    Ok(u16_le(&header, 0x38) as usize)
}

pub fn count_textures_from_path(path: &std::path::Path) -> Result<usize> {
    let file = std::fs::File::open(path)?;
    let mut reader = std::io::BufReader::new(file);

    let mut rsc_header = [0u8; RSC_HEADER_LEN];
    reader.read_exact(&mut rsc_header)?;
    let magic = u32_le(&rsc_header, 0);
    if magic != RSC7_MAGIC {
        bail!("not an RSC7 file (magic = 0x{magic:08X})");
    }

    let mut header = [0u8; 0x40];
    let mut decoder = DeflateDecoder::new(reader);
    decoder
        .read_exact(&mut header)
        .with_context(|| "reading system section header")?;

    Ok(u16_le(&header, 0x38) as usize)
}

pub fn parse(data: &[u8]) -> Result<ParsedYtd> {
    if data.len() < RSC_HEADER_LEN {
        bail!("YTD data too short ({} bytes)", data.len());
    }
    let magic = u32_le(data, 0);
    if magic != RSC7_MAGIC {
        bail!("not an RSC7 file (magic = 0x{magic:08X})");
    }

    let system_flags = u32_le(data, 8);
    let graphics_flags = u32_le(data, 12);
    let sys_size = resource_size_from_flags(system_flags);
    let gfx_size = resource_size_from_flags(graphics_flags);
    let body = &data[RSC_HEADER_LEN..];

    let decompressed = {
        let mut out = Vec::new();
        if DeflateDecoder::new(body).read_to_end(&mut out).is_ok() && !out.is_empty() {
            out
        } else {
            body.to_vec()
        }
    };
    if decompressed.len() < sys_size {
        bail!(
            "decompressed size {} smaller than declared system size {sys_size}",
            decompressed.len()
        );
    }

    let system = decompressed[..sys_size].to_vec();
    let graphics_end = (sys_size + gfx_size).min(decompressed.len());
    let graphics = decompressed[sys_size..graphics_end].to_vec();

    if system.len() < 0x40 {
        bail!(
            "system section too small for a TextureDictionary ({} bytes)",
            system.len()
        );
    }

    let hash_ptr = u64_le(&system, 0x20);
    let hash_count = u16_le(&system, 0x28) as usize;
    let tex_ptr_array = u64_le(&system, 0x30);
    let tex_count = u16_le(&system, 0x38) as usize;

    let hash_data = if hash_count > 0 {
        resolve(&system, &graphics, hash_ptr, hash_count * 4)
    } else {
        None
    };

    let mut textures = Vec::with_capacity(tex_count);
    if tex_count > 0 {
        let ptr_bytes = tex_count * 8;
        let ptr_data =
            resolve(&system, &graphics, tex_ptr_array, ptr_bytes).with_context(|| {
                format!("texture pointer array out of bounds (va=0x{tex_ptr_array:X})")
            })?;

        for i in 0..tex_count {
            let tex_va = u64_le(ptr_data, i * 8);
            if tex_va == 0 {
                continue;
            }
            let name_hash = hash_data
                .and_then(|h| h.get(i * 4..i * 4 + 4))
                .map(|b| u32_le(b, 0))
                .unwrap_or(0);

            if (tex_va & SYSTEM_TAG) != SYSTEM_TAG || (tex_va & GRAPHICS_TAG) == GRAPHICS_TAG {
                eprintln!("[ytd_io] texture {i}: pointer not in system section, skipping");
                continue;
            }
            let struct_offset = (tex_va - SYSTEM_TAG) as usize;
            if struct_offset + TEXTURE_STRUCT_LEN > system.len() {
                eprintln!("[ytd_io] texture {i}: struct out of bounds, skipping");
                continue;
            }
            let raw = &system[struct_offset..struct_offset + TEXTURE_STRUCT_LEN];

            let name_ptr = u64_le(raw, 0x28);
            let width = u16_le(raw, 0x50);
            let height = u16_le(raw, 0x52);
            let depth = u16_le(raw, 0x54);
            let stride = u16_le(raw, 0x56);
            let format = TextureFormat::from_u32(u32_le(raw, 0x58));
            let levels = raw[0x5D];
            let data_ptr = u64_le(raw, 0x70);
            let name = string_at(&system, name_ptr).unwrap_or_default();

            let pixel_size = calc_pixel_data_size(stride, height, levels);
            let pixel_data = if pixel_size > 0 && data_ptr != 0 {
                resolve(&system, &graphics, data_ptr, pixel_size)
                    .map(|s| s.to_vec())
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            textures.push(RawTexture {
                name,
                name_hash,
                width,
                height,
                depth,
                format,
                levels,
                stride,
                pixel_data,
                struct_offset,
            });
        }
    }

    Ok(ParsedYtd {
        textures,
        system,
        sys_flags: system_flags,
        gfx_version_nibble: graphics_flags >> 28,
    })
}

fn encode_capacity_flags(size: usize, version_nibble: u32) -> u32 {
    if size == 0 {
        return version_nibble << 28;
    }
    for base_shift in 0u32..=15 {
        let base_size: u64 = 0x200u64 << base_shift;
        let unit = base_size * 16;
        let count = (size as u64).div_ceil(unit);
        if count <= 127 {
            let flags = base_shift | ((count as u32) << 17) | (version_nibble << 28);
            debug_assert_eq!(resource_size_from_flags(flags), (count * unit) as usize);
            return flags;
        }
    }
    version_nibble << 28
}

pub fn rebuild(parsed: ParsedYtd, patches: &[Option<TexturePatch>]) -> Result<Vec<u8>> {
    if patches.len() != parsed.textures.len() {
        bail!(
            "patch count {} != texture count {}",
            patches.len(),
            parsed.textures.len()
        );
    }

    let mut system = parsed.system;
    let mut graphics: Vec<u8> = Vec::with_capacity(system.len());

    for (tex, patch) in parsed.textures.iter().zip(patches.iter()) {
        let data_offset = graphics.len();
        match patch {
            Some(p) => {
                let stride = compute_stride(p.width, p.format);
                graphics.extend_from_slice(&p.pixel_data);
                let s = &mut system[tex.struct_offset..tex.struct_offset + TEXTURE_STRUCT_LEN];
                s[0x50..0x52].copy_from_slice(&p.width.to_le_bytes());
                s[0x52..0x54].copy_from_slice(&p.height.to_le_bytes());
                s[0x56..0x58].copy_from_slice(&stride.to_le_bytes());
                s[0x58..0x5C].copy_from_slice(&(p.format as u32).to_le_bytes());
                s[0x5D] = p.levels;
            }
            None => {
                graphics.extend_from_slice(&tex.pixel_data);
            }
        }
        let data_va = GRAPHICS_TAG + data_offset as u64;
        let s = &mut system[tex.struct_offset..tex.struct_offset + TEXTURE_STRUCT_LEN];
        s[0x70..0x78].copy_from_slice(&data_va.to_le_bytes());
    }

    let graphics_flags = encode_capacity_flags(graphics.len(), parsed.gfx_version_nibble);
    let padded_capacity = resource_size_from_flags(graphics_flags);
    graphics.resize(padded_capacity, 0);

    let version = resource_version_from_flags(parsed.sys_flags, graphics_flags);

    system.append(&mut graphics);
    let body = system;

    let mut compressed = Vec::new();
    {
        let mut enc = DeflateEncoder::new(&mut compressed, Compression::fast());
        enc.write_all(&body)?;
        enc.finish()?;
    }

    let mut out = Vec::with_capacity(RSC_HEADER_LEN + compressed.len());
    out.extend_from_slice(&RSC7_MAGIC.to_le_bytes());
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&parsed.sys_flags.to_le_bytes());
    out.extend_from_slice(&graphics_flags.to_le_bytes());
    out.extend_from_slice(&compressed);
    Ok(out)
}
