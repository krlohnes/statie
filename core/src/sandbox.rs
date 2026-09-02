//! Sandboxed filesystem paths (A9): tools only ever see a `SandboxPath`,
//! never a bare `Path`, and every `SandboxPath` is constructed by joining
//! and canonicalizing against a `SandboxRoot`.

use std::path::{Component, Path, PathBuf};

use crate::error::SandboxError;

/// A canonicalized sandbox root directory.
#[derive(Clone)]
pub struct SandboxRoot(PathBuf);

impl SandboxRoot {
    pub fn new(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let canon = path.as_ref().canonicalize()?;
        Ok(SandboxRoot(canon))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Joins `rel` onto the root. Rejects absolute paths and any `..`
    /// component outright. Resolves the deepest existing ancestor with
    /// `canonicalize` and requires the result to start with the canonical
    /// root — this catches symlinks that would otherwise escape.
    pub fn join(&self, rel: &str) -> Result<SandboxPath, SandboxError> {
        let rel_path = Path::new(rel);

        if rel_path.is_absolute() {
            return Err(SandboxError::AbsolutePath(rel.to_string()));
        }
        for component in rel_path.components() {
            match component {
                Component::ParentDir => {
                    return Err(SandboxError::ParentEscape(rel.to_string()));
                }
                Component::Prefix(_) | Component::RootDir => {
                    return Err(SandboxError::AbsolutePath(rel.to_string()));
                }
                _ => {}
            }
        }

        let candidate = self.0.join(rel_path);

        // Find the deepest existing ancestor of `candidate` (including
        // itself) and canonicalize that; then re-append the remaining,
        // not-yet-existing suffix.
        let mut existing = candidate.clone();
        let mut suffix: Vec<std::ffi::OsString> = Vec::new();
        loop {
            if existing.exists() {
                break;
            }
            match existing.file_name() {
                Some(name) => {
                    suffix.push(name.to_os_string());
                    existing = match existing.parent() {
                        Some(p) => p.to_path_buf(),
                        None => break,
                    };
                }
                None => break,
            }
        }

        let canon_existing = existing
            .canonicalize()
            .map_err(|e| SandboxError::Io(e.to_string()))?;

        if !canon_existing.starts_with(&self.0) {
            return Err(SandboxError::OutsideRoot(rel.to_string()));
        }

        let mut resolved = canon_existing;
        for part in suffix.into_iter().rev() {
            resolved.push(part);
        }

        if !resolved.starts_with(&self.0) {
            return Err(SandboxError::OutsideRoot(rel.to_string()));
        }

        Ok(SandboxPath(resolved))
    }
}

/// A path known to resolve inside a `SandboxRoot`. No public constructor.
#[derive(Debug)]
pub struct SandboxPath(PathBuf);

impl SandboxPath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn relative(&self, root: &SandboxRoot) -> PathBuf {
        self.0
            .strip_prefix(root.path())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| self.0.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();
        let err = root.join("../escape.txt").unwrap_err();
        assert!(matches!(err, SandboxError::ParentEscape(_)));
    }

    #[test]
    fn rejects_nested_parent_dir_escape() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();
        let err = root.join("sub/../../escape.txt").unwrap_err();
        assert!(matches!(err, SandboxError::ParentEscape(_)));
    }

    #[test]
    fn rejects_absolute_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();
        let err = root.join("/etc/passwd").unwrap_err();
        assert!(matches!(err, SandboxError::AbsolutePath(_)));
    }

    #[test]
    fn rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret.txt"), b"secret").unwrap();

        let link_path = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), &link_path).unwrap();

        let root = SandboxRoot::new(dir.path()).unwrap();
        let err = root.join("link/secret.txt").unwrap_err();
        assert!(matches!(err, SandboxError::OutsideRoot(_)));
    }

    #[test]
    fn allows_not_yet_existing_file_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();
        let path = root.join("new_file.txt").unwrap();
        assert!(path.as_path().starts_with(root.path()));
        assert_eq!(path.as_path().file_name().unwrap(), "new_file.txt");
    }

    #[test]
    fn allows_not_yet_existing_nested_file_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = SandboxRoot::new(dir.path()).unwrap();
        let path = root.join("nested/dir/new_file.txt").unwrap();
        assert!(path.as_path().starts_with(root.path()));
    }
}
