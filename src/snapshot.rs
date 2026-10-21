use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Map, Value};

use crate::error::Error;

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TestMapTree {
    pub sha: Option<String>,
    pub prefix: String,
    pub files: Map<String, Value>,
    pub dirty: Vec<String>,
}

pub fn snapshot_tree(cwd: &Path) -> Result<Option<TestMapTree>, Error> {
    let Some(root) = repo_root(cwd) else {
        return Ok(None);
    };
    let Some(mut files) = committed_files(&root) else {
        return Ok(None);
    };
    let dirty = apply_working_tree(&root, &mut files);
    Ok(Some(TestMapTree {
        sha: head_sha(&root),
        prefix: prefix_for(&root, cwd),
        files,
        dirty,
    }))
}

fn repo_root(cwd: &Path) -> Option<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let path = PathBuf::from(out.trim());
    if path.as_os_str().is_empty() {
        None
    } else {
        Some(path)
    }
}

fn head_sha(root: &Path) -> Option<String> {
    let out = git(root, &["rev-parse", "HEAD"])?;
    let sha = out.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_owned())
    }
}

fn prefix_for(root: &Path, cwd: &Path) -> String {
    let Ok(root) = root.canonicalize() else {
        return String::new();
    };
    let Ok(cwd) = cwd.canonicalize() else {
        return String::new();
    };
    if cwd == root {
        return String::new();
    }
    let Ok(rest) = cwd.strip_prefix(&root) else {
        return String::new();
    };
    if rest.as_os_str().is_empty() {
        return String::new();
    }
    let mut prefix = rest.to_string_lossy().replace('\\', "/");
    if !prefix.ends_with('/') {
        prefix.push('/');
    }
    prefix
}

fn committed_files(root: &Path) -> Option<Map<String, Value>> {
    let out = git(root, &["ls-tree", "-r", "-z", "--full-tree", "HEAD"])?;
    let mut files = Map::new();
    for entry in out.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let Some((meta, path)) = entry.split_once('\t') else {
            continue;
        };
        let mut parts = meta.split(' ');
        let _mode = parts.next();
        let _kind = parts.next();
        let Some(blob) = parts.next() else {
            continue;
        };
        files.insert(path.to_owned(), Value::String(blob.to_owned()));
    }
    Some(files)
}

fn apply_working_tree(root: &Path, files: &mut Map<String, Value>) -> Vec<String> {
    let Some(out) = git(
        root,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=no",
            "--no-renames",
        ],
    ) else {
        return Vec::new();
    };
    if out.is_empty() {
        return Vec::new();
    }
    let mut changed = Vec::new();
    let mut deleted = Vec::new();
    for entry in out.split('\0') {
        if entry.len() < 4 {
            continue;
        }
        let status = &entry[..2];
        let path = &entry[3..];
        if status.contains('D') && !root.join(path).exists() {
            files.remove(path);
            deleted.push(path.to_owned());
        } else {
            changed.push(path.to_owned());
        }
    }
    if let Some(hashed) = hash_objects(root, &changed) {
        for (path, blob) in hashed {
            files.insert(path, Value::String(blob));
        }
    }
    changed.extend(deleted);
    changed
}

