//! Operations on finished snapshots: summary, integrity verification, deletion.

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
        base: m.base.map(|b| b.name),
    })
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
