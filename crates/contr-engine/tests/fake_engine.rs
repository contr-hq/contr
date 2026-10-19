#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "test helpers")]

use std::time::Duration;

use contr_api::containers::v1::{Container, ContainerState};
use contr_api::volumes::v1::CreateVolumeRequest;
use contr_backend::EngineConnector;
use contr_engine::{Change, Engine, EngineCache, EngineError};
use contr_testkit::{FakeEngine, Fixtures};
use futures_util::StreamExt;
use futures_util::stream::BoxStream;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

async fn connect(fake: &FakeEngine) -> Engine {
    let connector = EngineConnector::UnixSocket(fake.socket_path().to_path_buf());
    Engine::connect(&connector).await.unwrap()
}

async fn next<T>(watch: &mut BoxStream<'static, Change<T>>) -> Change<T> {
    tokio::time::timeout(EVENT_TIMEOUT, watch.next())
        .await
        .expect("no change within the timeout")
        .expect("the watch ended")
}

fn named<'a>(containers: &'a [Container], name: &str) -> &'a Container {
    containers
        .iter()
        .find(|container| container.names.iter().any(|candidate| candidate == name))
        .unwrap()
}

#[tokio::test]
async fn reports_engine_info() {
    let fake = FakeEngine::start(Fixtures::basic()).unwrap();
    let info = connect(&fake).await.info().await.unwrap();

    assert_eq!(info.version, "29.0.0-fake");
    assert_eq!(info.os, "linux");
    assert!(info.endpoint.starts_with("unix:///"));
    assert!(!info.rootless);
}

#[tokio::test]
async fn cache_follows_container_events() {
    let fake = FakeEngine::start(Fixtures::basic()).unwrap();
    let engine = connect(&fake).await;
    let cache = EngineCache::start(engine.clone()).await.unwrap();
    let mut watch = cache.watch_containers();

    let Change::Snapshot(initial) = next(&mut watch).await else {
        panic!("a watch starts with a snapshot");
    };
    assert_eq!(initial.len(), 3);
    let web = named(&initial, "shop-web-1").clone();
    assert_eq!(web.state(), ContainerState::Running);

    engine.stop_container("shop-web-1", None).await.unwrap();
    let Change::Upsert(stopped) = next(&mut watch).await else {
        panic!("stopping updates the container");
    };
    assert_eq!(stopped.id, web.id);
    assert_eq!(stopped.state(), ContainerState::Exited);

    engine.remove_container(&web.id, false, false).await.unwrap();
    assert_eq!(next(&mut watch).await, Change::Remove(web.id.clone()));
    assert_eq!(cache.containers().len(), 2);
}

#[tokio::test]
async fn cache_follows_volume_events() {
    let fake = FakeEngine::start(Fixtures::basic()).unwrap();
    let engine = connect(&fake).await;
    let cache = EngineCache::start(engine.clone()).await.unwrap();
    let mut watch = cache.watch_volumes();
    next(&mut watch).await;

    let request = CreateVolumeRequest {
        name: "cache".to_owned(),
        ..Default::default()
    };
    let created = engine.create_volume(&request).await.unwrap();
    let Change::Upsert(upserted) = next(&mut watch).await else {
        panic!("creating a volume upserts it");
    };
    assert_eq!(upserted, created);

    engine.remove_volume("cache", false).await.unwrap();
    assert_eq!(next(&mut watch).await, Change::Remove("cache".to_owned()));
}

#[tokio::test]
async fn closing_the_cache_ends_watches() {
    let fake = FakeEngine::start(Fixtures::basic()).unwrap();
    let cache = EngineCache::start(connect(&fake).await).await.unwrap();
    let mut watch = cache.watch_networks();
    next(&mut watch).await;

    cache.close();

    assert!(watch.next().await.is_none());
}

#[tokio::test]
async fn missing_objects_are_not_found() {
    let fake = FakeEngine::start(Fixtures::basic()).unwrap();
    let engine = connect(&fake).await;

    let error = engine.inspect_container("missing").await.unwrap_err();

    assert!(matches!(error, EngineError::NotFound(_)), "{error:?}");
    assert!(engine.find_volume("missing").await.unwrap().is_none());
}

#[tokio::test]
async fn removing_a_running_container_conflicts() {
    let fake = FakeEngine::start(Fixtures::basic()).unwrap();
    let engine = connect(&fake).await;

    let error = engine.remove_container("shop-db-1", false, false).await.unwrap_err();

    assert!(matches!(error, EngineError::Conflict(_)), "{error:?}");
}
