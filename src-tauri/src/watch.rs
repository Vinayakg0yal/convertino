//! Watched folders: a new file in a watched folder is converted on its own
//! (compress OBS recordings, screenshots to WebP, remove GPS from uploads).
//!
//! Only files that arrive while Convertino runs are converted; files already
//! there are left as they are. A file is picked up once it has stopped
//! growing for a few seconds (and, on Windows, nothing holds it open for
//! writing), so a recording isn't touched while it's still being written.
//! Results follow the same choice as folder jobs: a new folder next to the
//! watched one, next to the original, or in its place.

use crate::folder::{self, Action, Into, Opts};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter};

/// One watched folder, as Settings keeps it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", default)]
pub struct Watched {
    pub path: String,
    /// A folder action for one file: "folder.compress", "folder.to:image.webp",
    /// "folder.gps", "folder.normalize", "folder.audio".
    pub action: String,
    pub into: Into,
    pub on: bool,
    pub last: Option<LastRun>,
}

impl Default for Watched {
    fn default() -> Self {
        Watched { path: String::new(), action: "folder.compress".into(), into: Into::New, on: true, last: None }
    }
}

/// The last file a watched folder converted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct LastRun {
    /// Unix seconds.
    pub at: u64,
    pub file: String,
    /// "3.1 GB → 0.9 GB", or why it failed.
    pub result: String,
    pub ok: bool,
}

/// Actions a watched folder can take on each new file, for Settings' list.
pub const ACTIONS: &[(&str, &str)] = &[
    ("folder.compress", "Compress"),
    ("folder.to:video.mp4", "Videos → MP4"),
    ("folder.audio", "Videos → MP3 (audio only)"),
    ("folder.to:image.jpg", "Pictures → JPG"),
    ("folder.to:image.webp", "Pictures → WEBP"),
    ("folder.to:image.avif", "Pictures → AVIF"),
    ("folder.to:image.png", "Pictures → PNG"),
    ("folder.to:raw.jpg", "RAW photos → JPG"),
    ("folder.gps", "Remove GPS"),
    ("folder.to:audio.mp3", "Audio → MP3"),
    ("folder.normalize", "Normalize audio"),
    ("folder.to:doc.pdf", "Documents → PDF"),
];

/// How long a new file must stay the same size before it's converted.
const SETTLE: Duration = Duration::from_secs(5);
const TICK: Duration = Duration::from_secs(3);

#[derive(Default)]
struct FolderState {
    /// Files already there, made by Convertino, or done.
    known: HashSet<PathBuf>,
    /// New files: (size, since when it's been that size).
    pending: HashMap<PathBuf, (u64, Instant)>,
}

/// A path added from the folder wheel's "Watch this folder…", for Settings to pick up.
fn pending_add() -> &'static Mutex<Option<String>> {
    static P: OnceLock<Mutex<Option<String>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(None))
}

pub fn set_pending(path: String) {
    if let Ok(mut p) = pending_add().lock() {
        *p = Some(path);
    }
}

pub fn take_pending() -> Option<String> {
    pending_add().lock().ok().and_then(|mut p| p.take())
}

/// Starts watching (once, when Convertino starts).
pub fn start(app: AppHandle) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(move || {
        let mut states: HashMap<PathBuf, FolderState> = HashMap::new();
        loop {
            std::thread::sleep(TICK);
            let list = crate::settings::get().watched;
            let active: HashSet<PathBuf> = list.iter().filter(|w| w.on && !w.path.is_empty()).map(|w| PathBuf::from(&w.path)).collect();
            // A folder switched off forgets what it knew: switched on again, only newer files count.
            states.retain(|p, _| active.contains(p));
            for w in list.iter().filter(|w| w.on && !w.path.is_empty()) {
                let dir = PathBuf::from(&w.path);
                let fresh = !states.contains_key(&dir);
                let st = states.entry(dir.clone()).or_default();
                let files = visible_files(&dir);
                if fresh {
                    st.known.extend(files);
                    continue;
                }
                for f in files {
                    if st.known.contains(&f) {
                        continue;
                    }
                    let size = f.metadata().map(|m| m.len()).unwrap_or(0);
                    let settled = match st.pending.get(&f) {
                        Some((s, since)) if *s == size && size > 0 => since.elapsed() >= SETTLE && !held_open(&f),
                        _ => {
                            st.pending.insert(f.clone(), (size, Instant::now()));
                            false
                        }
                    };
                    if settled {
                        st.pending.remove(&f);
                        st.known.insert(f.clone());
                        let made = convert_one(&app, w, &f);
                        // Results that land in the watched folder aren't new files to convert.
                        st.known.extend(made);
                    }
                }
                st.pending.retain(|p, _| p.exists());
            }
        }
    });
}

