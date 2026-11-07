//! Prosty, lokalny cache odpowiedzi. Klucz to SHA-256 znormalizowanego URL,
//! wartość to już wyekstrahowany dokument (nie surowy HTML).
//!
//! Cache jest best effort: błędy odczytu/zapisu nie przerywają fetchu. Pliki
//! mają uprawnienia `0600` na Unixie, bo mogą zawierać wrażliwe fragmenty źródeł.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::FetchedDocument;

#[derive(Serialize, Deserialize)]
struct CacheEntry {
    stored_at: u64,
    document: FetchedDocument,
}

pub struct FetchCache {
    dir: PathBuf,
    ttl: Duration,
}

impl FetchCache {
    pub fn new(dir: impl AsRef<Path>, ttl: Duration) -> io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        set_private_dir(&dir)?;
        Ok(Self { dir, ttl })
    }

    pub fn directory(&self) -> &Path {
        &self.dir
    }

    pub(crate) fn get(&self, url: &Url) -> Option<FetchedDocument> {
        let path = self.path(url);
        let bytes = fs::read(path).ok()?;
        let entry: CacheEntry = serde_json::from_slice(&bytes).ok()?;
        let now = now_secs();
        if now.saturating_sub(entry.stored_at) >= self.ttl.as_secs() {
            return None;
        }
        Some(entry.document)
    }

    pub(crate) fn put(&self, url: &Url, document: &FetchedDocument) -> io::Result<()> {
        let entry = CacheEntry {
            stored_at: now_secs(),
            document: document.clone(),
        };
        let bytes = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        let path = self.path(url);
        let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), now_nanos()));

        fs::write(&tmp, bytes)?;
        set_private_permissions(&tmp)?;
        match fs::rename(&tmp, &path) {
            Ok(()) => Ok(()),
            Err(error) => {
                let _ = fs::remove_file(&tmp);
                Err(error)
            }
        }
    }

    fn path(&self, url: &Url) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(url.as_str().as_bytes());
        let key = hex::encode(hasher.finalize());
        self.dir.join(format!("{key}.json"))
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
}

fn set_private_dir(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn set_private_permissions(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> FetchedDocument {
        FetchedDocument {
            requested_url: "https://example.com/".into(),
            final_url: "https://example.com/".into(),
            canonical_url: "https://example.com/".into(),
            status: 200,
            content_type: "text/html".into(),
            title: "Tytuł".into(),
            description: "Opis".into(),
            text: "Treść".into(),
            lang: Some("pl".into()),
            published: None,
            content_hash: "sha256:aaa".into(),
            html_hash: "sha256:bbb".into(),
            fetched_at: "2026-01-01T00:00:00.000Z".into(),
            noindex: false,
            cache_hit: false,
        }
    }

    #[test]
    fn roundtrip_and_ttl() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(dir.path(), Duration::from_secs(60)).unwrap();
        let url = Url::parse("https://example.com/").unwrap();

        assert!(cache.get(&url).is_none());
        cache.put(&url, &doc()).unwrap();
        let loaded = cache.get(&url).unwrap();
        assert_eq!(loaded.title, "Tytuł");
        assert!(!loaded.cache_hit);

        let expired = FetchCache::new(dir.path(), Duration::from_secs(0)).unwrap();
        assert!(expired.get(&url).is_none());
    }

    #[test]
    fn cache_key_is_stable() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(dir.path(), Duration::from_secs(60)).unwrap();
        let a = Url::parse("https://example.com/a").unwrap();
        let b = Url::parse("https://example.com/b").unwrap();
        assert_ne!(cache.path(&a), cache.path(&b));
        assert_eq!(cache.path(&a), cache.path(&a));
    }

    #[cfg(unix)]
    #[test]
    fn cache_dir_and_file_are_private() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let cache = FetchCache::new(dir.path(), Duration::from_secs(60)).unwrap();
        let url = Url::parse("https://example.com/").unwrap();
        cache.put(&url, &doc()).unwrap();
        let path = cache.path(&url);
        let file_mode = fs::metadata(path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
    }
}
