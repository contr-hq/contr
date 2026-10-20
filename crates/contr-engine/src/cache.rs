//! Engine state cache: one full listing, then `/events` deltas, then a periodic resync that repairs anything an event
//! did not cover. Watch RPCs read only from here.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, Weak};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bollard::models::{EventMessage, EventMessageTypeEnum};
use bollard::query_parameters::EventsOptions;
use contr_api::containers::v1::{Container, ContainerState};
use contr_api::engine::v1::EngineUsage;
use contr_api::images::v1::Image;
use contr_api::networks::v1::Network;
use contr_api::volumes::v1::Volume;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use crate::usage::{RunningIds, Usage};
use crate::{Engine, EngineError};

const CHANGE_QUEUE: usize = 256;
const RESYNC_INTERVAL: Duration = Duration::from_secs(30);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);
const WATCHED_EVENT_TYPES: [&str; 4] = ["container", "image", "volume", "network"];
const IGNORED_CONTAINER_ACTIONS: [&str; 9] = [
    "attach",
    "detach",
    "resize",
    "top",
    "copy",
    "archive-path",
    "extract-to-dir",
    "export",
    "commit",
];

/// Change to a cached list, in the order Watch RPCs send it.
#[derive(Clone, Debug, PartialEq)]
pub enum Change<T> {
    /// Every item; replaces whatever the receiver holds.
    Snapshot(Vec<T>),
    Upsert(T),
    /// Key of a removed item: the ID, or the name for volumes.
    Remove(String),
}

trait Keyed {
    fn key(&self) -> &str;
}

impl Keyed for Container {
    fn key(&self) -> &str {
        &self.id
    }
}

impl Keyed for Image {
    fn key(&self) -> &str {
        &self.id
    }
}

impl Keyed for Volume {
    fn key(&self) -> &str {
        &self.name
    }
}

impl Keyed for Network {
    fn key(&self) -> &str {
        &self.id
    }
}

/// Containers, images, volumes and networks of one engine, kept current by a background task until
/// [`EngineCache::close`] runs or the cache is dropped.
pub struct EngineCache {
    engine: Engine,
    collections: Collections,
    usage: Arc<Usage>,
    cancel: CancellationToken,
}

impl EngineCache {
    /// Lists everything once, then follows engine events on a background task.
    pub async fn start(engine: Engine) -> Result<Self, EngineError> {
        let collections = Collections::default();
        let since = unix_now() - 1;
        collections.resync(&engine).await?;

        let cancel = CancellationToken::new();
        tokio::spawn(follow(engine.clone(), collections.clone(), cancel.clone(), since));

        Ok(Self {
            engine,
            collections,
            usage: Arc::default(),
            cancel,
        })
    }

    /// The engine's resource usage about every two seconds; ends when the cache closes. Each container's stats are
    /// streamed only while some usage stream is open.
    pub fn watch_usage(&self) -> BoxStream<'static, EngineUsage> {
        let containers = Arc::clone(&self.collections.containers);
        let running: RunningIds = Arc::new(move || {
            containers
                .list()
                .into_iter()
                .filter(|container| container.state() == ContainerState::Running)
                .map(|container| container.id)
                .collect()
        });

        self.usage.watch(&self.engine, running, self.cancel.clone())
    }

    pub fn containers(&self) -> Vec<Container> {
        self.collections.containers.list()
    }

    /// Snapshot, then every change; ends when the cache closes.
    pub fn watch_containers(&self) -> BoxStream<'static, Change<Container>> {
        self.collections.containers.watch(self.cancel.clone())
    }

    /// Snapshot, then every change; ends when the cache closes.
    pub fn watch_images(&self) -> BoxStream<'static, Change<Image>> {
        self.collections.images.watch(self.cancel.clone())
    }

    /// Snapshot, then every change; ends when the cache closes.
    pub fn watch_volumes(&self) -> BoxStream<'static, Change<Volume>> {
        self.collections.volumes.watch(self.cancel.clone())
    }

    /// Snapshot, then every change; ends when the cache closes.
    pub fn watch_networks(&self) -> BoxStream<'static, Change<Network>> {
        self.collections.networks.watch(self.cancel.clone())
    }

    /// ID of the cached container that `reference` names: an ID, a unique ID prefix or a name.
    pub fn resolve_container(&self, reference: &str) -> Option<String> {
        let containers = self.collections.containers.lock();
        if let Some(container) = containers
            .values()
            .find(|container| container.names.iter().any(|name| name == reference))
        {
            return Some(container.id.clone());
        }

        let mut matches = containers
            .keys()
            .filter(|id| !reference.is_empty() && id.starts_with(reference));
        match (matches.next(), matches.next()) {
            (Some(id), None) => Some(id.clone()),
            _ => None,
        }
    }

    /// Re-reads one container so a caller that just changed it sees the result without waiting for its event.
    pub async fn refresh_container(&self, id: &str) {
        if let Err(error) = self.collections.refresh_container(&self.engine, id).await {
            tracing::debug!(%error, id, "refreshing a container failed");
        }
    }

    /// Re-reads one volume so a caller that just changed it sees the result without waiting for its event.
    pub async fn refresh_volume(&self, name: &str) {
        match self.engine.find_volume(name).await {
            Ok(volume) => self.collections.volumes.upsert_or_remove(name, volume),
            Err(error) => tracing::debug!(%error, name, "refreshing a volume failed"),
        }
    }

    /// Re-reads every network so a caller that just changed one sees the result without waiting for its event.
    pub async fn refresh_networks(&self) {
        match self.engine.list_networks().await {
            Ok(networks) => self.collections.networks.replace_all(networks),
            Err(error) => tracing::debug!(%error, "refreshing networks failed"),
        }
    }

    /// Re-reads every image so a caller that just changed one sees the result without waiting for its event.
    pub async fn refresh_images(&self) {
        match self.engine.list_images().await {
            Ok(images) => self.collections.images.replace_all(images),
            Err(error) => tracing::debug!(%error, "refreshing images failed"),
        }
    }

    /// Stops following the engine and ends every watch stream.
    pub fn close(&self) {
        self.cancel.cancel();
    }
}

