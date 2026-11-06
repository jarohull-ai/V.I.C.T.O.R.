use std::collections::HashSet;

use victor_domain::UnixMs;

use crate::error::ContextError;
use crate::item::{ContextItem, ContextItemKind, MemoryTier};
use crate::scope::ContextScope;
use crate::store::ContextStore;

pub const DEFAULT_MAX_ITEMS: usize = 32;
pub const DEFAULT_MAX_BYTES: u64 = 64 * 1024;

/// Żądanie widoku. `scopes` jest jawne — o tym, czy zadanie widzi kontekst
/// projektu, decyduje wywołujący (runtime + polityka), nie magazyn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextViewRequest {
    pub scopes: Vec<ContextScope>,
    pub tiers: Option<Vec<MemoryTier>>,
    pub kinds: Option<Vec<ContextItemKind>>,
    pub max_items: usize,
    pub max_bytes: u64,
    pub include_secret: bool,
    pub now_unix_ms: UnixMs,
}

impl ContextViewRequest {
    pub fn new(scopes: Vec<ContextScope>, now_unix_ms: UnixMs) -> Self {
        Self {
            scopes,
            tiers: None,
            kinds: None,
            max_items: DEFAULT_MAX_ITEMS,
            max_bytes: DEFAULT_MAX_BYTES,
            include_secret: false,
            now_unix_ms,
        }
    }

    pub fn with_tiers(mut self, tiers: Vec<MemoryTier>) -> Self {
        self.tiers = Some(tiers);
        self
    }

    pub fn with_kinds(mut self, kinds: Vec<ContextItemKind>) -> Self {
        self.kinds = Some(kinds);
        self
    }

    pub fn with_budget(mut self, max_items: usize, max_bytes: u64) -> Self {
        self.max_items = max_items;
        self.max_bytes = max_bytes;
        self
    }

