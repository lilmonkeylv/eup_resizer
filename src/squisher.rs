use std::collections::{HashMap, HashSet};
use std::fs;
use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use rayon::prelude::*;

use crate::pipeline;
use crate::settings::Settings;

const DEFAULT_MANIFEST: &str = "fx_version 'cerulean'\ngame 'gta5'\n";

#[derive(Debug, Clone)]
struct AssetKey {
    relative_dir: PathBuf,
    prefix: String,
    id: u32,
    id_byte_range: Range<usize>,
    digit_width: usize,
}

fn is_all_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn parse_asset(relative_path: &Path) -> Option<AssetKey> {
    let file_name = relative_path.file_name()?.to_str()?;
    let (stem, ext) = file_name.rsplit_once('.')?;
    let ext = ext.to_ascii_lowercase();
    if ext != "ydd" && ext != "ytd" {
        return None;
    }

    let component_start = stem.rfind('^').map(|i| i + 1).unwrap_or(0);
    let component = &stem[component_start..];
    let lower = component.to_ascii_lowercase();
    let parts: Vec<&str> = lower.split('_').collect();

    let id_part_index = if ext == "ytd" {
        parts.iter().position(|p| *p == "diff")? + 1
    } else {
        parts.iter().rposition(|p| is_all_digits(p))?
    };
    let prefix_end_index = if ext == "ytd" {
        id_part_index - 1
    } else {
        id_part_index
    };

    let id_str = *parts.get(id_part_index)?;
    if !is_all_digits(id_str) {
        return None;
    }
    let id: u32 = id_str.parse().ok()?;
    if prefix_end_index == 0 {
        return None;
    }
    let prefix = parts[..prefix_end_index].join("_");

    let mut cursor = component_start;
    for (i, part) in component.split('_').enumerate() {
        if i == id_part_index {
            let range = cursor..cursor + part.len();
            let relative_dir = relative_path
                .parent()
                .unwrap_or(Path::new(""))
                .to_path_buf();
            return Some(AssetKey {
                relative_dir,
                prefix,
                id,
                id_byte_range: range,
                digit_width: part.len(),
            });
        }
        cursor += part.len() + 1;
    }
    None
}

fn rename_with_id(original_path: &Path, key: &AssetKey, new_id: u32, new_width: usize) -> PathBuf {
    let file_name = original_path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let mut new_name = String::with_capacity(file_name.len() + 4);
    new_name.push_str(&file_name[..key.id_byte_range.start]);
    new_name.push_str(&format!("{new_id:0new_width$}"));
    new_name.push_str(&file_name[key.id_byte_range.end..]);
    original_path.with_file_name(new_name)
}

fn all_files_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };

    let mut files = Vec::new();
    let mut subdirs = Vec::new();
    for entry in entries.flatten() {
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let path = entry.path();
        if is_dir {
            subdirs.push(path);
        } else {
            files.push(path);
        }
    }

    files.extend(
        subdirs
            .par_iter()
            .flat_map(|d| all_files_in(d))
            .collect::<Vec<_>>(),
    );
    files
}

fn walk_relative(root: &Path) -> Vec<PathBuf> {
    all_files_in(root)
        .into_iter()
        .filter_map(|path| path.strip_prefix(root).ok().map(|p| p.to_path_buf()))
        .collect()
}

fn resolve_collision(
    output: &Path,
    dest_rel: &Path,
    src: &Path,
    label: &str,
) -> Result<Option<PathBuf>> {
    let dest = output.join(dest_rel);
    if !dest.exists() {
        return Ok(Some(dest_rel.to_path_buf()));
    }
    if fs::read(&dest)? == fs::read(src)? {
        return Ok(None);
    }
    let stem = dest_rel
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("file");
    let ext = dest_rel.extension().and_then(|e| e.to_str());
    let parent = dest_rel.parent().unwrap_or(Path::new(""));
    let mut n = 0u32;
    loop {
        let suffix = if n == 0 {
            format!("_{label}")
        } else {
            format!("_{label}_{n}")
        };
        let candidate_name = match ext {
            Some(e) => format!("{stem}{suffix}.{e}"),
            None => format!("{stem}{suffix}"),
        };
        let candidate = parent.join(candidate_name);
        if !output.join(&candidate).exists() {
            return Ok(Some(candidate));
        }
        n += 1;
    }
}