/// Files Explorer / Finder shows in the folder (not inside subfolders).
fn visible_files(dir: &Path) -> Vec<PathBuf> {
    match folder::scan_top(&[dir.to_path_buf()]) {
        Ok(s) => s.items.into_iter().filter(|i| !i.earlier).map(|i| i.path).collect(),
        Err(_) => Vec::new(),
    }
}

/// Windows: another program still has the file open for writing (a recording in progress).
#[cfg(windows)]
fn held_open(f: &Path) -> bool {
    // A read-only file can't be opened for writing anyway: only the size counts.
    if f.metadata().map(|m| m.permissions().readonly()).unwrap_or(true) {
        return false;
    }
    std::fs::OpenOptions::new().read(true).write(true).open(f).is_err()
}

#[cfg(not(windows))]
fn held_open(_f: &Path) -> bool {
    false
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Converts one new file as its watched folder says. Returns what it made.
fn convert_one(app: &AppHandle, w: &Watched, file: &Path) -> Vec<PathBuf> {
    let name = crate::engine::file_name(file);
    let busy = app.clone();
    let Some((made, last)) = convert_file(w, file, &move |on| tray_busy(&busy, on.then_some(name.as_str()))) else { return Vec::new() };
    crate::settings::update(|s| {
        if let Some(x) = s.watched.iter_mut().find(|x| x.path == w.path) {
            x.last = Some(last.clone());
        }
    });
    let _ = app.emit_to("settings", "settings-changed", ());
    let _ = app.emit_to("main", "watched", serde_json::json!({ "folder": w.path, "file": last.file, "result": last.result, "ok": last.ok }));
    made
}

/// The conversion itself (no windows): what it made and the line Settings shows.
/// None when the file isn't something this folder converts.
fn convert_file(w: &Watched, file: &Path, busy: &dyn Fn(bool)) -> Option<(Vec<PathBuf>, LastRun)> {
    let dir = PathBuf::from(&w.path);
    let name = crate::engine::file_name(file);
    let action = Action::from_id(&w.action)?;
    let mut scan = folder::scan_top(&[dir.clone()]).ok()?;
    // Only this file: the rest of the folder is left as it is.
    scan.items.retain(|i| i.path == file);
    scan.alone.clear();
    scan.skipped_dirs.clear();
    if scan.items.is_empty() {
        return None;
    }
    let prefs = crate::settings::get();
    let opts = Opts { into: w.into, subfolders: false, skip_done: true, ..Opts::default() };
    let mut plan = match folder::plan(&scan, &action, &opts, &prefs.quality, &prefs) {
        Ok(p) => p,
        // Not something this folder converts (a text file in a video folder): nothing to say.
        Err(e) => {
            log::info!("watched {}: {name} left alone ({e})", dir.display());
            return None;
        }
    };
    // One holding folder per watched folder (not a new one for every file).
    if plan.replace {
        let parent = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| dir.clone());
        plan.holding = Some(parent.join(folder::HOLDING).join(folder::folder_name(&dir)));
    }
    log::info!("watched {}: converting {name}", dir.display());
    busy(true);
    let id = crate::engine::new_id();
    let o = folder::run(id, &plan, Arc::new(|_| {}), Arc::new(|_, _| {}));
    busy(false);
    let ok = o.failed == 0 && (!o.made.is_empty() || !o.replaced.is_empty());
    let result = if ok && o.before > 0 {
        format!("{} → {}", crate::engine::human_size(o.before), crate::engine::human_size(o.after))
    } else if ok {
        "Done".into()
    } else {
        o.rows.iter().find(|r| r.kind != folder::RowKind::Done).map(|r| r.status.clone()).unwrap_or_else(|| "Nothing was made".into())
    };
    // (Replaced files are among `made` already.)
    let made = o.made.clone();
    Some((made, LastRun { at: now_secs(), file: name, result, ok }))
}

