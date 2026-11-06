use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Zakres ścieżek, do którego narzędzie prosi o dostęp.
///
/// `root` jest deklaracją *intencji* narzędzia. Policy engine i tak sprawdza,
/// czy ten root leży w katalogach dozwolonych przez użytkownika.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathScope {
    pub root: PathBuf,
    pub include_subdirectories: bool,
}

impl PathScope {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            include_subdirectories: true,
        }
    }

    pub fn single(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            include_subdirectories: false,
        }
    }
}

/// Minimalne prawo wymagane do wykonania narzędzia.
///
/// Zgodnie z §9 projektu żadne narzędzie nie dziedziczy dowolnego dostępu
/// tylko dlatego, że aplikacja działa jako użytkownik.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Capability {
    FilesystemRead { scope: PathScope },
    FilesystemWrite { scope: PathScope },
    Network { provider: String },
    ProcessLaunch { allowlist: Vec<String> },
}

impl Capability {
    pub fn kind(&self) -> &'static str {
        match self {
            Capability::FilesystemRead { .. } => "filesystem_read",
            Capability::FilesystemWrite { .. } => "filesystem_write",
            Capability::Network { .. } => "network",
            Capability::ProcessLaunch { .. } => "process_launch",
        }
    }

    pub fn filesystem_scope(&self) -> Option<&PathScope> {
        match self {
            Capability::FilesystemRead { scope } | Capability::FilesystemWrite { scope } => {
                Some(scope)
            }
            _ => None,
        }
    }
}

/// Poziomy ryzyka z §9. Kolejność wariantów jest istotna: `Ord` rośnie z ryzykiem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum RiskLevel {
    R0,
    R1,
    R2,
    R3,
}

impl RiskLevel {
    pub const ALL: [RiskLevel; 4] = [RiskLevel::R0, RiskLevel::R1, RiskLevel::R2, RiskLevel::R3];

    /// Etykieta zawsze towarzyszy kolorowi — wymóg dostępności z §8.
    pub fn label(&self) -> &'static str {
        match self {
            RiskLevel::R0 => "R0 — odczyt",
            RiskLevel::R1 => "R1 — zmiana odwracalna",
            RiskLevel::R2 => "R2 — zmiana zewnętrzna",
            RiskLevel::R3 => "R3 — destrukcyjna / systemowa",
        }
    }

    pub fn explanation(&self) -> &'static str {
        match self {
            RiskLevel::R0 => "Tylko odczyt danych; nic nie jest modyfikowane.",
            RiskLevel::R1 => {
                "Zmiana lokalna, którą można cofnąć z dziennika operacji (staging / trash)."
            }
            RiskLevel::R2 => {
                "Zmiana widoczna poza tym komputerem, np. wysłanie pliku do usługi zewnętrznej."
            }
            RiskLevel::R3 => "Trwałe usunięcie, instalacja lub zmiana systemu. Wyłączone w MVP.",
        }
    }

    /// R1 i wyżej wymagają jawnej zgody użytkownika.
    pub fn requires_approval(&self) -> bool {
        !matches!(self, RiskLevel::R0)
    }

    /// R3 jest poza domyślnym, ostrożnym profilem MVP — policy engine musi
    /// odmówić, dopóki użytkownik świadomie nie włączy działań krytycznych.
    pub fn allowed_in_mvp(&self) -> bool {
        !matches!(self, RiskLevel::R3)
    }

    /// Działanie krytyczne: trwałe usunięcie, zmiana systemu, instalacja
    /// nieznanego oprogramowania, wysyłka danych poza komputer.
    ///
    /// Misja V.I.C.T.O.R. nie zabrania takich działań — wymaga, żeby były
    /// dostępne wyłącznie po osobnej, mocnej zgodzie.
    pub fn is_critical(&self) -> bool {
        matches!(self, RiskLevel::R3)
    }

    /// Czy po wykonaniu operacji musi istnieć wpis do cofnięcia.
    pub fn requires_undo_record(&self) -> bool {
        matches!(self, RiskLevel::R1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn risk_ordering_matches_severity() {
        assert!(RiskLevel::R0 < RiskLevel::R1);
        assert!(RiskLevel::R1 < RiskLevel::R2);
        assert!(RiskLevel::R2 < RiskLevel::R3);
    }

    #[test]
    fn only_r0_skips_approval() {
        for level in RiskLevel::ALL {
            assert_eq!(level.requires_approval(), level != RiskLevel::R0);
        }
    }

    #[test]
    fn r3_is_disabled_in_mvp() {
        assert!(!RiskLevel::R3.allowed_in_mvp());
        assert!(RiskLevel::R2.allowed_in_mvp());
    }

    #[test]
    fn risk_label_is_text_not_only_colour() {
        for level in RiskLevel::ALL {
            assert!(level.label().starts_with(match level {
                RiskLevel::R0 => "R0",
                RiskLevel::R1 => "R1",
                RiskLevel::R2 => "R2",
                RiskLevel::R3 => "R3",
            }));
            assert!(!level.explanation().is_empty());
        }
    }
}
