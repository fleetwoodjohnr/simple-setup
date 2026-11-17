//! Operations on finished snapshots: summary, integrity verification, deletion, and packing
//! into a single `.tar.gz` file (and back).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use schemars::JsonSchema;
use serde::Serialize;

use crate::build::{CHECKSUMS, Progress, hash_file};
use crate::manifest::{MANIFEST_FILE, Manifest, Readiness};
use crate::paths::{check_rel, join_rel};
use crate::platform::{Device, Platform};
use crate::selection::Selection;
use crate::{Error, IoContext, Result, invalid};

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Summary {
    pub path: PathBuf,
    pub id: String,
    pub name: String,
    pub created_at: String,
    pub source: Device,
    pub targets: Vec<Platform>,
    pub apps: Vec<String>,
    pub folders: Vec<String>,
    pub files: usize,
    pub bytes: u64,
    /// Apps with an offline installer in the snapshot.
    pub installers: usize,
    pub readiness: Vec<Readiness>,
    pub warnings: Vec<String>,
    /// What was chosen when the snapshot was made, for reuse.
    pub selection: Selection,
    /// Encrypted: apps, folders and the selection need the password to be shown.
    pub encrypted: bool,
    /// Name of the snapshot this one updates.
    pub base: Option<String>,
    /// ID of the snapshot this one updates.
    pub base_id: Option<String>,
}

pub fn summary(path: &Path) -> Result<Summary> {
    summary_with(path, None)
}

/// The summary of an encrypted snapshot with its contents, opened with `key`.
pub fn summary_with(path: &Path, key: Option<&crate::crypt::Key>) -> Result<Summary> {
    let m = match key {
        Some(k) => Manifest::open(path, Some(k))?,
        None => Manifest::read(path)?,
    };
    Ok(Summary {
        path: path.to_path_buf(),
        id: m.id,
        name: m.name,
        created_at: m.created_at,
        source: m.source,
        targets: m.targets,
        apps: m.apps.iter().map(|a| a.name.clone()).collect(),
        folders: m
            .roots
            .iter()
            .filter(|r| r.app.is_none())
            .map(|r| r.original.clone())
            .collect(),
        files: m.files.len(),
        bytes: m.total_bytes,
        installers: m.installers.len(),
        readiness: m.readiness,
        warnings: m.warnings,
        selection: m.selection,
        encrypted: m.encryption.is_some(),
        base_id: m.base.as_ref().map(|b| b.id.clone()),
        base: m.base.map(|b| b.name),
    })
}

/// Which history each snapshot belongs to: the ID of the oldest known snapshot it was updated
/// from, step by step (its own ID when it updates none of these). Every snapshot of a history
/// is complete in itself (reused files were linked or copied), so older ones can be deleted.
pub fn history(snaps: &[Summary]) -> HashMap<String, String> {
    let base: HashMap<&str, &str> = snaps
        .iter()
        .filter_map(|s| Some((s.id.as_str(), s.base_id.as_deref()?)))
        .collect();
    snaps
        .iter()
        .map(|s| {
            let mut root = s.id.as_str();
            // Bounded: a damaged pair of snapshots naming each other cannot loop.
            for _ in 0..snaps.len() {
                match base.get(root) {
                    Some(b) if snaps.iter().any(|x| x.id == *b) => root = b,
                    _ => break,
                }
            }
            (s.id.clone(), root.to_string())
        })
        .collect()
}

/// Deletes the snapshots of `bundle`'s history (among `known`, the snapshots this computer
/// knows) beyond the newest `keep`; returns their paths.
pub fn prune(bundle: &Path, known: &[PathBuf], keep: usize) -> Result<Vec<PathBuf>> {
    let id = Manifest::read(bundle)?.id;
    let sums: Vec<Summary> = known.iter().filter_map(|p| summary(p).ok()).collect();
    let old: Vec<PathBuf> = prunable(&sums, &id, keep)
        .into_iter()
        .map(|s| s.path.clone())
        .collect();
    for p in &old {
        delete(p)?;
    }
    Ok(old)
}