    pub fn including_secret(mut self, include: bool) -> Self {
        self.include_secret = include;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextView {
    /// Posortowane od najnowszego; przy remisie po `id` rosnąco.
    pub items: Vec<ContextItem>,
    pub total_bytes: u64,
    /// Ile elementów odpadło przez budżet, filtr lub wygaśnięcie.
    pub dropped_items: usize,
}

impl ContextView {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Stabilna reprezentacja do promptu. Ten sam widok zawsze daje ten sam
    /// tekst — bez zależności od kolejności wstawiania do magazynu.
    pub fn render_plain(&self) -> String {
        self.items
            .iter()
            .map(|item| {
                format!(
                    "### {} ({}, {})\n{}",
                    item.id,
                    item.kind.as_str(),
                    item.scope.label(),
                    item.content.render()
                )
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    }
}

/// Buduje ograniczony widok. Deterministyczny: sortowanie i budżet nie
/// zależą od kolejności wstawiania ani od HashMap.
pub fn build_view(
    store: &dyn ContextStore,
    request: &ContextViewRequest,
) -> Result<ContextView, ContextError> {
    if request.max_items == 0 {
        return Err(ContextError::InvalidBudget("max_items".into()));
    }
    if request.max_bytes == 0 {
        return Err(ContextError::InvalidBudget("max_bytes".into()));
    }

    let mut candidates = Vec::new();
    for scope in &request.scopes {
        candidates.extend(store.list(scope)?);
    }

    let total_candidates = candidates.len();
    let mut seen = HashSet::new();
    candidates.retain(|item| seen.insert((item.scope.clone(), item.id.clone())));

    candidates.retain(|item| {
        !item.is_expired(request.now_unix_ms)
            && !(item.sensitivity.is_secret() && !request.include_secret)
            && request
                .tiers
                .as_ref()
                .is_none_or(|tiers| tiers.contains(&item.tier))
            && request
                .kinds
                .as_ref()
                .is_none_or(|kinds| kinds.contains(&item.kind))
    });

    candidates.sort_by(|a, b| {
        b.created_at_unix_ms
            .cmp(&a.created_at_unix_ms)
            .then_with(|| a.id.as_str().cmp(b.id.as_str()))
            .then_with(|| a.scope.label().cmp(&b.scope.label()))
    });

    let mut items = Vec::new();
    let mut total_bytes = 0u64;
    for item in candidates {
        let size = item.size_bytes();
        let fits = items.len() < request.max_items
            && total_bytes.saturating_add(size) <= request.max_bytes;
        if fits {
            total_bytes += size;
            items.push(item);
        }
    }

    let dropped_items = total_candidates.saturating_sub(items.len());
    Ok(ContextView {
        items,
        total_bytes,
        dropped_items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::item::{
        ContextContent, ContextItemId, ContextSource, Origin, Retention, Sensitivity,
    };
    use crate::store::InMemoryContextStore;

    fn project(id: &str) -> ContextScope {
        ContextScope::project(id).unwrap()
    }

    fn make(
        id: &str,
        scope: &ContextScope,
        created_at: UnixMs,
        kind: ContextItemKind,
        tier: MemoryTier,
        text: &str,
    ) -> ContextItem {
        ContextItem::new(
            ContextItemId::parse(id).unwrap(),
            scope.clone(),
            kind,
            tier,
            ContextContent::inline(text),
            ContextSource::new(Origin::User, "test"),
            created_at,
        )
        .unwrap()
    }

    #[test]
    fn view_is_deterministic_regardless_of_insertion_order() {
        let a = project("p");

        let store_a = InMemoryContextStore::new();
        let store_b = InMemoryContextStore::new();
        let items = [
            make(
                "one",
                &a,
                10,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "1",
            ),
            make(
                "two",
                &a,
                20,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "2",
            ),
            make(
                "three",
                &a,
                20,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "3",
            ),
        ];
        for item in &items {
            store_a.put(item.clone()).unwrap();
        }
        for item in items.iter().rev() {
            store_b.put(item.clone()).unwrap();
        }

        let request = ContextViewRequest::new(vec![a], 100);
        let view_a = build_view(&store_a, &request).unwrap();
        let view_b = build_view(&store_b, &request).unwrap();

        let ids_a: Vec<_> = view_a.items.iter().map(|i| i.id.as_str()).collect();
        let ids_b: Vec<_> = view_b.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids_a, vec!["three", "two", "one"]);
        assert_eq!(ids_a, ids_b);
        assert_eq!(view_a.render_plain(), view_b.render_plain());
    }

    #[test]
    fn scope_isolation_is_enforced_by_view() {
        let a = project("a");
        let b = project("b");
        let store = InMemoryContextStore::new();
        store
            .put(make(
                "a1",
                &a,
                1,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "a",
            ))
            .unwrap();
        store
            .put(make(
                "b1",
                &b,
                2,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "b",
            ))
            .unwrap();

        let view = build_view(&store, &ContextViewRequest::new(vec![a], 10)).unwrap();
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].id.as_str(), "a1");
    }

    #[test]
    fn same_id_in_two_scopes_can_both_enter_the_view() {
        let a = project("a");
        let b = project("b");
        let store = InMemoryContextStore::new();
        store
            .put(make(
                "same",
                &a,
                5,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "a",
            ))
            .unwrap();
        store
            .put(make(
                "same",
                &b,
                5,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "b",
            ))
            .unwrap();

        let view = build_view(
            &store,
            &ContextViewRequest::new(vec![a, b], 10).with_budget(10, 1_000),
        )
        .unwrap();
        assert_eq!(view.items.len(), 2);
        let scopes: Vec<_> = view.items.iter().map(|i| i.scope.label()).collect();
        assert_eq!(scopes, vec!["project:a", "project:b"]);
    }

    #[test]
    fn max_items_keeps_newest() {
        let p = project("p");
        let store = InMemoryContextStore::new();
        for (index, id) in ["a", "b", "c"].iter().enumerate() {
            store
                .put(make(
                    id,
                    &p,
                    index as u64 + 1,
                    ContextItemKind::Note,
                    MemoryTier::Working,
                    "x",
                ))
                .unwrap();
        }

        let request = ContextViewRequest::new(vec![p], 100).with_budget(2, 1_000);
        let view = build_view(&store, &request).unwrap();
        assert_eq!(view.dropped_items, 1);
        assert_eq!(view.items[0].id.as_str(), "c");
        assert_eq!(view.items[1].id.as_str(), "b");
    }

    #[test]
    fn byte_budget_skips_oversized_and_keeps_fitting() {
        let p = project("p");
        let store = InMemoryContextStore::new();
        store
            .put(make(
                "big",
                &p,
                3,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "1234567890",
            ))
            .unwrap();
        store
            .put(make(
                "small",
                &p,
                2,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "123",
            ))
            .unwrap();

        let request = ContextViewRequest::new(vec![p], 100).with_budget(10, 5);
        let view = build_view(&store, &request).unwrap();
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].id.as_str(), "small");
        assert_eq!(view.total_bytes, 3);
    }

    #[test]
    fn expired_and_secret_items_are_excluded_by_default() {
        let p = project("p");
        let store = InMemoryContextStore::new();
        store
            .put(make(
                "live",
                &p,
                1,
                ContextItemKind::Note,
                MemoryTier::Durable,
                "ok",
            ))
            .unwrap();
        store
            .put(
                make(
                    "expired",
                    &p,
                    1,
                    ContextItemKind::Note,
                    MemoryTier::Durable,
                    "x",
                )
                .with_retention(Retention::Until(50))
                .unwrap(),
            )
            .unwrap();
        store
            .put(
                make(
                    "secret",
                    &p,
                    2,
                    ContextItemKind::Note,
                    MemoryTier::Durable,
                    "x",
                )
                .with_sensitivity(Sensitivity::Secret),
            )
            .unwrap();

        let view = build_view(&store, &ContextViewRequest::new(vec![p.clone()], 100)).unwrap();
        let ids: Vec<_> = view.items.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, vec!["live"]);

        let with_secret = build_view(
            &store,
            &ContextViewRequest::new(vec![p], 100).including_secret(true),
        )
        .unwrap();
        assert!(with_secret.items.iter().any(|i| i.id.as_str() == "secret"));
    }

    #[test]
    fn tier_and_kind_filters_work() {
        let p = project("p");
        let store = InMemoryContextStore::new();
        store
            .put(make(
                "note",
                &p,
                1,
                ContextItemKind::Note,
                MemoryTier::Working,
                "a",
            ))
            .unwrap();
        store
            .put(make(
                "decision",
                &p,
                2,
                ContextItemKind::Decision,
                MemoryTier::Durable,
                "b",
            ))
            .unwrap();
        store
            .put(make(
                "evidence",
                &p,
                3,
                ContextItemKind::Evidence,
                MemoryTier::Evidence,
                "c",
            ))
            .unwrap();

        let request = ContextViewRequest::new(vec![p], 100)
            .with_tiers(vec![MemoryTier::Durable])
            .with_kinds(vec![ContextItemKind::Decision]);
        let view = build_view(&store, &request).unwrap();
        assert_eq!(view.items.len(), 1);
        assert_eq!(view.items[0].id.as_str(), "decision");
    }

    #[test]
    fn empty_budget_is_rejected() {
        let p = project("p");
        let store = InMemoryContextStore::new();
        let request = ContextViewRequest::new(vec![p], 0).with_budget(0, 10);
        assert!(matches!(
            build_view(&store, &request),
            Err(ContextError::InvalidBudget(_))
        ));
    }
}
