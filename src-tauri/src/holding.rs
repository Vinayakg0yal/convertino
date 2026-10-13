//! The Holding Area: where originals go when a job replaces them.
//!
//! Shared with Crofty's other tools (Drive Cleanup reviews and empties it); the
//! layout follows the contract between Crofty's tools (TOOLS-CONTRACT.md):
//! - files on the system drive (Windows) or under the home folder:
//!   `<home>\_Quarantine\<batch>\<path inside home>` (outside home: `_outside\<path>`)
//! - files on any other drive: `X:\_Quarantine\<batch>\<path on the drive>`
//! - each batch has a `manifest.json` listing every original, written as each one moves,
//!   so nothing in the Holding Area is ever unlisted.
//!
//! On screen it is always "the Holding Area"; the folder keeps Drive Cleanup's name.

use serde_json::{json, Value};
use std::path::{Component, Path, PathBuf, Prefix};
use std::time::{SystemTime, UNIX_EPOCH};

/// The folder's name on disk (Drive Cleanup's name for it).
pub const DIR: &str = "_Quarantine";
/// Originals moved from outside the Holding Area's parent folder (C:\Games\… held in the user folder).
const OUTSIDE: &str = "_outside";
/// Tests (and anyone who wants it elsewhere) can put every Holding Area under one folder.
const BASE_ENV: &str = "CONVERTINO_HOLDING_BASE";

/// A batch name: the time a job started plus "convertino" ("2026-10-13-09-41-07 convertino").
pub fn batch_name() -> String {
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    format!("{} convertino", stamp(secs))
}

/// UTC "YYYY-MM-DD-HH-MM-SS" (the same shape Drive Cleanup uses).
fn stamp(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!("{y:04}-{m:02}-{d:02}-{:02}-{:02}-{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

fn home() -> Option<PathBuf> {
    std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).map(PathBuf::from)
}

fn starts_with_ci(p: &Path, base: &Path) -> bool {
    if cfg!(windows) {
        let (a, b) = (p.to_string_lossy().to_lowercase(), base.to_string_lossy().to_lowercase());
        let b = b.trim_end_matches(['\\', '/']);
        a == b || a.starts_with(&format!("{b}\\")) || a.starts_with(&format!("{b}/"))
    } else {
        p.starts_with(base)
    }
}

fn strip_ci(p: &Path, base: &Path) -> PathBuf {
    let n = base.components().count();
    p.components().skip(n).collect()
}

/// The drive letter of a Windows path ("D:"), if it has one.
fn drive_of(p: &Path) -> Option<String> {
    match p.components().next() {
        Some(Component::Prefix(pre)) => match pre.kind() {
            Prefix::Disk(l) | Prefix::VerbatimDisk(l) => Some(format!("{}:", (l as char).to_ascii_uppercase())),
            _ => None,
        },
        _ => None,
    }
}

/// The Holding Area for `original` and where the original goes inside a batch:
/// (Holding Area folder, path of the original inside the batch).
pub fn place(original: &Path) -> (PathBuf, PathBuf) {
    if let Some(b) = std::env::var_os(BASE_ENV) {
        let base = PathBuf::from(b);
        let rel: PathBuf = original.components().filter(|c| matches!(c, Component::Normal(_))).collect();
        return (base.join(DIR), rel);
    }
    let home = home();
    if let Some(h) = &home {
        if starts_with_ci(original, h) {
            return (h.join(DIR), strip_ci(original, h));
        }
    }
    if cfg!(windows) {
        let sys = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".into()).to_uppercase();
        if let Some(d) = drive_of(original) {
            if d != sys {
                let root = PathBuf::from(format!("{d}\\"));
                return (root.join(DIR), strip_ci(original, &root));
            }
        }
    } else if cfg!(target_os = "macos") {
        // Another disk: /Volumes/<disk>/_Quarantine
        let mut it = original.components();
        if let (Some(Component::RootDir), Some(Component::Normal(v)), Some(Component::Normal(disk))) = (it.next(), it.next(), it.next()) {
            if v == "Volumes" {
                let root = PathBuf::from("/Volumes").join(disk);
                return (root.join(DIR), strip_ci(original, &root));
            }
        }
    }
    // The system drive outside the user folder (or anything else): the user folder's Holding Area.
    let base = home.unwrap_or_else(std::env::temp_dir);
    let rel: PathBuf = original.components().filter(|c| matches!(c, Component::Normal(_))).collect();
    (base.join(DIR), Path::new(OUTSIDE).join(rel))
}

/// Where `original` goes in batch `batch`.
pub fn held_path(original: &Path, batch: &str) -> PathBuf {
    let (area, rel) = place(original);
    area.join(batch).join(rel)
}

/// The batch folder a held file is in (the folder right under `_Quarantine`).
pub fn batch_dir_of(held: &Path) -> Option<PathBuf> {
    held.ancestors().find(|a| a.parent().and_then(|p| p.file_name()).is_some_and(|n| n == DIR)).map(Path::to_path_buf)
}

fn read_manifest(batch_dir: &Path) -> Value {
    std::fs::read_to_string(batch_dir.join("manifest.json")).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or(Value::Null)
}

fn write_manifest(batch_dir: &Path, m: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(m).map_err(|e| e.to_string())?;
    let tmp = batch_dir.join("manifest.json.tmp");
    std::fs::write(&tmp, text).map_err(|e| format!("Couldn't write the Holding Area's list: {e}"))?;
    std::fs::rename(&tmp, batch_dir.join("manifest.json")).map_err(|e| format!("Couldn't write the Holding Area's list: {e}"))
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

/// Adds one moved original to its batch's list. `replaced` is the file now in its place.
pub fn record(held: &Path, original: &Path, replaced: &Path, bytes: u64) -> Result<(), String> {
    let batch_dir = batch_dir_of(held).ok_or_else(|| format!("{} isn't in a Holding Area", held.display()))?;
    let mut m = read_manifest(&batch_dir);
    if !m.is_object() {
        let batch = batch_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        m = json!({ "batch": batch, "kind": "convertino", "createdAt": now_ms(), "entries": [] });
    }
    let entry = json!({
        "from": original.to_string_lossy(),
        "to": held.to_string_lossy(),
        "bytes": bytes,
        "isDir": false,
        "label": "Replaced by Convertino",
        "replaced": replaced.to_string_lossy(),
    });
    if let Some(list) = m.get_mut("entries").and_then(Value::as_array_mut) {
        list.push(entry);
    }
    write_manifest(&batch_dir, &m)
}

/// After an original moved back out: drops it from its batch's list, and the
/// batch itself (folders and list) once nothing is left in it.
pub fn forget(held: &Path) {
    let Some(batch_dir) = batch_dir_of(held) else { return };
    let mut m = read_manifest(&batch_dir);
    let mut left = 0;
    if let Some(list) = m.get_mut("entries").and_then(Value::as_array_mut) {
        let h = held.to_string_lossy();
        list.retain(|e| e.get("to").and_then(Value::as_str) != Some(&h));
        left = list.len();
    }
    if left == 0 {
        remove_empty(&batch_dir);
        let _ = std::fs::remove_file(batch_dir.join("manifest.json"));
        let _ = std::fs::remove_dir(&batch_dir);
        // The Holding Area itself goes too when this was its last batch.
        if let Some(area) = batch_dir.parent() {
            let _ = std::fs::remove_dir(area);
        }
    } else {
        let _ = write_manifest(&batch_dir, &m);
        if let Some(dir) = held.parent() {
            remove_empty_up(dir, &batch_dir);
        }
    }
}

fn remove_empty(dir: &Path) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                remove_empty(&e.path());
                let _ = std::fs::remove_dir(e.path());
            }
        }
    }
}

