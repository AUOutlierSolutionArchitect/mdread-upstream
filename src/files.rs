//! Find sibling Markdown files for the file sidebar.

use std::path::{Path, PathBuf};
use std::sync::mpsc;

const EXTS: &[&str] = &["md", "markdown", "mdown", "mkd", "mdx"];
const SKIP_DIRS: &[&str] = &["node_modules", "target", ".git", "vendor", "dist", "build", "__pycache__", ".venv", "venv"];
const MAX_FILES: usize = 2000;
const MAX_DEPTH: usize = 4;
/// Folders one scan may read, so a sidebar moved up to `/` or a drive root stays quick.
const MAX_DIRS: usize = 5000;

pub fn is_markdown(p: &Path) -> bool {
    p.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| EXTS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

pub fn scan(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs_left = MAX_DIRS;
    walk(dir, 0, &mut dirs_left, &mut out);
    out.sort_by(|a, b| {
        let da = a.components().count();
        let db = b.components().count();
        da.cmp(&db).then_with(|| a.to_string_lossy().to_lowercase().cmp(&b.to_string_lossy().to_lowercase()))
    });
    out
}

fn walk(dir: &Path, depth: usize, dirs_left: &mut usize, out: &mut Vec<PathBuf>) {
    if out.len() >= MAX_FILES || *dirs_left == 0 {
        return;
    }
    *dirs_left -= 1;
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let path = e.path();
        let name = e.file_name();
        let name = name.to_string_lossy();
        let Ok(ft) = e.file_type() else { continue };
        if hidden(&e) {
            continue;
        }
        if ft.is_dir() {
            if depth + 1 < MAX_DEPTH && !skipped(&name) {
                walk(&path, depth + 1, dirs_left, out);
            }
        } else if (ft.is_file() || ft.is_symlink()) && is_markdown(&path) {
            out.push(path);
            if out.len() >= MAX_FILES {
                return;
            }
        }
    }
}

fn skipped(dir_name: &str) -> bool {
    dir_name.starts_with('.') || SKIP_DIRS.contains(&dir_name)
}

/// Whether `scan(root)` lists `path`: it is under `root`, no more than `MAX_DEPTH` deep,
/// and not in a folder the scan skips.
pub fn covers(root: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else { return false };
    let parts: Vec<_> = rel.iter().collect();
    let Some((_, dirs)) = parts.split_last() else { return false };
    parts.len() <= MAX_DEPTH && dirs.iter().all(|d| !skipped(&d.to_string_lossy()))
}

/// The folder the files sidebar shows once `path` is open, when it showed `current`
/// before: still `current` if it lists `path` (a link into a subfolder); the folder both
/// share if that is just above `path`'s folder (a link to a neighbouring folder), so the
/// way back stays in the list; otherwise `path`'s own folder. Never the home folder or
/// one above it, which would list far too much.
pub fn sidebar_root(current: Option<&Path>, path: &Path, home: Option<&Path>) -> Option<PathBuf> {
    let dir = path.parent()?;
    if let Some(cur) = current {
        if covers(cur, path) {
            return Some(cur.to_path_buf());
        }
        if let Some(shared) = cur.ancestors().find(|a| dir.starts_with(a)) {
            let too_wide = shared.parent().is_none() || home.is_some_and(|h| h.starts_with(shared));
            if dir.parent() == Some(shared) && !too_wide && covers(shared, path) {
                return Some(shared.to_path_buf());
            }
        }
    }
    Some(dir.to_path_buf())
}

/// Windows marks hidden folders (AppData, $RECYCLE.BIN, System Volume Information) with
/// attributes rather than a leading dot. The attributes come with the directory listing,
/// so this costs no extra system call.
#[cfg(windows)]
fn hidden(e: &std::fs::DirEntry) -> bool {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_SYSTEM};
    e.metadata().is_ok_and(|m| m.file_attributes() & (FILE_ATTRIBUTE_HIDDEN | FILE_ATTRIBUTE_SYSTEM) != 0)
}

#[cfg(not(windows))]
fn hidden(_: &std::fs::DirEntry) -> bool {
    false
}

pub fn spawn_scan(dir: PathBuf, ctx: egui::Context) -> mpsc::Receiver<Vec<PathBuf>> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("file-scan".into())
        .spawn(move || {
            let files = scan(&dir);
            let _ = tx.send(files);
            ctx.request_repaint();
        })
        .ok();
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn covers_files_the_scan_lists() {
        let root = p("/r/repo");
        assert!(covers(&root, &p("/r/repo/README.md")));
        assert!(covers(&root, &p("/r/repo/a/b/c/deep.md")));
        assert!(!covers(&root, &p("/r/repo/a/b/c/d/too-deep.md")));
        assert!(!covers(&root, &p("/r/repo/node_modules/x/README.md")));
        assert!(!covers(&root, &p("/r/repo/.github/CONTRIBUTING.md")));
        assert!(!covers(&root, &p("/r/other/README.md")));
        assert!(!covers(&root, &root));
    }

    #[test]
    fn sidebar_root_follows_links_without_losing_the_way_back() {
        let home = p("/home/u");
        let root = |cur: &str, file: &str| sidebar_root(Some(&p(cur)), &p(file), Some(&home)).unwrap();
        // First file: its own folder.
        assert_eq!(sidebar_root(None, &p("/home/u/repo/docs/plan.md"), Some(&home)), Some(p("/home/u/repo/docs")));
        // Into a subfolder the sidebar lists: unchanged.
        assert_eq!(root("/home/u/repo/docs", "/home/u/repo/docs/adr/0001.md"), p("/home/u/repo/docs"));
        // Up a level: that folder, which lists the one before.
        assert_eq!(root("/home/u/repo/docs", "/home/u/repo/README.md"), p("/home/u/repo"));
        // A neighbouring folder: the folder both are in.
        assert_eq!(root("/home/u/repo/docs", "/home/u/repo/src/README.md"), p("/home/u/repo"));
        // Further away: the new file's folder.
        assert_eq!(root("/home/u/repo/docs", "/home/u/repo/src/a/README.md"), p("/home/u/repo/src/a"));
        // Never the home folder or above it.
        assert_eq!(root("/home/u/notes", "/home/u/todo/list.md"), p("/home/u/todo"));
        assert_eq!(root("/home/u", "/home/v/x.md"), p("/home/v"));
        assert_eq!(root("/srv/a", "/srv2/x.md"), p("/srv2"));
        // Nor one that would not list the new file.
        assert_eq!(root("/home/u/repo/docs", "/home/u/repo/.github/README.md"), p("/home/u/repo/.github"));
    }
}
