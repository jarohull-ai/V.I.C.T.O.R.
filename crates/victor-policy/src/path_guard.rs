use std::path::{Component, Path, PathBuf};

use victor_domain::PathScope;

use crate::error::PolicyError;

/// Komponenty ścieżki, których nie wolno skanować ani modyfikować domyślnie.
/// Rozszerzenie tej listy może wyłącznie użytkownik w ustawieniach — i musi
/// to być widoczne w UI (§9).
pub const DEFAULT_DENIED_COMPONENTS: &[&str] = &[
    ".ssh",
    ".gnupg",
    ".pki",
    ".password-store",
    ".aws",
    ".azure",
    ".kube",
    ".docker",
    ".mozilla",
    ".thunderbird",
    "keyrings",
    "cookies",
];

/// Prefiksy systemowe pomijane przy skanowaniu (§9).
pub const DEFAULT_DENIED_PREFIXES: &[&str] = &["/proc", "/sys", "/dev"];

/// Strażnik ścieżek. Trzyma zarówno wersję dosłowną korzeni (do wykrycia
/// ucieczki przez symlink), jak i wersję kanoniczną (do porównań).
#[derive(Debug, Clone)]
pub struct PathGuard {
    lexical_roots: Vec<PathBuf>,
    canonical_roots: Vec<PathBuf>,
    denied_components: Vec<String>,
    denied_prefixes: Vec<PathBuf>,
}

impl PathGuard {
    pub fn new(allowed_roots: impl IntoIterator<Item = PathBuf>) -> Result<Self, PolicyError> {
        let mut lexical_roots = Vec::new();
        let mut canonical_roots = Vec::new();

        for root in allowed_roots {
            if !root.is_absolute() {
                return Err(PolicyError::RelativePath(root));
            }
            let canonical =
                std::fs::canonicalize(&root).map_err(|_| PolicyError::MissingPath(root.clone()))?;
            lexical_roots.push(root);
            canonical_roots.push(canonical);
        }

        if canonical_roots.is_empty() {
            return Err(PolicyError::EmptyAllowedRoots);
        }

        Ok(Self {
            lexical_roots,
            canonical_roots,
            denied_components: DEFAULT_DENIED_COMPONENTS
                .iter()
                .map(|value| (*value).to_string())
                .collect(),
            denied_prefixes: DEFAULT_DENIED_PREFIXES.iter().map(PathBuf::from).collect(),
        })
    }

    pub fn allowed_roots(&self) -> &[PathBuf] {
        &self.canonical_roots
    }

    /// Dodatkowe zabronione komponenty (np. z ustawień użytkownika).
    pub fn with_denied_components(mut self, components: impl IntoIterator<Item = String>) -> Self {
        self.denied_components.extend(components);
        self
    }

    /// Waliduje ścieżkę i zwraca jej kanoniczną postać.
    pub fn validate(&self, raw: &Path) -> Result<PathBuf, PolicyError> {
        if !raw.is_absolute() {
            return Err(PolicyError::RelativePath(raw.to_path_buf()));
        }

        for component in raw.components() {
            match component {
                Component::ParentDir => {
                    return Err(PolicyError::ParentTraversal(raw.to_path_buf()))
                }
                Component::Normal(name) => {
                    if let Some(name) = name.to_str() {
                        if self.denied_components.iter().any(|d| d == name) {
                            return Err(PolicyError::DeniedComponent {
                                path: raw.to_path_buf(),
                                component: name.to_string(),
                            });
                        }
                    }
                }
                _ => {}
            }
        }

        let canonical = Self::canonicalize_target(raw)?;

        for component in canonical.components() {
            if let Component::Normal(name) = component {
                if let Some(name) = name.to_str() {
                    if self.denied_components.iter().any(|d| d == name) {
                        return Err(PolicyError::DeniedComponent {
                            path: canonical.clone(),
                            component: name.to_string(),
                        });
                    }
                }
            }
        }

        if let Some(prefix) = self
            .denied_prefixes
            .iter()
            .find(|prefix| canonical.starts_with(prefix))
        {
            return Err(PolicyError::DeniedPrefix(prefix.clone()));
        }

        if !self
            .canonical_roots
            .iter()
            .any(|root| canonical.starts_with(root))
        {
            // Ścieżka wyglądała na dozwoloną dosłownie, ale po rozwiązaniu
            // symlinków wyszła poza zakres — to jest próba ucieczki, a nie
            // zwykłe podanie katalogu spoza listy.
            if self.lexical_roots.iter().any(|root| raw.starts_with(root)) {
                return Err(PolicyError::SymlinkEscape {
                    path: raw.to_path_buf(),
                    target: canonical,
                });
            }
            return Err(PolicyError::OutsideAllowedRoot(canonical));
        }

        Ok(canonical)
    }

    /// Sprawdza, czy ścieżka mieści się w zakresie zadeklarowanym przez narzędzie.
    pub fn validate_in_scope(&self, raw: &Path, scope: &PathScope) -> Result<PathBuf, PolicyError> {
        let canonical = self.validate(raw)?;
        let scope_root = self.validate(&scope.root)?;

        let inside = if scope.include_subdirectories {
            canonical.starts_with(&scope_root)
        } else {
            canonical.parent() == Some(scope_root.as_path())
        };

        if !inside {
            return Err(PolicyError::OutsideToolScope {
                path: canonical,
                scope: scope_root,
            });
        }

        Ok(canonical)
    }