fn find_free_id(used: &mut HashSet<u32>, digit_width: usize) -> (u32, usize) {
    let mut width = digit_width.max(1);
    loop {
        let max = 10u32.saturating_pow(width as u32).saturating_sub(1);
        let start = used.iter().max().map_or(0, |m| m + 1);
        let candidate = (start..=max)
            .find(|c| !used.contains(c))
            .or_else(|| (0..start).find(|c| !used.contains(c)));
        if let Some(id) = candidate {
            used.insert(id);
            return (id, width);
        }
        width += 1;
    }
}

#[derive(Debug, Default, Clone)]
pub struct MergeReport {
    pub assets_copied: usize,
    pub assets_renumbered: usize,
    pub other_files_copied: usize,
    pub warnings: Vec<String>,
    pub resize_stats: Option<(usize, usize)>,
}

fn is_manifest(rel: &Path) -> bool {
    let name = rel
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    name == "fxmanifest.lua" || name == "__resource.lua"
}

pub fn merge_packs(
    packs: &[PathBuf],
    output: &Path,
    resize: Option<&Settings>,
    mut log: impl FnMut(String),
    mut on_progress: impl FnMut(usize, usize),
) -> Result<MergeReport> {
    if packs.len() < 2 {
        bail!("need at least two packs to squish");
    }
    fs::create_dir_all(output)?;
    let mut report = MergeReport::default();
    let mut used_ids: HashMap<(PathBuf, String), HashSet<u32>> = HashMap::new();
    let mut resized_total = (0usize, 0usize);
    let mut any_resized = false;

    let pack_files: Vec<Vec<PathBuf>> = packs.par_iter().map(|p| walk_relative(p)).collect();
    let total_files: usize = pack_files
        .iter()
        .map(|files| files.iter().filter(|r| !is_manifest(r)).count())
        .sum();
    let mut files_done = 0usize;
    on_progress(0, total_files);

    let write_asset = |src: &Path,
                       dest_rel: &Path,
                       resize: Option<&Settings>|
     -> Result<(Option<(usize, usize)>, Option<String>)> {
        let dest = output.join(dest_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        let is_ytd = dest
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ytd"))
            == Some(true);
        if is_ytd {
            if let Some(settings) = resize {
                match fs::read(src)
                    .map_err(anyhow::Error::from)
                    .and_then(|bytes| pipeline::resize_ytd_bytes(&bytes, settings))
                {
                    Ok((rebuilt, stats)) => {
                        fs::write(&dest, rebuilt)?;
                        return Ok((Some((stats.textures_resized, stats.textures_total)), None));
                    }
                    Err(e) => {
                        fs::copy(src, &dest)?;
                        let warning = format!(
                            "'{}': resize failed ({e}), copied unresized instead",
                            dest_rel.display()
                        );
                        return Ok((None, Some(warning)));
                    }
                }
            }
        }
        fs::copy(src, &dest)?;
        Ok((None, None))
    };

    for (idx, pack) in packs.iter().enumerate() {
        let pack_num = idx + 1;
        let label = format!("pack{pack_num}");

        let mut asset_groups: HashMap<(PathBuf, String, u32), Vec<(PathBuf, AssetKey)>> =
            HashMap::new();
        let mut other_files = Vec::new();
        for rel in &pack_files[idx] {
            if is_manifest(rel) {
                continue;
            }
            match parse_asset(rel) {
                Some(key) => {
                    let group_key = (key.relative_dir.clone(), key.prefix.clone(), key.id);
                    asset_groups
                        .entry(group_key)
                        .or_default()
                        .push((rel.clone(), key));
                }
                None => other_files.push(rel.clone()),
            }
        }

        let mut planned: Vec<(PathBuf, PathBuf, PathBuf)> = Vec::new();

        let mut sorted_groups: Vec<_> = asset_groups.into_iter().collect();
        sorted_groups.sort_by(|a, b| a.0.cmp(&b.0));

        for ((relative_dir, prefix, id), files) in sorted_groups {
            let namespace = used_ids
                .entry((relative_dir.clone(), prefix.clone()))
                .or_default();
            let renumber = namespace.contains(&id);
            let new_id_width = if renumber {
                let widest = files.iter().map(|(_, k)| k.digit_width).max().unwrap_or(3);
                Some(find_free_id(namespace, widest))
            } else {
                namespace.insert(id);
                None
            };

            for (rel, key) in &files {
                let src = pack.join(rel);
                let dest_rel = match new_id_width {
                    Some((new_id, new_width)) => {
                        let renamed = rename_with_id(rel, key, new_id, new_width);
                        log(format!(
                            "pack {pack_num}: renumbered {} -> {} ({prefix} id {id} already used)",
                            rel.display(),
                            renamed.display()
                        ));
                        report.assets_renumbered += 1;
                        renamed
                    }
                    None => {
                        report.assets_copied += 1;
                        rel.clone()
                    }
                };

                match resolve_collision(output, &dest_rel, &src, &label) {
                    Ok(Some(final_rel)) => {
                        if final_rel != dest_rel {
                            report.warnings.push(format!(
                                "pack {pack_num} '{}': still collided after renumbering, kept both as '{}'",
                                rel.display(),
                                final_rel.display()
                            ));
                        }
                        planned.push((src, final_rel, rel.clone()));
                    }
                    Ok(None) => {}
                    Err(e) => report.warnings.push(format!(
                        "pack {pack_num} '{}': collision check failed: {e}",
                        rel.display()
                    )),
                }
            }
        }

        for rel in &other_files {
            let src = pack.join(rel);
            match resolve_collision(output, rel, &src, &label) {
                Ok(Some(dest_rel)) => {
                    if &dest_rel != rel {
                        let is_metadata = rel.extension().and_then(|e| e.to_str()).map(|e| {
                            e.eq_ignore_ascii_case("ymt") || e.eq_ignore_ascii_case("ynd")
                        }) == Some(true);
                        let note = if is_metadata {
                            " - ped component metadata, not auto-mergeable; review both and reconcile by hand"
                        } else {
                            ""
                        };
                        report.warnings.push(format!(
                            "pack {pack_num} '{}': name collided with an earlier pack, kept both as '{}'{note}",
                            rel.display(),
                            dest_rel.display()
                        ));
                    }
                    report.other_files_copied += 1;
                    planned.push((src, dest_rel, rel.clone()));
                }
                Ok(None) => {}
                Err(e) => report.warnings.push(format!(
                    "pack {pack_num} '{}': collision check failed: {e}",
                    rel.display()
                )),
            }
        }

        const CHUNK_SIZE: usize = 1000;
        for chunk in planned.chunks(CHUNK_SIZE) {
            let results: Vec<(PathBuf, Result<(Option<(usize, usize)>, Option<String>)>)> = chunk
                .par_iter()
                .map(|(src, dest_rel, orig_rel)| {
                    (orig_rel.clone(), write_asset(src, dest_rel, resize))
                })
                .collect();

            for (orig_rel, result) in results {
                match result {
                    Ok((Some(stats), _)) => {
                        any_resized = true;
                        resized_total.0 += stats.0;
                        resized_total.1 += stats.1;
                    }
                    Ok((None, Some(w))) => report.warnings.push(format!("pack {pack_num} {w}")),
                    Ok((None, None)) => {}
                    Err(e) => report
                        .warnings
                        .push(format!("pack {pack_num} '{}': {e}", orig_rel.display())),
                }
                files_done += 1;
                on_progress(files_done, total_files);
            }
        }
    }

    merge_manifests(packs, output, &mut report)?;

    if any_resized {
        report.resize_stats = Some(resized_total);
    }
    Ok(report)
}

fn read_manifest(pack: &Path) -> Option<String> {
    for name in ["fxmanifest.lua", "__resource.lua"] {
        if let Ok(text) = fs::read_to_string(pack.join(name)) {
            return Some(text);
        }
    }
    None
}

fn extract_quoted(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\'' {
            if let Some(end) = line[i + 1..].find('\'') {
                out.push(line[i + 1..i + 1 + end].to_string());
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    out
}

#[derive(Default)]
struct ParsedManifest {
    fx_version: Option<String>,
    game: Option<String>,
    files_entries: Vec<String>,
    other_lines: Vec<String>,
}

fn parse_manifest(text: &str) -> ParsedManifest {
    let mut parsed = ParsedManifest::default();
    let mut in_files_block = false;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        if in_files_block {
            parsed.files_entries.extend(extract_quoted(line));
            if line.contains('}') {
                in_files_block = false;
            }
            continue;
        }

        if line.starts_with("files") {
            parsed.files_entries.extend(extract_quoted(line));
            if !line.contains('}') {
                in_files_block = true;
            }
            continue;
        }

        if line.starts_with("fx_version") {
            parsed.fx_version.get_or_insert_with(|| line.to_string());
            continue;
        }
        if line.starts_with("game") {
            parsed.game.get_or_insert_with(|| line.to_string());
            continue;
        }

        parsed.other_lines.push(line.to_string());
    }

    parsed
}

fn merge_manifests(packs: &[PathBuf], output: &Path, report: &mut MergeReport) -> Result<()> {
    let mut fx_version = None;
    let mut game = None;
    let mut files_entries: Vec<String> = Vec::new();
    let mut seen_files: HashSet<String> = HashSet::new();
    let mut other_lines: Vec<String> = Vec::new();
    let mut seen_other: HashSet<String> = HashSet::new();
    let mut any_manifest = false;
    let mut any_extra = false;

    for (idx, pack) in packs.iter().enumerate() {
        let Some(text) = read_manifest(pack) else {
            continue;
        };
        any_manifest = true;
        let parsed = parse_manifest(&text);

        if fx_version.is_none() {
            fx_version = parsed.fx_version;
        }
        if game.is_none() {
            game = parsed.game;
        }
        for entry in parsed.files_entries {
            if seen_files.insert(entry.clone()) {
                files_entries.push(entry);
                if idx > 0 {
                    any_extra = true;
                }
            }
        }
        for line in parsed.other_lines {
            if seen_other.insert(line.clone()) {
                other_lines.push(line);
                if idx > 0 {
                    any_extra = true;
                }
            }
        }
    }

    let merged = if !any_manifest {
        DEFAULT_MANIFEST.to_string()
    } else {
        let mut out = String::new();
        out.push_str(fx_version.as_deref().unwrap_or("fx_version 'cerulean'"));
        out.push('\n');
        out.push_str(game.as_deref().unwrap_or("game 'gta5'"));
        out.push('\n');
        if !files_entries.is_empty() {
            out.push_str("\nfiles {\n");
            for entry in &files_entries {
                out.push_str(&format!("    '{entry}',\n"));
            }
            out.push_str("}\n");
        }
        if !other_lines.is_empty() {
            out.push('\n');
            for line in &other_lines {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    };

    if any_extra {
        report.warnings.push(
            "fxmanifest.lua: packs' manifests differ beyond boilerplate; merged (files{} \
             blocks combined into one, other directives deduped by exact line) - review for \
             conflicting exports/dependency/data_file directives before shipping."
                .to_string(),
        );
    }

    fs::write(output.join("fxmanifest.lua"), merged)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, b"").unwrap();
    }

    #[test]
    fn merge_renumbers_colliding_ids_and_leaves_the_rest_untouched() {
        let root = std::env::temp_dir().join(format!("eup_squisher_test_{}", std::process::id()));
        let (pack_a, pack_b, out) = (root.join("a"), root.join("b"), root.join("out"));
        let _ = fs::remove_dir_all(&root);

        touch(&pack_a.join("stream/male/jbib_014_u.ydd"));
        touch(&pack_a.join("stream/male/jbib_diff_014_a_uni.ytd"));
        touch(&pack_a.join("stream/male/jbib_015_u.ydd"));
        touch(&pack_a.join("stream/male/jbib_diff_015_a_uni.ytd"));
        fs::write(
            pack_a.join("fxmanifest.lua"),
            "fx_version 'cerulean'\ngame 'gta5'\n",
        )
        .unwrap();

        touch(&pack_b.join("stream/male/jbib_014_u.ydd"));
        touch(&pack_b.join("stream/male/jbib_diff_014_a_uni.ytd"));
        touch(&pack_b.join("stream/male/jbib_diff_014_b_uni.ytd"));
        touch(&pack_b.join("stream/male/jbib_020_u.ydd"));
        touch(&pack_b.join("stream/male/jbib_diff_020_a_uni.ytd"));
        fs::write(
            pack_b.join("fxmanifest.lua"),
            "fx_version 'cerulean'\ngame 'gta5'\n",
        )
        .unwrap();

        let mut lines = Vec::new();
        let report =
            merge_packs(&[pack_a, pack_b], &out, None, |l| lines.push(l), |_, _| {}).unwrap();

        assert_eq!(
            report.assets_renumbered, 3,
            "ydd + 2 ytd variants sharing id 014 should all renumber together"
        );
        assert!(
            out.join("stream/male/jbib_014_u.ydd").exists(),
            "pack 1's id 014 must survive untouched"
        );
        assert!(out.join("stream/male/jbib_015_u.ydd").exists());
        assert!(
            out.join("stream/male/jbib_020_u.ydd").exists(),
            "pack 2's non-colliding id must survive untouched"
        );

        assert!(
            out.join("stream/male/jbib_016_u.ydd").exists(),
            "expected renumber to next free id 016"
        );
        assert!(out.join("stream/male/jbib_diff_016_a_uni.ytd").exists());
        assert!(out.join("stream/male/jbib_diff_016_b_uni.ytd").exists());

        assert!(
            fs::read_to_string(out.join("fxmanifest.lua"))
                .unwrap()
                .contains("fx_version")
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn colliding_ymt_keeps_both_copies_instead_of_overwriting() {
        let root =
            std::env::temp_dir().join(format!("eup_squisher_ymt_test_{}", std::process::id()));
        let (pack_a, pack_b, out) = (root.join("a"), root.join("b"), root.join("out"));
        let _ = fs::remove_dir_all(&root);

        fs::create_dir_all(pack_a.join("stream")).unwrap();
        fs::create_dir_all(pack_b.join("stream")).unwrap();
        fs::write(
            pack_a.join("stream/mp_freemode_overlays.ymt"),
            b"pack-a-component-data",
        )
        .unwrap();
        fs::write(
            pack_b.join("stream/mp_freemode_overlays.ymt"),
            b"pack-b-component-data-DIFFERENT",
        )
        .unwrap();

        let mut lines = Vec::new();
        let report =
            merge_packs(&[pack_a, pack_b], &out, None, |l| lines.push(l), |_, _| {}).unwrap();

        let a_bytes = fs::read(out.join("stream/mp_freemode_overlays.ymt")).unwrap();
        assert_eq!(
            a_bytes, b"pack-a-component-data",
            "pack 1's ymt must survive untouched at its original name"
        );

        let kept_both = fs::read_dir(out.join("stream"))
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name().to_string_lossy().contains("pack2")
                    && e.file_name().to_string_lossy().ends_with(".ymt")
            });
        assert!(
            kept_both,
            "pack 2's differing ymt must be kept under a distinct name, not dropped"
        );

        assert!(
            report
                .warnings
                .iter()
                .any(|w| w.contains("ymt") || w.contains("metadata")),
            "a collision on ped component metadata must be flagged for manual review"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn four_packs_all_fold_into_one_namespace() {
        let root =
            std::env::temp_dir().join(format!("eup_squisher_4pack_test_{}", std::process::id()));
        let dirs: Vec<PathBuf> = (1..=4).map(|n| root.join(format!("p{n}"))).collect();
        let out = root.join("out");
        let _ = fs::remove_dir_all(&root);

        for dir in &dirs {
            touch(&dir.join("stream/male/jbib_014_u.ydd"));
            touch(&dir.join("stream/male/jbib_diff_014_a_uni.ytd"));
        }

        let mut lines = Vec::new();
        let report = merge_packs(&dirs, &out, None, |l| lines.push(l), |_, _| {}).unwrap();

        assert_eq!(
            report.assets_renumbered, 6,
            "packs 2, 3, and 4 each renumber their 014 group (2 files each)"
        );

        let ydd_ids: HashSet<String> = fs::read_dir(out.join("stream/male"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".ydd"))
            .collect();
        assert_eq!(
            ydd_ids.len(),
            4,
            "four distinct drawables must exist, one per pack, none overwritten"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_merge_combines_files_blocks_into_one_valid_table() {
        let root =
            std::env::temp_dir().join(format!("eup_squisher_manifest_test_{}", std::process::id()));
        let pack_a = root.join("a");
        let pack_b = root.join("b");
        let out = root.join("out");
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&pack_a).unwrap();
        fs::create_dir_all(&pack_b).unwrap();
        fs::create_dir_all(&out).unwrap();

        fs::write(
            pack_a.join("fxmanifest.lua"),
            "fx_version 'cerulean'\ngame { 'gta5' }\nfiles {\n\t'female_shop.meta',\n\t'pedalternatevariations.meta'\n}\ndata_file 'SHOP_PED_APPAREL_META_FILE' 'female_shop.meta'\ndata_file 'ALTERNATE_VARIATIONS_FILE' 'pedalternatevariations.meta'\n",
        )
        .unwrap();
        fs::write(
            pack_b.join("fxmanifest.lua"),
            "fx_version 'cerulean'\ngame { 'gta5' }\nfiles {\n\t'male_shop.meta'\n}\ndata_file 'SHOP_PED_APPAREL_META_FILE' 'male_shop.meta'\n",
        )
        .unwrap();

        let mut report = MergeReport::default();
        merge_manifests(&[pack_a, pack_b], &out, &mut report).unwrap();
        let merged = fs::read_to_string(out.join("fxmanifest.lua")).unwrap();

        let files_start = merged
            .find("files {")
            .expect("must have exactly one files block");
        let files_end = merged[files_start..]
            .find('}')
            .expect("files block must close")
            + files_start;
        assert_eq!(
            merged[files_start + 1..].find("files {"),
            None,
            "must not have a second files block"
        );

        let files_block = &merged[files_start..=files_end];
        for expected in [
            "female_shop.meta",
            "pedalternatevariations.meta",
            "male_shop.meta",
        ] {
            assert!(
                files_block.contains(expected),
                "'{expected}' must be inside the single files{{}} block, got: {merged}"
            );
        }

        for (line, offset) in merged.lines().scan(0usize, |pos, l| {
            let start = *pos;
            *pos += l.len() + 1;
            Some((l, start))
        }) {
            let trimmed = line.trim();
            let inside_files_block = offset >= files_start && offset <= files_end;
            if trimmed.starts_with('\'') && !inside_files_block {
                panic!(
                    "bare quoted string outside any table (invalid Lua): {trimmed:?}\nfull manifest:\n{merged}"
                );
            }
        }

        assert!(merged.contains("data_file 'SHOP_PED_APPAREL_META_FILE' 'female_shop.meta'"));
        assert!(merged.contains("data_file 'SHOP_PED_APPAREL_META_FILE' 'male_shop.meta'"));
        assert!(
            merged.contains("data_file 'ALTERNATE_VARIATIONS_FILE' 'pedalternatevariations.meta'")
        );

        let _ = fs::remove_dir_all(&root);
    }
}