fn remove_empty_up(mut dir: &Path, stop: &Path) {
    while dir != stop && dir.starts_with(stop) {
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => break,
        }
    }
}

/// "the Holding Area (D:\_Quarantine\2026-10-13-09-41-07 convertino)"
pub fn describe(batch_dir: &Path) -> String {
    format!("the Holding Area ({})", batch_dir.display())
}

/// Tests: every Holding Area goes under one temporary folder (set once for the whole test run).
#[cfg(test)]
pub fn test_base() -> PathBuf {
    static ONCE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    ONCE.get_or_init(|| {
        let b = std::env::temp_dir().join(format!("convertino-holding-{}", std::process::id()));
        std::env::set_var(BASE_ENV, &b);
        b
    })
    .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_is_utc_date_time() {
        assert_eq!(stamp(0), "1970-01-01-00-00-00");
        assert_eq!(stamp(1_791_867_100), "2026-10-13-04-51-40");
        assert_eq!(stamp(951_782_400), "2000-02-29-00-00-00");
    }

    #[test]
    fn batch_dir_is_the_folder_under_the_area() {
        let held = Path::new("/x/_Quarantine/2026-10-13-04-51-40 convertino/Clips/a.mov");
        assert_eq!(batch_dir_of(held).unwrap(), Path::new("/x/_Quarantine/2026-10-13-04-51-40 convertino"));
        assert!(batch_dir_of(Path::new("/x/y/a.mov")).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn windows_places() {
        if std::env::var_os(BASE_ENV).is_some() {
            return; // another test put every Holding Area in a temp folder
        }
        let home = home().unwrap();
        let (a, r) = place(&home.join("Videos").join("a.mp4"));
        assert_eq!((a, r), (home.join(DIR), PathBuf::from("Videos\\a.mp4")));
        let (a, r) = place(Path::new("Z:\\Clips\\a.mp4"));
        assert_eq!((a, r), (PathBuf::from("Z:\\_Quarantine"), PathBuf::from("Clips\\a.mp4")));
    }

    #[test]
    fn record_and_forget_keep_the_list_honest() {
        let root = std::env::temp_dir().join(format!("convertino-holding-unit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let batch = root.join(DIR).join("2026-10-13-04-51-40 convertino");
        let a = batch.join("Clips").join("a.mov");
        let b = batch.join("Clips").join("b.mov");
        for p in [&a, &b] {
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "x").unwrap();
        }
        record(&a, Path::new("/d/Clips/a.mov"), Path::new("/d/Clips/a.mp4"), 1).unwrap();
        record(&b, Path::new("/d/Clips/b.mov"), Path::new("/d/Clips/b.mp4"), 1).unwrap();
        let m = read_manifest(&batch);
        assert_eq!(m["kind"], "convertino");
        assert_eq!(m["batch"], "2026-10-13-04-51-40 convertino");
        assert_eq!(m["entries"].as_array().unwrap().len(), 2);
        assert_eq!(m["entries"][0]["replaced"], "/d/Clips/a.mp4");
        // a moves back out: its entry goes, b stays listed
        std::fs::remove_file(&a).unwrap();
        forget(&a);
        assert_eq!(read_manifest(&batch)["entries"].as_array().unwrap().len(), 1);
        // b too: the batch and the empty Holding Area go
        std::fs::remove_file(&b).unwrap();
        forget(&b);
        assert!(!batch.exists());
        assert!(!root.join(DIR).exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