    fn canonicalize_target(raw: &Path) -> Result<PathBuf, PolicyError> {
        if raw.exists() {
            return std::fs::canonicalize(raw).map_err(|_| PolicyError::MissingPath(raw.into()));
        }

        let parent = raw
            .parent()
            .ok_or_else(|| PolicyError::MissingPath(raw.into()))?;
        if !parent.exists() {
            return Err(PolicyError::MissingPath(raw.into()));
        }
        let canonical_parent =
            std::fs::canonicalize(parent).map_err(|_| PolicyError::MissingPath(raw.into()))?;
        let name = raw
            .file_name()
            .ok_or_else(|| PolicyError::MissingPath(raw.into()))?;

        Ok(canonical_parent.join(name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn guard(root: &Path) -> PathGuard {
        PathGuard::new([root.to_path_buf()]).unwrap()
    }

    #[test]
    fn accepts_path_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"x").unwrap();
        let resolved = guard(dir.path()).validate(&file).unwrap();
        assert_eq!(resolved, std::fs::canonicalize(&file).unwrap());
    }

    #[test]
    fn accepts_not_yet_existing_file_under_existing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nowy.txt");
        let resolved = guard(dir.path()).validate(&target).unwrap();
        assert_eq!(resolved.file_name().unwrap(), "nowy.txt");
    }

    #[test]
    fn rejects_relative_path() {
        let dir = tempfile::tempdir().unwrap();
        let err = guard(dir.path())
            .validate(Path::new("względna/ścieżka"))
            .unwrap_err();
        assert!(matches!(err, PolicyError::RelativePath(_)));
    }

    #[test]
    fn rejects_parent_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let evil = dir.path().join("..").join("etc");
        let err = guard(dir.path()).validate(&evil).unwrap_err();
        assert!(matches!(err, PolicyError::ParentTraversal(_)));
    }

    #[test]
    fn rejects_path_outside_root() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let err = guard(dir.path()).validate(outside.path()).unwrap_err();
        assert!(
            matches!(err, PolicyError::OutsideAllowedRoot(_)),
            "otrzymano {err:?}"
        );
    }

    #[test]
    fn rejects_symlink_pointing_outside_root() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("sekret.txt");
        std::fs::write(&secret, b"tajne").unwrap();
        let link = root.path().join("link");
        symlink(&secret, &link).unwrap();

        let err = guard(root.path()).validate(&link).unwrap_err();
        assert!(
            matches!(err, PolicyError::SymlinkEscape { .. }),
            "otrzymano {err:?}"
        );
    }

    #[test]
    fn rejects_symlinked_directory_escape() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let link_dir = root.path().join("katalog");
        symlink(outside.path(), &link_dir).unwrap();

        let err = guard(root.path())
            .validate(&link_dir.join("x.txt"))
            .unwrap_err();
        assert!(
            matches!(err, PolicyError::SymlinkEscape { .. }),
            "otrzymano {err:?}"
        );
    }

    #[test]
    fn accepts_symlink_that_stays_inside_root() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = root.path().join("link");
        symlink(&real, &link).unwrap();

        assert!(guard(root.path()).validate(&link).is_ok());
    }

    #[test]
    fn rejects_secret_components() {
        let dir = tempfile::tempdir().unwrap();
        for component in [".ssh", ".gnupg", "keyrings"] {
            let target = dir.path().join(component).join("x");
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            let err = guard(dir.path()).validate(&target).unwrap_err();
            assert!(
                matches!(err, PolicyError::DeniedComponent { .. }),
                "{component}: otrzymano {err:?}"
            );
        }
    }

    #[test]
    fn rejects_denied_system_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let err = guard(dir.path())
            .validate(Path::new("/proc/self/environ"))
            .unwrap_err();
        assert!(
            matches!(
                err,
                PolicyError::DeniedPrefix(_) | PolicyError::OutsideAllowedRoot(_)
            ),
            "otrzymano {err:?}"
        );
    }

    #[test]
    fn rejects_missing_parent() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("nie").join("ma").join("pliku.txt");
        let err = guard(dir.path()).validate(&target).unwrap_err();
        assert!(
            matches!(err, PolicyError::MissingPath(_)),
            "otrzymano {err:?}"
        );
    }

    #[test]
    fn empty_root_list_is_rejected() {
        let err = PathGuard::new(Vec::<PathBuf>::new()).unwrap_err();
        assert_eq!(err, PolicyError::EmptyAllowedRoots);
    }

    #[test]
    fn non_directory_scope_limits_to_direct_children() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), b"x").unwrap();
        std::fs::write(dir.path().join("sub").join("b.txt"), b"x").unwrap();

        let guard = guard(dir.path());
        let scope = PathScope::single(dir.path());
        assert!(guard
            .validate_in_scope(&dir.path().join("a.txt"), &scope)
            .is_ok());
        let err = guard
            .validate_in_scope(&dir.path().join("sub").join("b.txt"), &scope)
            .unwrap_err();
        assert!(matches!(err, PolicyError::OutsideToolScope { .. }));
    }
}