fn hash_objects(root: &Path, paths: &[String]) -> Option<Vec<(String, String)>> {
    let existing: Vec<&String> = paths
        .iter()
        .filter(|path| root.join(path).is_file())
        .collect();
    if existing.is_empty() {
        return Some(Vec::new());
    }
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["hash-object", "--stdin-paths"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdin = child.stdin.take()?;
    let input: String = existing.iter().map(|path| format!("{path}\n")).collect();
    let writer = std::thread::spawn(move || {
        use std::io::Write;
        stdin.write_all(input.as_bytes())
    });
    let output = child.wait_with_output().ok()?;
    writer.join().ok()?.ok()?;
    if !output.status.success() {
        return Some(Vec::new());
    }
    let hashes: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .split('\n')
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    Some(
        existing
            .into_iter()
            .zip(hashes)
            .map(|(path, blob)| (path.clone(), blob))
            .collect(),
    )
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if output.status.success() {
        Some(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn git_cmd(cwd: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "{args:?}");
    }

    fn init_repo() -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "transport-snapshot-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&root).unwrap();
        git_cmd(&root, &["init"]);
        git_cmd(&root, &["config", "user.email", "snapshot@example.com"]);
        git_cmd(&root, &["config", "user.name", "Snapshot"]);
        git_cmd(&root, &["config", "commit.gpgsign", "false"]);
        root
    }

    #[test]
    fn snapshot_matches_git_hashes_and_ignores_untracked() {
        let root = init_repo();
        fs::write(root.join("keep.txt"), b"one\n").unwrap();
        git_cmd(&root, &["add", "keep.txt"]);
        git_cmd(&root, &["commit", "-m", "add keep"]);
        fs::write(root.join("noise.log"), b"ignore me\n").unwrap();

        let tree = snapshot_tree(&root).unwrap().unwrap();
        let expected = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["hash-object", "keep.txt"])
            .output()
            .unwrap();
        let blob = String::from_utf8_lossy(&expected.stdout).trim().to_owned();
        assert_eq!(
            tree.files.get("keep.txt").and_then(Value::as_str),
            Some(blob.as_str())
        );
        assert!(!tree.files.contains_key("noise.log"));
        assert!(tree.dirty.is_empty());
        assert_eq!(tree.prefix, "");
        assert!(tree.sha.is_some());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_folds_edits_and_deletions() {
        let root = init_repo();
        fs::write(root.join("keep.txt"), b"one\n").unwrap();
        fs::write(root.join("gone.txt"), b"two\n").unwrap();
        git_cmd(&root, &["add", "."]);
        git_cmd(&root, &["commit", "-m", "base"]);
        fs::write(root.join("keep.txt"), b"changed\n").unwrap();
        fs::remove_file(root.join("gone.txt")).unwrap();

        let tree = snapshot_tree(&root).unwrap().unwrap();
        let expected = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["hash-object", "keep.txt"])
            .output()
            .unwrap();
        let blob = String::from_utf8_lossy(&expected.stdout).trim().to_owned();
        assert_eq!(
            tree.files.get("keep.txt").and_then(Value::as_str),
            Some(blob.as_str())
        );
        assert!(!tree.files.contains_key("gone.txt"));
        assert!(tree.dirty.iter().any(|path| path == "keep.txt"));
        assert!(tree.dirty.iter().any(|path| path == "gone.txt"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_prefix_is_relative_to_the_command() {
        let root = init_repo();
        let nested = root.join("services").join("api");
        fs::create_dir_all(&nested).unwrap();
        fs::write(root.join("README"), b"root\n").unwrap();
        git_cmd(&root, &["add", "README"]);
        git_cmd(&root, &["commit", "-m", "readme"]);
        let tree = snapshot_tree(&nested).unwrap().unwrap();
        assert_eq!(tree.prefix, "services/api/");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshot_outside_a_checkout_is_unavailable() {
        let dir = std::env::temp_dir().join(format!("transport-not-a-repo-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("not-a-repo");
        fs::write(&marker, b"x").unwrap();
        let isolated = dir.join("empty");
        fs::create_dir_all(&isolated).unwrap();
        assert!(snapshot_tree(&isolated).unwrap().is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn snapshot_hashes_thousands_of_dirty_files() {
        let root = init_repo();
        let count = 3200;
        for index in 0..count {
            fs::write(root.join(format!("f{index:04}.txt")), b"v1\n").unwrap();
        }
        git_cmd(&root, &["add", "."]);
        git_cmd(&root, &["commit", "-m", "many"]);
        for index in 0..count {
            fs::write(root.join(format!("f{index:04}.txt")), b"v2\n").unwrap();
        }
        let tree = snapshot_tree(&root).unwrap().unwrap();
        assert_eq!(tree.files.len(), count);
        assert_eq!(tree.dirty.len(), count);
        let sample = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["hash-object", "f0007.txt"])
            .output()
            .unwrap();
        let blob = String::from_utf8_lossy(&sample.stdout).trim().to_owned();
        assert_eq!(
            tree.files.get("f0007.txt").and_then(Value::as_str),
            Some(blob.as_str())
        );
        let _ = fs::remove_dir_all(&root);
    }
}
