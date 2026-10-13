//! Keeps an agent's file access inside its working folder.

use std::path::{Component, Path, PathBuf};

/// Resolves `requested` against `root` (which must already be canonical) and
/// returns the path to use, or an error if it would leave `root`.
///
/// Refuses `..` components, absolute paths outside `root`, and symlinks
/// (anywhere along the path) that resolve outside the repo. The target itself
/// may not exist yet (for writes); its nearest existing ancestor is checked.
pub fn resolve_in_root(root: &Path, requested: &Path) -> Result<PathBuf, String> {
    if requested
        .components()
        .any(|c| matches!(c, Component::ParentDir))
    {
        return Err(format!("refused: `..` in {}", requested.display()));
    }
    let joined = if requested.is_absolute() {
        requested.to_path_buf()
    } else {
        root.join(requested)
    };

    // Walk up to the nearest ancestor that exists and canonicalize it; this
    // follows any symlinks on the way.
    let mut existing = joined.as_path();
    let mut rest: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if existing.symlink_metadata().is_ok() {
            break;
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name);
                existing = parent;
            }
            _ => {
                return Err(format!(
                    "refused: no existing ancestor for {}",
                    joined.display()
                ));
            }
        }
    }
    let canonical = existing
        .canonicalize()
        .map_err(|e| format!("refused: cannot resolve {}: {e}", existing.display()))?;
    if !canonical.starts_with(root) {
        return Err(format!(
            "refused: {} resolves to {}, outside {}",
            requested.display(),
            canonical.display(),
            root.display()
        ));
    }
    let mut resolved = canonical;
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let base = tempfile::tempdir().unwrap();
        let root = base.path().join("repo");
        let outside = base.path().join("outside");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::write(root.join("README.md"), "hi").unwrap();
        fs::write(outside.join("secret.txt"), "no").unwrap();
        let root = root.canonicalize().unwrap();
        let outside = outside.canonicalize().unwrap();
        (base, root, outside)
    }

    #[test]
    fn allows_paths_inside() {
        let (_b, root, _o) = setup();
        assert_eq!(
            resolve_in_root(&root, &root.join("README.md")).unwrap(),
            root.join("README.md")
        );
        assert_eq!(
            resolve_in_root(&root, Path::new("sub/new/NOTES.md")).unwrap(),
            root.join("sub/new/NOTES.md")
        );
    }

    #[test]
    fn refuses_parent_dir() {
        let (_b, root, _o) = setup();
        assert!(resolve_in_root(&root, Path::new("../outside/secret.txt")).is_err());
        assert!(resolve_in_root(&root, &root.join("sub/../../outside/secret.txt")).is_err());
        // `..` that would stay inside is refused too; agents send absolute paths.
        assert!(resolve_in_root(&root, &root.join("sub/../README.md")).is_err());
    }

    #[test]
    fn refuses_absolute_outside() {
        let (_b, root, outside) = setup();
        assert!(resolve_in_root(&root, &outside.join("secret.txt")).is_err());
        assert!(resolve_in_root(&root, Path::new("/etc/passwd")).is_err());
        assert!(resolve_in_root(&root, &outside.join("new.txt")).is_err());
    }

    #[test]
    fn refuses_symlinks_pointing_out() {
        let (_b, root, outside) = setup();
        std::os::unix::fs::symlink(outside.join("secret.txt"), root.join("link.txt")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("linkdir")).unwrap();
        // Dangling link to a file that doesn't exist yet outside the repo.
        std::os::unix::fs::symlink(outside.join("later.txt"), root.join("dangling.txt")).unwrap();
        assert!(resolve_in_root(&root, &root.join("link.txt")).is_err());
        assert!(resolve_in_root(&root, &root.join("linkdir/secret.txt")).is_err());
        assert!(resolve_in_root(&root, &root.join("linkdir/new.txt")).is_err());
        assert!(resolve_in_root(&root, &root.join("dangling.txt")).is_err());
    }

    #[test]
    fn allows_symlinks_inside() {
        let (_b, root, _o) = setup();
        std::os::unix::fs::symlink(root.join("README.md"), root.join("alias.md")).unwrap();
        assert_eq!(
            resolve_in_root(&root, &root.join("alias.md")).unwrap(),
            root.join("README.md")
        );
    }
}
