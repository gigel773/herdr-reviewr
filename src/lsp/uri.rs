//! File URIs to paths and back, and whether a server's path lies in the worktree.

use std::path::{Path, PathBuf};

use url::Url;

/// A path's `file:` URI; `None` for a relative path.
pub fn from_path(path: &Path) -> Option<String> {
    Url::from_file_path(path).ok().map(String::from)
}

/// A `file:` URI's path; any other scheme, or a host on unix, is no file of ours.
pub fn to_path(uri: &str) -> Option<PathBuf> {
    let url = Url::parse(uri).ok()?;
    (url.scheme() == "file").then(|| url.to_file_path().ok()).flatten()
}

/// Where a server's path lies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    /// In the worktree, as the repo-relative `/`-separated path the navigator uses.
    Repo(String),
    /// Anywhere else, or nowhere: shown, never opened in the pane.
    External(PathBuf),
}

/// Classify `path` by its own spelling, once its resolved target proves it stays in `repo`.
pub fn locate(repo: &Path, path: &Path) -> Located {
    let external = || Located::External(path.to_path_buf());
    let (Ok(real_repo), Ok(real)) = (std::fs::canonicalize(repo), std::fs::canonicalize(path))
    else {
        return external();
    };
    // A link inside may point out: the target decides containment.
    if !real.starts_with(&real_repo) {
        return external();
    }
    // The spelling names the path (an ignored link stays itself), if it reaches the same file.
    let spelled = lexical(path);
    if std::fs::canonicalize(&spelled).ok().as_ref() != Some(&real) {
        return external();
    }
    let inside = spelled.strip_prefix(repo).or_else(|_| spelled.strip_prefix(&real_repo));
    match inside.ok().and_then(|rel| rel.iter().map(|p| p.to_str()).collect::<Option<Vec<_>>>()) {
        Some(parts) if !parts.is_empty() => Located::Repo(parts.join("/")),
        _ => external(),
    }
}

/// `path` with `.` and `..` folded away, links left alone.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{Located, from_path, locate, to_path};

    #[test]
    fn a_path_survives_the_trip_through_its_uri() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a dir").join("ünï #1.cpp");
        let uri = from_path(&path).unwrap();
        assert!(uri.starts_with("file:///"), "{uri}");
        assert!(uri.contains("%20") && !uri.contains(' '), "escaped: {uri}");
        assert_eq!(to_path(&uri), Some(path));
        assert_eq!(from_path(std::path::Path::new("relative.cpp")), None);
    }

    #[test]
    fn only_a_file_uri_names_a_path() {
        assert_eq!(to_path("https://example.com/a.cpp"), None);
        assert_eq!(to_path("jdt://contents/a.class"), None);
        assert_eq!(to_path("not a uri"), None);
    }

    #[test]
    fn a_worktree_file_is_repo_relative_and_the_rest_is_external() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/a.cpp"), "").unwrap();
        let inside = repo.path().join("src/a.cpp");
        assert_eq!(locate(repo.path(), &inside), Located::Repo("src/a.cpp".into()));
        // `..` resolves before the comparison.
        let dotted = repo.path().join("src/../src/a.cpp");
        assert_eq!(locate(repo.path(), &dotted), Located::Repo("src/a.cpp".into()));
        let elsewhere = tempfile::tempdir().unwrap();
        let far = elsewhere.path().join("far.h");
        std::fs::write(&far, "").unwrap();
        assert_eq!(locate(repo.path(), &far), Located::External(far.clone()));
        // A file that is gone cannot be opened, so it is never a worktree place.
        let gone = repo.path().join("gone.cpp");
        assert_eq!(locate(repo.path(), &gone), Located::External(gone.clone()));
        // The repo itself is no file.
        assert_eq!(locate(repo.path(), repo.path()), Located::External(repo.path().into()));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_out_of_the_worktree_is_external() {
        let repo = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::write(elsewhere.path().join("far.h"), "").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), repo.path().join("vendor")).unwrap();
        let through = repo.path().join("vendor/far.h");
        assert_eq!(locate(repo.path(), &through), Located::External(through.clone()));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_inside_the_worktree_keeps_its_own_path() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join("src")).unwrap();
        std::fs::write(repo.path().join("src/real.cpp"), "").unwrap();
        std::os::unix::fs::symlink("src/real.cpp", repo.path().join("alias.cpp")).unwrap();
        let alias = repo.path().join("alias.cpp");
        assert_eq!(locate(repo.path(), &alias), Located::Repo("alias.cpp".into()));
        // `alias/..` is `src` on disk but the root by spelling: no path is claimed.
        std::fs::create_dir(repo.path().join("src/deep")).unwrap();
        std::fs::write(repo.path().join("target.cpp"), "root").unwrap();
        std::fs::write(repo.path().join("src/target.cpp"), "src").unwrap();
        std::os::unix::fs::symlink("src/deep", repo.path().join("deep")).unwrap();
        let mixed = repo.path().join("deep/../target.cpp");
        assert_eq!(locate(repo.path(), &mixed), Located::External(mixed.clone()));
    }
}
