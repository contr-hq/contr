#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "test helpers")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use contr_daemon::{BackendChoice, Config};
use contr_sdk::api::containers::v1::watch_containers_response::Event;
use contr_sdk::api::containers::v1::{
    ContainerState, InspectContainerRequest, RemoveContainerRequest, StopContainerRequest, WatchContainersRequest,
};
use contr_sdk::api::daemon::v1::BackendKind;
use contr_sdk::api::engine::v1::{EngineState, StartRequest, StopRequest, start_response, stop_response};
use contr_sdk::{Client, ClientIdentity, Endpoint, Error};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    endpoint: Endpoint,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), contr_daemon::DaemonError>>,
    #[cfg(unix)]
    _dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let instance = NEXT.fetch_add(1, Ordering::Relaxed);

        #[cfg(unix)]
        let dir = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let endpoint = Endpoint::Unix(dir.path().join("run").join("contrd.sock"));
        #[cfg(windows)]
        let endpoint = Endpoint::NamedPipe(format!(r"\\.\pipe\contr-sdk-test-{}-{instance}", std::process::id()));
        #[cfg(unix)]
        let _ = instance;

        let config = Config {
            backend: BackendChoice::Fake,
            listen: Some(endpoint.clone()),
            engine_socket: None,
            fixtures: None,
            guest_dir: None,
            docker_context: false,
        };
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(contr_daemon::run(config, async {
            let _ = signal.await;
        }));

        Self {
            endpoint,
            shutdown: Some(shutdown),
            task,
            #[cfg(unix)]
            _dir: dir,
        }
    }

    async fn client(&self) -> Client {
        let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
        loop {
            match Client::connect(&self.endpoint, &ClientIdentity::default()).await {
                Ok(client) => return client,
                Err(Error::Connect { .. }) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => panic!("cannot connect: {error}"),
            }
        }
    }

    async fn stop(mut self) {
        let _ = self.shutdown.take().unwrap().send(());
        (&mut self.task).await.unwrap().unwrap();
    }
}

async fn start_engine(client: &Client) {
    let mut events = client.engine().start(StartRequest {}).await.unwrap().into_inner();
    let mut last = None;
    while let Some(event) = events.message().await.unwrap() {
        last = event.event;
    }
    assert_eq!(last, Some(start_response::Event::State(EngineState::Running.into())));
}

#[tokio::test]
async fn handshake_reports_the_contract_revision() {
    let daemon = Daemon::start();
    let client = daemon.client().await;

    assert_eq!(client.handshake().api_revision, contr_sdk::api::API_REVISION);
    assert_eq!(client.handshake().backend_kind(), BackendKind::Fake);

    daemon.stop().await;
}

#[tokio::test]
async fn engine_calls_fail_until_the_engine_starts() {
    let daemon = Daemon::start();
    let client = daemon.client().await;

    assert_eq!(client.engine_state().await.unwrap().state(), EngineState::Stopped);
    assert!(matches!(
        client.list_containers().await,
        Err(Error::EngineNotRunning(_))
    ));
    assert!(client.info().await.unwrap().engine.is_none());

    start_engine(&client).await;

    assert_eq!(client.list_containers().await.unwrap().len(), 3);
    let info = client.info().await.unwrap();
    assert_eq!(info.engine.unwrap().version, "29.0.0-fake");

    daemon.stop().await;
}

#[tokio::test]
async fn watch_reports_container_changes_and_ends_when_the_engine_stops() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let mut watch = client
        .containers()
        .watch_containers(WatchContainersRequest {})
        .await
        .unwrap()
        .into_inner();
    let Some(Event::Snapshot(snapshot)) = watch.message().await.unwrap().unwrap().event else {
        panic!("a watch starts with a snapshot");
    };
    assert_eq!(snapshot.containers.len(), 3);

    let request = StopContainerRequest {
        id: "shop-web-1".to_owned(),
        timeout_seconds: Some(0),
    };
    client.containers().stop_container(request).await.unwrap();
    let Some(Event::Upserted(stopped)) = watch.message().await.unwrap().unwrap().event else {
        panic!("stopping a container upserts it");
    };
    assert_eq!(stopped.state(), ContainerState::Exited);

    let mut stop = client.engine().stop(StopRequest {}).await.unwrap().into_inner();
    let mut last = None;
    while let Some(event) = stop.message().await.unwrap() {
        last = event.event;
    }
    assert_eq!(last, Some(stop_response::Event::State(EngineState::Stopped.into())));

    let ended = loop {
        match watch.message().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the watch ended without a status"),
            Err(status) => break Error::from(status),
        }
    };
    assert!(matches!(ended, Error::EngineNotRunning(_)), "{ended:?}");

    daemon.stop().await;
}

#[tokio::test]
async fn engine_errors_map_to_typed_errors() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let missing = client
        .containers()
        .inspect_container(InspectContainerRequest {
            id: "missing".to_owned(),
        })
        .await
        .unwrap_err();
    let running = client
        .containers()
        .remove_container(RemoveContainerRequest {
            id: "shop-db-1".to_owned(),
            ..Default::default()
        })
        .await
        .unwrap_err();

    assert!(matches!(Error::from(missing), Error::NotFound(_)));
    assert!(matches!(Error::from(running), Error::Conflict(_)));

    daemon.stop().await;
}

#[tokio::test]
async fn inspect_returns_details_and_the_engine_document() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let inspected = client
        .containers()
        .inspect_container(InspectContainerRequest {
            id: "shop-db-1".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(inspected.container.unwrap().names, ["shop-db-1"]);
    assert_eq!(inspected.details.unwrap().mounts[0].name, "shop_pgdata");
    assert!(inspected.raw_json.contains("\"Name\":\"/shop-db-1\""));

    daemon.stop().await;
}