/// The snapshots of `id`'s history beyond the newest `keep`, oldest first: what pruning to
/// `keep` deletes.
pub fn prunable<'a>(snaps: &'a [Summary], id: &str, keep: usize) -> Vec<&'a Summary> {
    let h = history(snaps);
    let Some(root) = h.get(id) else {
        return Vec::new();
    };
    let mut chain: Vec<&Summary> = snaps
        .iter()
        .filter(|s| h.get(&s.id) == Some(root))
        .collect();
    chain.sort_by(|a, b| a.created_at.cmp(&b.created_at));
    let n = chain.len().saturating_sub(keep.max(1));
    chain.truncate(n);
    chain
}

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct VerifyReport {
    pub ok: bool,
    pub checked: u64,
    pub bytes: u64,
    pub missing: Vec<String>,
    pub corrupt: Vec<String>,
}

/// `SHA256SUMS` lines → path → hash.
pub fn parse_sums(text: &str) -> Result<HashMap<String, String>> {
    let mut out = HashMap::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let (h, p) = line
            .split_once("  ")
            .ok_or_else(|| invalid("Damaged checksum list."))?;
        check_rel(p, true)?;
        out.insert(p.to_string(), h.to_string());
    }
    Ok(out)
}

/// Re-hashes every file the manifest and checksum list name and reports anything missing or
/// changed. Use before restoring and after copying a snapshot to new media.
pub fn verify(
    bundle: &Path,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<VerifyReport> {
    let m = Manifest::read(bundle)?;
    let mut r = VerifyReport::default();
    let sums_path = bundle.join(CHECKSUMS);
    let sums = match std::fs::read_to_string(&sums_path) {
        Ok(t) => parse_sums(&t)?,
        Err(_) => {
            r.missing.push(CHECKSUMS.into());
            HashMap::new()
        }
    };
    // Copied files and installer files (shared installer files are listed once).
    let mut todo: Vec<(String, String)> = Vec::new();
    let mut known: HashMap<&str, &str> = HashMap::new();
    let installer_files = m.installers.iter().flat_map(|i| i.recipe.files());
    for (p, h) in m
        .files
        .iter()
        .map(|f| (&f.stored, f.stored_hash()))
        .chain(installer_files.map(|f| (&f.stored, f.sha256.as_str())))
    {
        match known.insert(p, h) {
            None => todo.push((p.clone(), h.to_string())),
            Some(prev) if prev != h => r
                .corrupt
                .push(format!("{p} (listed twice with different checksums)")),
            Some(_) => {}
        }
    }
    for (p, h) in &sums {
        match known.get(p.as_str()) {
            Some(k) if k != h => r
                .corrupt
                .push(format!("{p} (checksum list disagrees with manifest)")),
            Some(_) => {}
            None => todo.push((p.clone(), h.clone())),
        }
    }
    if !sums.contains_key(MANIFEST_FILE) && !r.missing.contains(&CHECKSUMS.to_string()) {
        r.missing.push(format!("{MANIFEST_FILE} checksum"));
    }
    let total: u64 = todo
        .iter()
        .filter_map(|(rel, _)| {
            join_rel(bundle, rel, true)
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
        })
        .map(|md| md.len())
        .sum();
    let steps = todo.len() as u64;
    emit(Progress::Stage {
        label: "Verifying snapshot".into(),
        step: 1,
        steps: 1,
    });
    for (i, (rel, want)) in todo.iter().enumerate() {
        let p = join_rel(bundle, rel, true)?;
        match hash_file(&p, cancel, |n| r.bytes += n) {
            Ok((_, got)) if &got == want => r.checked += 1,
            Ok(_) => r.corrupt.push(rel.clone()),
            Err(Error::Cancelled) => return Err(Error::Cancelled),
            Err(_) => r.missing.push(rel.clone()),
        }
        emit(Progress::Copy {
            done_bytes: r.bytes.min(total),
            total_bytes: total,
            done_files: i as u64 + 1,
            total_files: steps,
            current: rel.clone(),
        });
    }
    r.ok = r.missing.is_empty() && r.corrupt.is_empty();
    Ok(r)
}

/// Deletes a snapshot folder after confirming it really is one.
pub fn delete(bundle: &Path) -> Result<()> {
    if !bundle.join(MANIFEST_FILE).is_file() {
        return Err(invalid("That folder is not a Simple Setup snapshot."));
    }
    std::fs::remove_dir_all(bundle).ctx(|| format!("Deleting {}", bundle.display()))
}

/// Packs a snapshot folder into one `.tar.gz` file at `out` (for storage or copying as a
/// single file). Only regular files are packed; the folder's name is the archive's top folder.
pub fn pack(
    bundle: &Path,
    out: &Path,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let m = Manifest::read(bundle)?;
    if out.exists() {
        return Err(invalid(format!("{} already exists.", out.display())));
    }
    let name = bundle
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or(m.name);
    let files: Vec<(PathBuf, String, u64)> = walkdir::WalkDir::new(bundle)
        .sort_by_file_name()
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter_map(|e| {
            let rel = e.path().strip_prefix(bundle).ok()?;
            let rel: Vec<String> = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect();
            let size = e.metadata().ok()?.len();
            Some((e.path().to_path_buf(), rel.join("/"), size))
        })
        .collect();
    let total: u64 = files.iter().map(|f| f.2).sum();
    emit(Progress::Stage {
        label: format!("Packing into {}", out.display()),
        step: 1,
        steps: 1,
    });
    let partial = out.with_extension("partial");
    let mut write = || -> Result<()> {
        let file =
            std::fs::File::create(&partial).ctx(|| format!("Writing {}", partial.display()))?;
        let gz = flate2::write::GzEncoder::new(
            std::io::BufWriter::new(file),
            flate2::Compression::fast(),
        );
        let mut tar = tar::Builder::new(gz);
        let mut done = 0;
        for (i, (path, rel, size)) in files.iter().enumerate() {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let mut f = std::fs::File::open(path).ctx(|| format!("Reading {rel}"))?;
            let mut h = tar::Header::new_gnu();
            h.set_size(*size);
            h.set_mode(0o644);
            h.set_entry_type(tar::EntryType::Regular);
            tar.append_data(&mut h, format!("{name}/{rel}"), &mut f)
                .ctx(|| format!("Packing {rel}"))?;
            done += size;
            emit(Progress::Copy {
                done_bytes: done,
                total_bytes: total,
                done_files: i as u64 + 1,
                total_files: files.len() as u64,
                current: rel.clone(),
            });
        }
        let gz = tar
            .into_inner()
            .ctx(|| format!("Writing {}", partial.display()))?;
        let w = gz
            .finish()
            .ctx(|| format!("Writing {}", partial.display()))?;
        w.into_inner()
            .map_err(|e| invalid(format!("Writing {}: {e}", partial.display())))?
            .sync_all()
            .ctx(|| format!("Writing {}", partial.display()))
    };
    match write() {
        Ok(()) => std::fs::rename(&partial, out).ctx(|| format!("Saving {}", out.display())),
        Err(e) => {
            let _ = std::fs::remove_file(&partial);
            Err(e)
        }
    }
}

/// Unpacks a `.tar.gz` made by [`pack`] into `dest/<its folder>` and checks every file. The
/// archive is untrusted: it is read twice, first to check every entry (plain files and folders
/// only, inside one top folder, no absolute paths or `..`) and that the space is there, then to
/// extract into a hidden folder that becomes the snapshot only after it verifies.
pub fn unpack(
    archive: &Path,
    dest: &Path,
    emit: &mut dyn FnMut(Progress),
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    use std::sync::atomic::Ordering;
    let open =
        || -> Result<tar::Archive<flate2::read::GzDecoder<std::io::BufReader<std::fs::File>>>> {
            let f =
                std::fs::File::open(archive).ctx(|| format!("Reading {}", archive.display()))?;
            Ok(tar::Archive::new(flate2::read::GzDecoder::new(
                std::io::BufReader::new(f),
            )))
        };
    let bad = |why: String| {
        invalid(format!(
            "{} is not a usable snapshot archive: {why}",
            archive.display()
        ))
    };
    // Entry path inside the top folder, as `/`-separated text.
    let split = |e: &tar::Entry<_>| -> Result<(String, String)> {
        let p = e.path().map_err(|e| bad(e.to_string()))?;
        let parts: Vec<String> = p
            .components()
            .map(|c| match c {
                std::path::Component::Normal(n) => Ok(n.to_string_lossy().into_owned()),
                _ => Err(bad(format!("unsafe path {}", p.display()))),
            })
            .collect::<Result<_>>()?;
        match parts.split_first() {
            Some((top, rest)) => Ok((top.clone(), rest.join("/"))),
            None => Err(bad("an empty path".into())),
        }
    };
    let mut top: Option<String> = None;
    let (mut total, mut count) = (0u64, 0u64);
    for e in open()?.entries().map_err(|e| bad(e.to_string()))? {
        let e = e.map_err(|e| bad(e.to_string()))?;
        let (t, rel) = split(&e)?;
        match e.header().entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => {
                check_rel(&rel, true).map_err(|_| bad(format!("unsafe path {rel}")))?;
            }
            tar::EntryType::Directory => {}
            k => return Err(bad(format!("{rel} is a {k:?} entry, not a plain file"))),
        }
        if !crate::paths::portable_component(&t) || t.starts_with('.') {
            return Err(bad(format!("unusable folder name {t:?}")));
        }
        if top.get_or_insert_with(|| t.clone()) != &t {
            return Err(bad("more than one top folder".into()));
        }
        total += e.size();
        count += 1;
        if count > 5_000_000 {
            return Err(bad("too many entries".into()));
        }
    }
    let top = top.ok_or_else(|| bad("it is empty".into()))?;
    let final_dir = dest.join(&top);
    if final_dir.exists() {
        return Err(invalid(format!("{} already exists.", final_dir.display())));
    }
    if let Some(free) = crate::platform::free_space(dest)
        && free < total + (64 << 20)
    {
        return Err(invalid(format!(
            "Not enough space in {}: {} needed, {free} free.",
            dest.display(),
            total
        )));
    }
    let staging = dest.join(format!(".{top}.unpacking"));
    let _ = std::fs::remove_dir_all(&staging);
    emit(Progress::Stage {
        label: format!("Unpacking {}", archive.display()),
        step: 1,
        steps: 2,
    });
    let mut extract = || -> Result<()> {
        let mut done = 0;
        for (i, e) in open()?
            .entries()
            .map_err(|e| bad(e.to_string()))?
            .enumerate()
        {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let mut e = e.map_err(|e| bad(e.to_string()))?;
            let (_, rel) = split(&e)?;
            if e.header().entry_type() == tar::EntryType::Directory || rel.is_empty() {
                continue;
            }
            let to = join_rel(&staging, &rel, true)?;
            crate::restore::ensure_dirs(&staging, to.parent().unwrap_or(&staging))?;
            let mut out = std::fs::File::create(&to).ctx(|| format!("Writing {}", to.display()))?;
            done += std::io::copy(&mut e, &mut out).ctx(|| format!("Unpacking {rel}"))?;
            emit(Progress::Copy {
                done_bytes: done,
                total_bytes: total,
                done_files: i as u64 + 1,
                total_files: count,
                current: rel,
            });
        }
        Ok(())
    };
    let checked = extract().and_then(|()| {
        emit(Progress::Stage {
            label: "Checking the unpacked snapshot".into(),
            step: 2,
            steps: 2,
        });
        let v = verify(&staging, emit, cancel)?;
        if v.ok {
            Ok(())
        } else {
            Err(invalid(format!(
                "The unpacked snapshot is damaged: {} missing and {} changed file(s).",
                v.missing.len(),
                v.corrupt.len()
            )))
        }
    });
    if let Err(e) = checked {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }
    std::fs::rename(&staging, &final_dir).ctx(|| format!("Saving {}", final_dir.display()))?;
    Ok(final_dir)
}

/// A snapshot packed into one file by [`pack`].
pub fn is_archive(p: &Path) -> bool {
    let n = p.to_string_lossy().to_ascii_lowercase();
    p.is_file() && (n.ends_with(".tar.gz") || n.ends_with(".tgz"))
}