/// The tray icon gets a dot (and its tooltip says what) while a watched folder converts.
fn tray_busy(app: &AppHandle, file: Option<&str>) {
    let Some(tray) = app.tray_by_id("main-tray") else { return };
    let _ = tray.set_tooltip(Some(match file {
        Some(f) => format!("Convertino · converting {f} (watched folder)"),
        None => "Convertino".to_string(),
    }));
    let Some(icon) = app.default_window_icon() else { return };
    let img = match file {
        Some(_) => with_dot(icon),
        None => tauri::image::Image::new_owned(icon.rgba().to_vec(), icon.width(), icon.height()),
    };
    let _ = tray.set_icon(Some(img));
}

/// The icon with a green dot in its bottom-right corner.
fn with_dot(icon: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let (w, h) = (icon.width() as i64, icon.height() as i64);
    let mut px = icon.rgba().to_vec();
    let r = (w.min(h) as f64 * 0.2).max(2.0);
    let (cx, cy) = (w as f64 - r - 0.5, h as f64 - r - 0.5);
    for y in 0..h {
        for x in 0..w {
            let d = ((x as f64 - cx).powi(2) + (y as f64 - cy).powi(2)).sqrt();
            let i = ((y * w + x) * 4) as usize;
            if d <= r + 1.0 {
                // A dark ring around the dot keeps it visible on light and dark taskbars.
                let (c, a) = if d <= r - 1.0 { ([0x2e, 0xa0, 0x43], 255) } else { ([0x10, 0x10, 0x10], 255) };
                px[i] = c[0];
                px[i + 1] = c[1];
                px[i + 2] = c[2];
                px[i + 3] = a;
            }
        }
    }
    tauri::image::Image::new_owned(px, w as u32, h as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_action_is_a_folder_action() {
        for (id, _) in ACTIONS {
            let a = Action::from_id(id).unwrap_or_else(|| panic!("{id}"));
            assert!(a.is_batch(), "{id}");
        }
    }

    #[test]
    fn a_new_picture_is_converted_and_the_rest_left_alone() {
        if crate::tools::find(crate::tools::Tool::Magick).is_none() || crate::tools::find(crate::tools::Tool::Ffmpeg).is_none() {
            return;
        }
        let d = std::env::temp_dir().join(format!("convertino-watch-{}", std::process::id())).join("Shots");
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
        std::fs::create_dir_all(&d).unwrap();
        let png = d.join("shot.png");
        let ff = crate::tools::find(crate::tools::Tool::Ffmpeg).unwrap();
        assert!(std::process::Command::new(ff).args(["-loglevel", "error", "-f", "lavfi", "-i", "testsrc2=size=320x240", "-frames:v", "1"]).arg(&png).status().unwrap().success());
        std::fs::write(d.join("notes.txt"), "x").unwrap();
        let w = Watched { path: d.to_string_lossy().into_owned(), action: "folder.to:image.jpg".into(), into: Into::Beside, on: true, last: None };
        let busy = std::sync::Mutex::new(Vec::new());
        let (made, last) = convert_file(&w, &png, &|b| busy.lock().unwrap().push(b)).unwrap();
        assert_eq!(made, vec![d.join("shot.jpg")]);
        assert!(last.ok, "{last:?}");
        assert_eq!(*busy.lock().unwrap(), vec![true, false]);
        // A text file isn't a picture: nothing happens, nothing to report.
        assert!(convert_file(&w, &d.join("notes.txt"), &|_| {}).is_none());
        // Replace: the original moves to the watched folder's own holding folder.
        let w2 = Watched { into: Into::Replace, ..w };
        std::fs::remove_file(d.join("shot.jpg")).unwrap();
        let (made, _) = convert_file(&w2, &png, &|_| {}).unwrap();
        assert_eq!(made, vec![d.join("shot.jpg")]);
        assert!(d.parent().unwrap().join(folder::HOLDING).join("Shots").join("shot.png").is_file());
        let _ = std::fs::remove_dir_all(d.parent().unwrap());
    }

    #[test]
    fn settings_round_trip() {
        let w = Watched { path: "D:\\OBS".into(), action: "folder.compress".into(), into: Into::Replace, on: true, last: None };
        let j = serde_json::to_string(&w).unwrap();
        assert!(j.contains("\"into\":\"replace\""));
        let back: Watched = serde_json::from_str(&j).unwrap();
        assert_eq!(back, w);
        let partial: Watched = serde_json::from_str("{\"path\":\"x\"}").unwrap();
        assert_eq!(partial.action, "folder.compress");
        assert!(partial.on);
    }
}
