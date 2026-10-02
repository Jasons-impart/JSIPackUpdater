use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};

const MARKER: &str = "config/createdelight_release_info.json";

pub(super) fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub(super) fn launcher_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = home_dir() {
        roots.push(home.join(".local/share/PrismLauncher/instances"));
        roots.push(
            home.join(".var/app/org.prismlauncher.PrismLauncher/data/PrismLauncher/instances"),
        );
        roots.push(home.join("Library/Application Support/PrismLauncher/instances"));
        roots.push(home.join(".local/share/ModrinthApp/profiles"));
    }
    if let Some(appdata) = std::env::var_os("APPDATA") {
        roots.push(PathBuf::from(appdata).join("PrismLauncher/instances"));
    }
    roots
}

pub(super) fn is_instance(path: &Path) -> bool {
    path.join(MARKER).is_file()
}

fn discovered_instances() -> Vec<PathBuf> {
    discovered_in_roots(launcher_roots())
}

fn discovered_in_roots(roots: impl IntoIterator<Item = PathBuf>) -> Vec<PathBuf> {
    let mut instances = Vec::new();
    for root in roots.into_iter().filter(|root| root.is_dir()) {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten().take(10_000) {
            let path = entry.path();
            for candidate in [path.join("minecraft"), path.join(".minecraft"), path] {
                if is_instance(&candidate) && !instances.contains(&candidate) {
                    instances.push(candidate);
                }
            }
        }
    }
    instances.sort();
    instances.truncate(8);
    instances
}

fn expand_home(value: &str) -> PathBuf {
    if value == "~" {
        return home_dir().unwrap_or_else(|| PathBuf::from(value));
    }
    if let Some(rest) = value.strip_prefix("~/")
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(value)
}

pub(super) struct DirectorySuggestion {
    pub path: PathBuf,
    pub is_instance: bool,
}

pub(super) struct SuggestionResult {
    pub matches: Vec<DirectorySuggestion>,
    pub input_is_directory: bool,
}

fn matches_prefix(name: &str, prefix: &str) -> bool {
    if cfg!(windows) {
        name.to_lowercase().starts_with(&prefix.to_lowercase())
    } else {
        name.starts_with(prefix)
    }
}

pub(super) fn directory_suggestions(value: &str) -> Vec<DirectorySuggestion> {
    if value.is_empty() {
        return discovered_instances()
            .into_iter()
            .map(|path| DirectorySuggestion {
                path,
                is_instance: true,
            })
            .collect();
    }
    let path = expand_home(value);
    let absolute = if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return Vec::new(),
        }
    };
    let trailing_separator = value.ends_with('/') || (cfg!(windows) && value.ends_with('\\'));
    let (parent, prefix) = if trailing_separator {
        (absolute.as_path(), String::new())
    } else {
        (
            absolute.parent().unwrap_or(absolute.as_path()),
            absolute
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default(),
        )
    };
    let Ok(entries) = fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut matches: Vec<_> = entries
        .flatten()
        .take(10_000)
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_dir()
                && path
                    .file_name()
                    .is_some_and(|name| matches_prefix(&name.to_string_lossy(), &prefix))
        })
        .collect();
    matches.sort();
    matches.truncate(8);
    matches
        .into_iter()
        .map(|path| DirectorySuggestion {
            is_instance: is_instance(&path),
            path,
        })
        .collect()
}

pub(super) fn suggestion_worker() -> (Sender<String>, Receiver<(String, SuggestionResult)>) {
    let (request_tx, request_rx) = mpsc::channel::<String>();
    let (result_tx, result_rx) = mpsc::channel();
    std::thread::spawn(move || {
        while let Ok(mut value) = request_rx.recv() {
            while let Ok(newer) = request_rx.try_recv() {
                value = newer;
            }
            let result = SuggestionResult {
                input_is_directory: !value.is_empty() && expand_home(&value).is_dir(),
                matches: directory_suggestions(&value),
            };
            if result_tx.send((value, result)).is_err() {
                break;
            }
        }
    });
    (request_tx, result_rx)
}

pub(super) fn selected_path(selected: Option<&PathBuf>, shown: &str) -> PathBuf {
    selected.cloned().unwrap_or_else(|| expand_home(shown))
}

fn preferred_browser_root() -> PathBuf {
    let fallback = launcher_roots()
        .into_iter()
        .find(|root| root.is_dir())
        .or_else(home_dir)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."));
    fallback.canonicalize().unwrap_or(fallback)
}

pub(super) fn browser_start(shown: &str, selected: Option<&PathBuf>) -> PathBuf {
    if shown.is_empty() && selected.is_none() {
        return preferred_browser_root();
    }
    let path = selected_path(selected, shown);
    let path = if path.is_absolute() {
        path
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path,
        }
    };
    if path.is_dir() {
        return path.canonicalize().unwrap_or(path);
    }
    if let Some(parent) = path.parent()
        && parent.is_dir()
    {
        return parent
            .canonicalize()
            .unwrap_or_else(|_| parent.to_path_buf());
    }
    preferred_browser_root()
}