impl Drop for EngineCache {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

#[derive(Clone, Default)]
struct Collections {
    containers: Arc<Collection<Container>>,
    images: Arc<Collection<Image>>,
    volumes: Arc<Collection<Volume>>,
    networks: Arc<Collection<Network>>,
}

impl Collections {
    async fn resync(&self, engine: &Engine) -> Result<(), EngineError> {
        let (containers, images, volumes, networks) = tokio::try_join!(
            engine.list_containers(),
            engine.list_images(),
            engine.list_volumes(),
            engine.list_networks(),
        )?;

        self.containers.replace_all(containers);
        self.images.replace_all(images);
        self.volumes.replace_all(volumes);
        self.networks.replace_all(networks);
        Ok(())
    }

    async fn apply(&self, engine: &Engine, event: EventMessage) {
        let action = event.action.unwrap_or_default();
        let id = event.actor.and_then(|actor| actor.id).unwrap_or_default();

        let result = match event.typ {
            Some(EventMessageTypeEnum::CONTAINER) => self.apply_container_event(engine, &action, &id).await,
            Some(EventMessageTypeEnum::IMAGE) if !matches!(action.as_str(), "push" | "save") => {
                engine.list_images().await.map(|images| self.images.replace_all(images))
            }
            Some(EventMessageTypeEnum::VOLUME) => match action.as_str() {
                "create" => engine
                    .find_volume(&id)
                    .await
                    .map(|volume| self.volumes.upsert_or_remove(&id, volume)),
                "destroy" => {
                    self.volumes.remove(&id);
                    Ok(())
                }
                _ => Ok(()),
            },
            Some(EventMessageTypeEnum::NETWORK) => match action.as_str() {
                "create" => engine
                    .find_network(&id)
                    .await
                    .map(|network| self.networks.upsert_or_remove(&id, network)),
                "destroy" | "remove" => {
                    self.networks.remove(&id);
                    Ok(())
                }
                _ => Ok(()),
            },
            _ => Ok(()),
        };

        if let Err(error) = result {
            tracing::debug!(%error, action, id, "applying an engine event failed");
        }
    }

    async fn apply_container_event(&self, engine: &Engine, action: &str, id: &str) -> Result<(), EngineError> {
        if action == "destroy" {
            self.containers.remove(id);
            Ok(())
        } else if action.starts_with("exec_") || IGNORED_CONTAINER_ACTIONS.contains(&action) {
            Ok(())
        } else {
            self.refresh_container(engine, id).await
        }
    }

    async fn refresh_container(&self, engine: &Engine, id: &str) -> Result<(), EngineError> {
        let container = engine.find_container(id).await?;
        self.containers.upsert_or_remove(id, container);
        Ok(())
    }
}

async fn follow(engine: Engine, collections: Collections, cancel: CancellationToken, mut since: i64) {
    let filters = HashMap::from([("type".to_owned(), WATCHED_EVENT_TYPES.map(str::to_owned).to_vec())]);

    loop {
        let options = EventsOptions {
            since: Some(since.to_string()),
            until: None,
            filters: Some(filters.clone()),
        };
        let mut events = engine.docker().events(Some(options));
        let mut resync = time::interval_at(Instant::now() + RESYNC_INTERVAL, RESYNC_INTERVAL);

        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                _ = resync.tick() => {
                    if let Err(error) = collections.resync(&engine).await {
                        tracing::debug!(%error, "periodic engine resync failed");
                    }
                }
                event = events.next() => match event {
                    Some(Ok(event)) => collections.apply(&engine, event).await,
                    Some(Err(error)) => {
                        tracing::debug!(%error, "engine event stream failed");
                        break;
                    }
                    None => break,
                },
            }
        }

        tokio::select! {
            () = cancel.cancelled() => return,
            () = time::sleep(RECONNECT_DELAY) => {}
        }
        since = unix_now() - 1;
        if let Err(error) = collections.resync(&engine).await {
            tracing::debug!(%error, "engine resync after reconnecting failed");
        }
    }
}

