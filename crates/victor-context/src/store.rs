use std::collections::HashMap;
use std::sync::RwLock;

use victor_domain::UnixMs;

use crate::error::ContextError;
use crate::item::{ContextItem, ContextItemId};
use crate::scope::ContextScope;

/// Klucz jest złożeniem scope i identyfikatora. Dzięki temu dwa projekty mogą
/// mieć element o tym samym `id` i nie ma potrzeby globalnej unikalności.
type StoreKey = (ContextScope, ContextItemId);

/// Magazyn kontekstu. Implementacja trwała (SQLite) musi zachować te same
/// gwarancje: `get` i `delete` działają wyłącznie w podanym scope.
pub trait ContextStore: Send + Sync {
    fn put(&self, item: ContextItem) -> Result<(), ContextError>;

    fn get(
        &self,
        id: &ContextItemId,
        scope: &ContextScope,
    ) -> Result<Option<ContextItem>, ContextError>;

    /// Usuwa tylko w obrębie podanego scope. Element z innego scope jest
    /// nieosiągalny — `get`/`delete` zachowują się tak, jakby nie istniał.
    fn delete(&self, id: &ContextItemId, scope: &ContextScope) -> Result<bool, ContextError>;

    fn list(&self, scope: &ContextScope) -> Result<Vec<ContextItem>, ContextError>;

    fn purge_expired(&self, now: UnixMs) -> Result<usize, ContextError>;
}

/// Magazyn w pamięci — do testów, prototypu i working memory. Nie jest trwały.
#[derive(Debug, Default)]
pub struct InMemoryContextStore {
    items: RwLock<HashMap<StoreKey, ContextItem>>,
}

impl InMemoryContextStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.read().map(|items| items.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn read(
        &self,
    ) -> Result<std::sync::RwLockReadGuard<'_, HashMap<StoreKey, ContextItem>>, ContextError> {
        self.items
            .read()
            .map_err(|_| ContextError::Store("blokada odczytu zatruta".into()))
    }

    fn write(
        &self,
    ) -> Result<std::sync::RwLockWriteGuard<'_, HashMap<StoreKey, ContextItem>>, ContextError> {
        self.items
            .write()
            .map_err(|_| ContextError::Store("blokada zapisu zatruta".into()))
    }
}

impl ContextStore for InMemoryContextStore {
    fn put(&self, item: ContextItem) -> Result<(), ContextError> {
        let mut items = self.write()?;
        items.insert((item.scope.clone(), item.id.clone()), item);
        Ok(())
    }

    fn get(
        &self,
        id: &ContextItemId,
        scope: &ContextScope,
    ) -> Result<Option<ContextItem>, ContextError> {
        let items = self.read()?;
        Ok(items.get(&(scope.clone(), id.clone())).cloned())
    }

    fn delete(&self, id: &ContextItemId, scope: &ContextScope) -> Result<bool, ContextError> {
        let mut items = self.write()?;
        Ok(items.remove(&(scope.clone(), id.clone())).is_some())
    }

    fn list(&self, scope: &ContextScope) -> Result<Vec<ContextItem>, ContextError> {
        let items = self.read()?;
        let mut result: Vec<ContextItem> = items
            .values()
            .filter(|item| &item.scope == scope)
            .cloned()
            .collect();
        result.sort_by(|a, b| {
            b.created_at_unix_ms
                .cmp(&a.created_at_unix_ms)
                .then_with(|| a.id.as_str().cmp(b.id.as_str()))
        });
        Ok(result)
    }

    fn purge_expired(&self, now: UnixMs) -> Result<usize, ContextError> {
        let mut items = self.write()?;
        let before = items.len();
        items.retain(|_, item| !item.is_expired(now));
        Ok(before - items.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{
        ContextContent, ContextItemKind, ContextSource, MemoryTier, Origin, Retention,
    };

    fn scope(id: &str) -> ContextScope {
        ContextScope::project(id).unwrap()
    }

    fn make(id: &str, scope: &ContextScope, created_at: UnixMs) -> ContextItem {
        ContextItem::new(
            ContextItemId::parse(id).unwrap(),
            scope.clone(),
            ContextItemKind::Note,
            MemoryTier::Durable,
            ContextContent::inline(format!("treść {id}")),
            ContextSource::new(Origin::User, "test"),
            created_at,
        )
        .unwrap()
    }

    #[test]
    fn put_and_get_roundtrip() {
        let store = InMemoryContextStore::new();
        let project = scope("victor");
        store.put(make("a", &project, 10)).unwrap();

        let fetched = store
            .get(&ContextItemId::parse("a").unwrap(), &project)
            .unwrap()
            .unwrap();
        assert_eq!(fetched.id.as_str(), "a");
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn items_are_invisible_across_scopes() {
        let store = InMemoryContextStore::new();
        let a = scope("projekt-a");
        let b = scope("projekt-b");
        store.put(make("x", &a, 1)).unwrap();

        assert!(store
            .get(&ContextItemId::parse("x").unwrap(), &b)
            .unwrap()
            .is_none());
    }

    #[test]
    fn delete_requires_matching_scope() {
        let store = InMemoryContextStore::new();
        let a = scope("projekt-a");
        let b = scope("projekt-b");
        store.put(make("x", &a, 1)).unwrap();

        assert!(!store
            .delete(&ContextItemId::parse("x").unwrap(), &b)
            .unwrap());
        assert_eq!(store.len(), 1);

        assert!(store
            .delete(&ContextItemId::parse("x").unwrap(), &a)
            .unwrap());
        assert!(store.is_empty());
    }

    #[test]
    fn same_id_can_exist_in_two_scopes() {
        let store = InMemoryContextStore::new();
        let a = scope("projekt-a");
        let b = scope("projekt-b");
        store.put(make("x", &a, 1)).unwrap();
        store.put(make("x", &b, 2)).unwrap();

        assert_eq!(store.len(), 2);
        assert_eq!(store.list(&a).unwrap().len(), 1);
        assert_eq!(store.list(&b).unwrap().len(), 1);
    }

    #[test]
    fn put_updates_within_the_same_scope() {
        let store = InMemoryContextStore::new();
        let a = scope("projekt-a");
        store.put(make("x", &a, 1)).unwrap();
        store
            .put(
                make("x", &a, 2)
                    .with_retention(Retention::TtlFromCreation(10))
                    .unwrap(),
            )
            .unwrap();

        assert_eq!(store.len(), 1);
        let item = store
            .get(&ContextItemId::parse("x").unwrap(), &a)
            .unwrap()
            .unwrap();
        assert_eq!(item.created_at_unix_ms, 2);
    }

    #[test]
    fn list_is_isolated_and_ordered() {
        let store = InMemoryContextStore::new();
        let a = scope("projekt-a");
        let b = scope("projekt-b");
        store.put(make("old", &a, 1)).unwrap();
        store.put(make("new", &a, 5)).unwrap();
        store.put(make("other", &b, 9)).unwrap();

        let listed = store.list(&a).unwrap();
        let ids: Vec<_> = listed.iter().map(|item| item.id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old"]);
    }

    #[test]
    fn purge_removes_only_expired() {
        let store = InMemoryContextStore::new();
        let project = scope("victor");
        let short = make("short", &project, 1_000)
            .with_retention(Retention::TtlFromCreation(100))
            .unwrap();
        let long = make("long", &project, 1_000);
        store.put(short).unwrap();
        store.put(long).unwrap();

        assert_eq!(store.purge_expired(1_099).unwrap(), 0);
        assert_eq!(store.purge_expired(1_100).unwrap(), 1);
        assert_eq!(store.len(), 1);
    }
}