pub(super) struct BrowserDirectory {
    pub path: PathBuf,
    pub is_instance: bool,
}

pub(super) fn child_directories(
    current: &Path,
    generation: &AtomicU64,
    request_id: u64,
) -> io::Result<(bool, Vec<BrowserDirectory>)> {
    let cancelled = || generation.load(Ordering::Acquire) != request_id;
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "directory scan cancelled",
        ));
    }
    let current_instance = is_instance(current);
    let mut children = Vec::new();
    for entry in fs::read_dir(current)? {
        if cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "directory scan cancelled",
            ));
        }
        let path = entry?.path();
        if path.is_dir() {
            children.push(BrowserDirectory {
                is_instance: is_instance(&path),
                path,
            });
        }
    }
    children.sort_by(|left, right| left.path.cmp(&right.path));
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "directory scan cancelled",
        ));
    }
    Ok((current_instance, children))
}

#[cfg(test)]
mod tests {
    use super::{
        browser_start, child_directories, directory_suggestions, discovered_in_roots, expand_home,
        home_dir, preferred_browser_root, selected_path, suggestion_worker,
    };
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::AtomicU64;

    #[test]
    fn directory_suggestions_complete_partial_directory_name() {
        let root = tempfile::TempDir::new().unwrap();
        let matching = root.path().join("instance-one");
        fs::create_dir_all(matching.join("config")).unwrap();
        fs::write(
            matching.join("config/createdelight_release_info.json"),
            b"{}",
        )
        .unwrap();
        fs::create_dir(root.path().join("other")).unwrap();
        let input = root.path().join("inst").to_string_lossy().into_owned();
        let found = directory_suggestions(&input);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].path, matching);
        assert!(found[0].is_instance);
    }

    #[test]
    fn trailing_forward_slash_lists_children_and_tilde_expands_home() {
        let root = tempfile::TempDir::new().unwrap();
        let child = root.path().join("child");
        fs::create_dir(&child).unwrap();
        let found: Vec<_> = directory_suggestions(&format!("{}/", root.path().display()))
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(found, vec![child]);
        let (sender, receiver) = suggestion_worker();
        sender.send(format!("{}/", root.path().display())).unwrap();
        let (_, result) = receiver
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        assert!(result.input_is_directory);
        if let Some(home) = home_dir() {
            assert_eq!(expand_home("~/"), home);
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_completion_ignores_directory_name_case() {
        let root = tempfile::TempDir::new().unwrap();
        let matching = root.path().join("Instances");
        fs::create_dir(&matching).unwrap();
        let input = root.path().join("inst").to_string_lossy().into_owned();
        let found: Vec<_> = directory_suggestions(&input)
            .into_iter()
            .map(|entry| entry.path)
            .collect();
        assert_eq!(found, vec![matching]);
    }

    #[test]
    fn missing_browser_directory_reports_read_failure() {
        let root = tempfile::TempDir::new().unwrap();
        assert!(child_directories(&root.path().join("missing"), &AtomicU64::new(1), 1).is_err());
    }

    #[test]
    fn cancelled_browser_scan_stops_before_listing() {
        let root = tempfile::TempDir::new().unwrap();
        let error = child_directories(root.path(), &AtomicU64::new(2), 1)
            .err()
            .unwrap();
        assert_eq!(error.kind(), std::io::ErrorKind::Interrupted);
    }

    #[test]
    fn discovers_both_prism_game_directory_layouts() {
        let root = tempfile::TempDir::new().unwrap();
        let mut expected = Vec::new();
        for (name, game) in [("one", "minecraft"), ("two", ".minecraft")] {
            let path = root.path().join(name).join(game);
            fs::create_dir_all(path.join("config")).unwrap();
            fs::write(path.join("config/createdelight_release_info.json"), b"{}").unwrap();
            expected.push(path);
        }
        expected.sort();
        assert_eq!(discovered_in_roots([root.path().to_path_buf()]), expected);
    }

    #[test]
    fn relative_browser_start_is_absolute() {
        assert_eq!(
            browser_start(".", None),
            std::env::current_dir().unwrap().canonicalize().unwrap()
        );
        assert_eq!(browser_start("", None), preferred_browser_root());
    }

    #[cfg(unix)]
    #[test]
    fn selected_directory_preserves_non_unicode_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let path = PathBuf::from(OsString::from_vec(b"/tmp/instance\xff".to_vec()));
        assert_eq!(selected_path(Some(&path), &path.to_string_lossy()), path);
    }
}
