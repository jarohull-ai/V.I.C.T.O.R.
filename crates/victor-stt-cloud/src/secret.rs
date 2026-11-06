//! Klucz API, który nie wycieka przez `Debug`, `Display` ani serializację.
//!
//! Świadomie nie implementujemy `Serialize`, `Display` ani `Deref`. Jedyny
//! dostęp to `expose()` wewnątrz crate'a, przy budowie nagłówka HTTP.

use std::fmt;

use victor_integrations::IntegrationError;

const MAX_API_KEY_LEN: usize = 512;

#[derive(Clone, PartialEq, Eq)]
pub struct ApiKey(String);

impl ApiKey {
    pub fn new(raw: impl Into<String>) -> Result<Self, IntegrationError> {
        let raw = raw.into();
        if raw.is_empty() {
            return Err(IntegrationError::NotConfigured(
                "klucz API jest pusty".into(),
            ));
        }
        if raw.len() > MAX_API_KEY_LEN {
            return Err(IntegrationError::NotConfigured(
                "klucz API jest zbyt długi".into(),
            ));
        }
        if raw.trim() != raw || !raw.bytes().all(|byte| (0x21..=0x7e).contains(&byte)) {
            return Err(IntegrationError::NotConfigured(
                "klucz API musi być widocznym ASCII bez białych znaków".into(),
            ));
        }
        Ok(Self(raw))
    }

    #[cfg(any(feature = "openai", feature = "deepgram"))]
    pub(crate) fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(***)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_empty_or_dirty_keys() {
        assert!(ApiKey::new("").is_err());
        assert!(ApiKey::new("   ").is_err());
        assert!(ApiKey::new("sk-test\nInjected: 1").is_err());
        assert!(ApiKey::new("sk-zażółć").is_err());
        assert!(ApiKey::new(format!("sk-{}", "x".repeat(600))).is_err());
    }

    #[test]
    fn debug_never_contains_the_key() {
        let key = ApiKey::new("sk-super-secret").unwrap();
        let rendered = format!("{key:?}");
        assert_eq!(rendered, "ApiKey(***)");
        assert!(!rendered.contains("super-secret"));
    }
}