/// Keyed items plus their change feed; every change is sent while the lock is held, so a subscriber's snapshot and its
/// changes neither overlap nor leave a gap.
struct Collection<T> {
    items: Mutex<BTreeMap<String, T>>,
    changes: broadcast::Sender<Change<T>>,
}

impl<T: Clone> Default for Collection<T> {
    fn default() -> Self {
        Self {
            items: Mutex::new(BTreeMap::new()),
            changes: broadcast::channel(CHANGE_QUEUE).0,
        }
    }
}

impl<T: Keyed + Clone + PartialEq + Send + Sync + 'static> Collection<T> {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, T>> {
        self.items.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn list(&self) -> Vec<T> {
        self.lock().values().cloned().collect()
    }

    fn upsert(&self, item: T) {
        let mut items = self.lock();
        if items.get(item.key()) != Some(&item) {
            items.insert(item.key().to_owned(), item.clone());
            let _ = self.changes.send(Change::Upsert(item));
        }
    }

    fn remove(&self, key: &str) {
        let mut items = self.lock();
        if items.remove(key).is_some() {
            let _ = self.changes.send(Change::Remove(key.to_owned()));
        }
    }

    fn upsert_or_remove(&self, key: &str, item: Option<T>) {
        match item {
            Some(item) => self.upsert(item),
            None => self.remove(key),
        }
    }

    fn replace_all(&self, fresh: Vec<T>) {
        let fresh: BTreeMap<String, T> = fresh.into_iter().map(|item| (item.key().to_owned(), item)).collect();
        let mut items = self.lock();

        let removed: Vec<String> = items.keys().filter(|key| !fresh.contains_key(*key)).cloned().collect();
        for key in removed {
            items.remove(&key);
            let _ = self.changes.send(Change::Remove(key));
        }
        for (key, item) in fresh {
            if items.get(&key) != Some(&item) {
                items.insert(key, item.clone());
                let _ = self.changes.send(Change::Upsert(item));
            }
        }
    }

    fn subscribe(&self) -> (Vec<T>, broadcast::Receiver<Change<T>>) {
        let items = self.lock();
        (items.values().cloned().collect(), self.changes.subscribe())
    }

    /// A receiver that falls behind gets a fresh snapshot instead of the changes it missed.
    fn watch(self: &Arc<Self>, cancel: CancellationToken) -> BoxStream<'static, Change<T>> {
        let (initial, feed) = self.subscribe();
        let collection = Arc::downgrade(self);

        let changes = stream::unfold(
            (feed, collection, cancel),
            |(mut feed, collection, cancel): (_, Weak<Self>, CancellationToken)| async move {
                let next = tokio::select! {
                    () = cancel.cancelled() => return None,
                    next = feed.recv() => next,
                };
                match next {
                    Ok(change) => Some((change, (feed, collection, cancel))),
                    Err(RecvError::Lagged(_)) => {
                        let (snapshot, fresh_feed) = collection.upgrade()?.subscribe();
                        Some((Change::Snapshot(snapshot), (fresh_feed, collection, cancel)))
                    }
                    Err(RecvError::Closed) => None,
                }
            },
        );

        stream::once(std::future::ready(Change::Snapshot(initial)))
            .chain(changes)
            .boxed()
    }
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq)]
    struct Item(&'static str, u32);

    impl Keyed for Item {
        fn key(&self) -> &str {
            self.0
        }
    }

    #[tokio::test]
    async fn watch_starts_with_a_snapshot_and_sends_only_real_changes() {
        let collection = Arc::new(Collection::default());
        collection.replace_all(vec![Item("a", 1), Item("b", 1)]);
        let mut watch = collection.watch(CancellationToken::new());

        collection.replace_all(vec![Item("a", 1), Item("b", 2), Item("c", 1)]);
        collection.upsert(Item("c", 1));
        collection.remove("a");

        assert_eq!(
            watch.next().await,
            Some(Change::Snapshot(vec![Item("a", 1), Item("b", 1)]))
        );
        assert_eq!(watch.next().await, Some(Change::Upsert(Item("b", 2))));
        assert_eq!(watch.next().await, Some(Change::Upsert(Item("c", 1))));
        assert_eq!(watch.next().await, Some(Change::Remove("a".to_owned())));
    }

    #[tokio::test]
    async fn lagging_watch_resynchronizes_with_a_snapshot() {
        let collection = Arc::new(Collection::default());
        let mut watch = collection.watch(CancellationToken::new());
        assert_eq!(watch.next().await, Some(Change::Snapshot(Vec::new())));

        for version in 0..=u32::try_from(CHANGE_QUEUE).unwrap() {
            collection.upsert(Item("a", version));
        }

        let last = u32::try_from(CHANGE_QUEUE).unwrap();
        assert_eq!(watch.next().await, Some(Change::Snapshot(vec![Item("a", last)])));
    }

    #[tokio::test]
    async fn cancelling_ends_the_watch() {
        let collection: Arc<Collection<Item>> = Arc::new(Collection::default());
        let cancel = CancellationToken::new();
        let mut watch = collection.watch(cancel.clone());
        watch.next().await;

        cancel.cancel();

        assert_eq!(watch.next().await, None);
    }
}
