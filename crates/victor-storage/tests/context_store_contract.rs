use victor_context::{
    ContextContent, ContextItem, ContextItemId, ContextItemKind, ContextScope, ContextSource,
    ContextStore, InMemoryContextStore, MemoryTier, Origin, Retention, Sensitivity,
};
use victor_storage::SqliteContextStore;

fn item(id: &str, scope: &ContextScope, created: u64) -> ContextItem {
    ContextItem::new(
        ContextItemId::parse(id).unwrap(),
        scope.clone(),
        ContextItemKind::Note,
        MemoryTier::Durable,
        ContextContent::inline(format!("treść {id}")),
        ContextSource::new(Origin::User, "contract-test"),
        created,
    )
    .unwrap()
    .with_sensitivity(Sensitivity::Sensitive)
    .with_retention(Retention::TtlFromCreation(100))
    .unwrap()
}

fn run_context_store_contract<S: ContextStore>(store: S) {
    let project_a = ContextScope::project("project-a").unwrap();
    let project_b = ContextScope::project("project-b").unwrap();

    store.put(item("same", &project_a, 10)).unwrap();
    store.put(item("same", &project_b, 100)).unwrap();
    store.put(item("newer", &project_a, 30)).unwrap();

    assert_eq!(
        store
            .get(&ContextItemId::parse("same").unwrap(), &project_a)
            .unwrap()
            .unwrap()
            .scope,
        project_a
    );
    assert!(store
        .get(
            &ContextItemId::parse("same").unwrap(),
            &ContextScope::task("other").unwrap()
        )
        .unwrap()
        .is_none());

    let listed = store.list(&project_a).unwrap();
    assert_eq!(
        listed
            .iter()
            .map(|item| item.id.as_str())
            .collect::<Vec<_>>(),
        vec!["newer", "same"]
    );

    assert!(!store
        .delete(
            &ContextItemId::parse("same").unwrap(),
            &ContextScope::task("other").unwrap()
        )
        .unwrap());
    assert!(store
        .get(&ContextItemId::parse("same").unwrap(), &project_a)
        .unwrap()
        .is_some());
    assert!(store
        .delete(&ContextItemId::parse("same").unwrap(), &project_a)
        .unwrap());

    assert_eq!(store.purge_expired(131).unwrap(), 1);
    assert!(store.list(&project_a).unwrap().is_empty());
    assert!(store
        .get(&ContextItemId::parse("same").unwrap(), &project_b)
        .unwrap()
        .is_some());
}

#[test]
fn identical_contract_runs_against_in_memory_store() {
    run_context_store_contract(InMemoryContextStore::new());
}

#[test]
fn identical_contract_runs_against_sqlite_store() {
    run_context_store_contract(SqliteContextStore::in_memory().unwrap());
}
