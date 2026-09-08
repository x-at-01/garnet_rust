use std::sync::Arc;

use wedb_blocking::{CollectionItemBroker, MemoryCollectionStore};

pub fn exact_broker() -> (Arc<CollectionItemBroker>, Arc<MemoryCollectionStore>) {
  let store = Arc::new(MemoryCollectionStore::new());
  let broker = Arc::new(CollectionItemBroker::with_provider(store.clone()));
  (broker, store)
}
