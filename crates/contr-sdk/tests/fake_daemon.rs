#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "test helpers")]

use std::io::{Cursor, Read, Write};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use contr_daemon::{BackendChoice, Config};
use contr_diagnostics::LogRing;
use contr_sdk::api::compose::v1::{Action, ListProjectsRequest, RunActionRequest, WatchProjectsRequest};
use contr_sdk::api::containers::v1::LogStream;
use contr_sdk::api::containers::v1::watch_containers_response::Event;
use contr_sdk::api::containers::v1::{
    ContainerState, CreateContainerRequest, InspectContainerRequest, KillContainerRequest, PauseContainerRequest,
    PruneContainersRequest, RemoveContainerRequest, RenameContainerRequest, StopContainerRequest,
    UnpauseContainerRequest, WatchContainerStatsRequest, WatchContainersRequest,
};
use contr_sdk::api::daemon::v1::{BackendKind, Capability};
use contr_sdk::api::diagnostics::v1::{
    CheckStatus, CreateBundleRequest, RunChecksRequest, RunRemediationRequest, create_bundle_response,
};
use contr_sdk::api::engine::v1::{
    EngineState, ResetRequest, ResetScope, StartRequest, StopRequest, StreamUsageRequest, WatchStateRequest,
    reset_response, start_response, stop_response,
};
use contr_sdk::api::images::v1::{InspectImageRequest, PushImageRequest, TagImageRequest, push_image_response};
use contr_sdk::api::kubernetes::v1::{ClusterState, GetClusterRequest, ImportImagesRequest, SetCurrentContextRequest};
use contr_sdk::api::networks::v1::{
    CheckHostPortsRequest, ConnectNetworkRequest, DisconnectNetworkRequest, HostPort, InspectNetworkRequest,
};
use contr_sdk::api::operations::v1::follow_operation_response::Event as OperationEvent;
use contr_sdk::api::operations::v1::{
    CancelOperationRequest, FollowOperationRequest, OperationRequest, OperationState, StartOperationRequest,
    WatchOperationsRequest, operation_request, watch_operations_response,
};
use contr_sdk::api::settings::v1::{
    EngineSettings, GetSettingsRequest, ProxyMode, ProxySettings, RegistrySettings, ResetSettingsRequest, Settings,
    UpdateSettingsRequest, UpdatesSettings, WatchSettingsRequest,
};
use contr_sdk::api::updates::v1::{CheckForUpdatesRequest, GetUpdateStatusRequest};
use contr_sdk::{Client, ClientIdentity, Endpoint, Error};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    endpoint: Endpoint,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), contr_daemon::DaemonError>>,
    dir: tempfile::TempDir,
    log: LogRing,
}

impl Daemon {
    fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap())
    }

    /// Daemon whose settings and administrator's settings are `settings.json` and `admin-settings.json` in `dir`.
    fn start_in(dir: tempfile::TempDir) -> Self {
        Self::start_with_feed(dir, None)
    }

    /// Daemon that checks `update_feed` for updates.
    fn start_with_feed(dir: tempfile::TempDir, update_feed: Option<String>) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let instance = NEXT.fetch_add(1, Ordering::Relaxed);

        #[cfg(unix)]
        let endpoint = Endpoint::Unix(dir.path().join("run").join("contrd.sock"));
        #[cfg(windows)]
        let endpoint = Endpoint::NamedPipe(format!(r"\\.\pipe\contr-sdk-test-{}-{instance}", std::process::id()));
        #[cfg(unix)]
        let _ = instance;

        let log = LogRing::new(64 * 1024);
        let config = Config {
            backend: BackendChoice::Fake,
            listen: Some(endpoint.clone()),
            engine_socket: None,
            fixtures: None,
            guest_dir: None,
            default_pipe: false,
            cli_dir: None,
            docker_context: false,
            settings_file: Some(dir.path().join("settings.json")),
            admin_settings: Some(dir.path().join("admin-settings.json")),
            compose: None,
            kubeconfig: Some(dir.path().join("kubeconfig")),
            log: Some(log.clone()),
            update_feed,
        };
        let (shutdown, signal) = oneshot::channel();
        let task = tokio::spawn(contr_daemon::run(config, async {
            let _ = signal.await;
        }));

        Self {
            endpoint,
            shutdown: Some(shutdown),
            task,
            dir,
            log,
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

    /// Stops the daemon and hands back its directory, so another daemon can start with the same settings.
    async fn stop_keeping_dir(mut self) -> tempfile::TempDir {
        let _ = self.shutdown.take().unwrap().send(());
        (&mut self.task).await.unwrap().unwrap();
        self.dir
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

async fn state_of(client: &Client, name: &str) -> Option<ContainerState> {
    client
        .list_containers()
        .await
        .unwrap()
        .into_iter()
        .find(|container| container.names.iter().any(|candidate| candidate == name))
        .map(|container| container.state())
}

#[tokio::test]
async fn creates_and_drives_a_container() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;
    let mut containers = client.containers();

    let created = containers
        .create_container(CreateContainerRequest {
            image: "alpine:3.22".to_owned(),
            name: "made".to_owned(),
            start: true,
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(state_of(&client, "made").await, Some(ContainerState::Running));

    let id = created.id.clone();
    containers
        .pause_container(PauseContainerRequest { id: id.clone() })
        .await
        .unwrap();
    assert_eq!(state_of(&client, "made").await, Some(ContainerState::Paused));
    containers
        .unpause_container(UnpauseContainerRequest { id: id.clone() })
        .await
        .unwrap();
    containers
        .kill_container(KillContainerRequest {
            id: id.clone(),
            signal: String::new(),
        })
        .await
        .unwrap();
    containers
        .rename_container(RenameContainerRequest {
            id: id.clone(),
            name: "renamed".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(state_of(&client, "renamed").await, Some(ContainerState::Exited));

    let pruned = containers
        .prune_containers(PruneContainersRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(pruned.deleted_ids.contains(&id));
    assert_eq!(state_of(&client, "renamed").await, None);

    daemon.stop().await;
}

#[tokio::test]
async fn creating_needs_a_local_image_and_a_free_name() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;
    let mut containers = client.containers();
    let request = |image: &str, name: &str| CreateContainerRequest {
        image: image.to_owned(),
        name: name.to_owned(),
        ..Default::default()
    };

    let missing = containers.create_container(request("redis:8", "")).await.unwrap_err();
    assert!(matches!(Error::from(missing), Error::NotFound(_)));
    let taken = containers
        .create_container(request("alpine:3.22", "shop-web-1"))
        .await
        .unwrap_err();
    assert!(matches!(Error::from(taken), Error::Conflict(_)));

    daemon.stop().await;
}

#[tokio::test]
async fn factory_reset_empties_the_engine() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    assert!(
        client
            .handshake()
            .capabilities()
            .any(|capability| capability == Capability::Reset)
    );
    start_engine(&client).await;

    let mut events = client
        .engine()
        .reset(ResetRequest {
            scope: ResetScope::Factory.into(),
        })
        .await
        .unwrap()
        .into_inner();
    let mut last = None;
    while let Some(event) = events.message().await.unwrap() {
        last = event.event;
    }

    assert_eq!(
        last,
        Some(reset_response::Event::State(EngineState::Uninitialized.into()))
    );
    assert!(matches!(
        client.list_containers().await,
        Err(Error::EngineNotRunning(_))
    ));

    daemon.stop().await;
}

#[tokio::test]
async fn usage_reports_capacity_and_ends_when_the_engine_stops() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let mut usage = client
        .engine()
        .stream_usage(StreamUsageRequest {})
        .await
        .unwrap()
        .into_inner();
    let first = usage.message().await.unwrap().unwrap().usage.unwrap();
    assert_eq!(first.cpus, 4);
    assert_eq!(first.memory_total_bytes, 8_589_934_592);
    assert_eq!(first.running_containers, 2);

    let mut events = client.engine().stop(StopRequest {}).await.unwrap().into_inner();
    while events.message().await.unwrap().is_some() {}
    let ended = loop {
        match usage.message().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the stream ended without a status"),
            Err(status) => break status,
        }
    };
    assert!(matches!(Error::from(ended), Error::EngineNotRunning(_)));

    daemon.stop().await;
}

async fn check_statuses(client: &Client) -> Vec<(String, CheckStatus)> {
    client
        .diagnostics()
        .run_checks(RunChecksRequest {})
        .await
        .unwrap()
        .into_inner()
        .checks
        .into_iter()
        .map(|check| {
            let status = check.status();
            (check.id, status)
        })
        .collect()
}

#[tokio::test]
async fn checks_follow_the_engine_and_unknown_fixes_are_refused() {
    let daemon = Daemon::start();
    let client = daemon.client().await;

    let stopped = check_statuses(&client).await;
    assert!(stopped.contains(&("prerequisites".to_owned(), CheckStatus::Pass)));
    assert!(stopped.contains(&("engine-api".to_owned(), CheckStatus::Skip)));

    start_engine(&client).await;
    let running = check_statuses(&client).await;
    assert!(running.contains(&("engine-api".to_owned(), CheckStatus::Pass)));

    let refused = client
        .diagnostics()
        .run_remediation(RunRemediationRequest {
            action_id: "install-wsl".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(Error::from(refused), Error::NotFound(_)));

    daemon.stop().await;
}

#[tokio::test]
async fn bundles_carry_the_daemon_and_engine_state_without_credentials() {
    let mut daemon = Daemon::start();
    let client = daemon.client().await;
    let proxy = Settings {
        proxy: Some(ProxySettings {
            mode: ProxyMode::Manual.into(),
            http: "http://svc:hunter2@proxy.corp:3128".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    client
        .settings()
        .update_settings(masked(proxy, &["proxy.mode", "proxy.http"]))
        .await
        .unwrap();
    start_engine(&client).await;
    daemon
        .log
        .write_all(b"INFO pulling with Authorization: Bearer hunter2\n")
        .unwrap();

    let mut events = client
        .diagnostics()
        .create_bundle(CreateBundleRequest {})
        .await
        .unwrap()
        .into_inner();
    let (mut steps, mut zip) = (Vec::new(), Vec::new());
    while let Some(event) = events.message().await.unwrap() {
        match event.event.unwrap() {
            create_bundle_response::Event::Progress(progress) => steps.push(progress.message),
            create_bundle_response::Event::Chunk(chunk) => zip.extend(chunk),
        }
    }
    let mut zip = zip::ZipArchive::new(Cursor::new(zip)).unwrap();
    let mut files: Vec<String> = zip.file_names().map(|name| name.unwrap().into_owned()).collect();
    files.sort();

    assert_eq!(steps.first().map(String::as_str), Some("Running the checks"));
    assert_eq!(
        files,
        [
            "checks.json",
            "contrd.json",
            "engine/containers.json",
            "engine/info.json",
            "engine/version.json",
            "kubernetes.json",
            "logs/contrd.log",
            "settings.json",
        ]
    );
    let mut everything = String::new();
    for index in 0..zip.len() {
        zip.by_index(index).unwrap().read_to_string(&mut everything).unwrap();
    }
    assert!(everything.contains("http://<redacted>@proxy.corp:3128"));
    assert!(everything.contains("Authorization: <redacted>"));
    assert!(!everything.contains("hunter2"));

    daemon.stop().await;
}

#[tokio::test]
async fn images_are_tagged_and_pushed() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    client
        .images()
        .tag_image(TagImageRequest {
            source: "alpine:3.22".to_owned(),
            target: "registry.corp:5000/team/base".to_owned(),
        })
        .await
        .unwrap();
    let tags = client
        .images()
        .inspect_image(InspectImageRequest {
            reference: "alpine:3.22".to_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .image
        .unwrap()
        .repo_tags;
    assert_eq!(tags, ["alpine:3.22", "registry.corp:5000/team/base:latest"]);
    let missing = client
        .images()
        .tag_image(TagImageRequest {
            source: "busybox".to_owned(),
            target: "x".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(Error::from(missing), Error::NotFound(_)));

    let mut events = client
        .images()
        .push_image(PushImageRequest {
            reference: "registry.corp:5000/team/base".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    let (mut statuses, mut pushed) = (Vec::new(), None);
    while let Some(event) = events.message().await.unwrap() {
        match event.event.unwrap() {
            push_image_response::Event::Status(status) => statuses.push(status),
            push_image_response::Event::Pushed(image) => pushed = Some(image),
        }
    }
    assert!(
        statuses.contains(&"Pushing 0.0 MB of 0.0 MB".to_owned()),
        "{statuses:?}"
    );
    let pushed = pushed.unwrap();
    assert_eq!(pushed.reference, "registry.corp:5000/team/base:latest");
    assert!(pushed.digest.starts_with("sha256:"), "{}", pushed.digest);

    let mut refused = client
        .images()
        .push_image(PushImageRequest {
            reference: "registry.corp:5000/team/absent:1".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    let failure = loop {
        match refused.message().await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the push of a missing tag succeeded"),
            Err(status) => break status,
        }
    };
    assert!(
        failure.message().contains("does not exist locally"),
        "{}",
        failure.message()
    );

    daemon.stop().await;
}

#[tokio::test]
async fn operations_outlive_the_client_that_started_them() {
    let daemon = Daemon::start();
    let first = daemon.client().await;
    let started = first
        .operations()
        .start_operation(StartOperationRequest {
            request: Some(OperationRequest {
                kind: Some(operation_request::Kind::StartEngine(StartRequest {})),
            }),
        })
        .await
        .unwrap()
        .into_inner()
        .operation
        .unwrap();
    drop(first);

    let client = daemon.client().await;
    let mut operations = client
        .operations()
        .watch_operations(WatchOperationsRequest {})
        .await
        .unwrap()
        .into_inner();
    let Some(watch_operations_response::Event::Snapshot(snapshot)) = operations.message().await.unwrap().unwrap().event
    else {
        panic!("the watch did not start with a snapshot");
    };
    assert_eq!(snapshot.operations[0].id, started.id);

    let mut events = client
        .operations()
        .follow_operation(FollowOperationRequest { id: started.id.clone() })
        .await
        .unwrap()
        .into_inner();
    let (mut engine_state, mut finished) = (None, None);
    while let Some(event) = events.message().await.unwrap() {
        match event.event.unwrap() {
            OperationEvent::EngineState(state) => engine_state = Some(state),
            OperationEvent::Finished(operation) => finished = Some(operation),
            _ => {}
        }
    }
    assert_eq!(engine_state, Some(EngineState::Running.into()));
    let finished = finished.unwrap();
    assert_eq!(finished.state(), OperationState::Succeeded);
    assert!(!finished.cancellable);

    let refused = client
        .operations()
        .cancel_operation(CancelOperationRequest {
            id: "unknown".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(Error::from(refused), Error::NotFound(_)));

    daemon.stop().await;
}

#[tokio::test]
async fn containers_join_and_leave_networks() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;
    let connect = ConnectNetworkRequest {
        network: "bridge".to_owned(),
        container: "migrate".to_owned(),
        aliases: vec!["jobs".to_owned()],
    };
    let mut watch = client
        .containers()
        .watch_containers(WatchContainersRequest {})
        .await
        .unwrap()
        .into_inner();
    let Some(Event::Snapshot(snapshot)) = watch.message().await.unwrap().unwrap().event else {
        panic!("a watch starts with a snapshot");
    };
    let migrate = snapshot
        .containers
        .iter()
        .find(|container| container.names == ["migrate"]);
    assert_eq!(migrate.unwrap().networks, ["shop_default"]);
    let mut next_networks = async || loop {
        let event = tokio::time::timeout(STARTUP_TIMEOUT, watch.message()).await.unwrap();
        if let Some(Event::Upserted(container)) = event.unwrap().unwrap().event {
            break container.networks;
        }
    };

    client.networks().connect_network(connect.clone()).await.unwrap();
    assert_eq!(next_networks().await, ["bridge", "shop_default"]);
    let network = client
        .networks()
        .inspect_network(InspectNetworkRequest {
            id: "bridge".to_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .raw_json;
    assert!(network.contains(r#""Name":"migrate""#), "{network}");
    let container = client
        .containers()
        .inspect_container(InspectContainerRequest {
            id: "migrate".to_owned(),
        })
        .await
        .unwrap()
        .into_inner()
        .raw_json;
    assert!(container.contains(r#""Aliases":["jobs"]"#), "{container}");

    let disconnect = DisconnectNetworkRequest {
        network: "bridge".to_owned(),
        container: "migrate".to_owned(),
        force: false,
    };
    client.networks().disconnect_network(disconnect.clone()).await.unwrap();
    assert_eq!(next_networks().await, ["shop_default"]);
    let again = client.networks().disconnect_network(disconnect).await.unwrap_err();
    assert!(matches!(Error::from(again), Error::Conflict(_)));
    let unknown = client
        .networks()
        .connect_network(ConnectNetworkRequest {
            container: "absent".to_owned(),
            ..connect
        })
        .await
        .unwrap_err();
    assert!(matches!(Error::from(unknown), Error::NotFound(_)));

    daemon.stop().await;
}

/// Serves the GitHub releases list `releases` to every request.
async fn release_feed(releases: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = vec![0; 4096];
            let _ = stream.read(&mut request).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{releases}",
                releases.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    format!("http://{address}/releases")
}

#[tokio::test]
async fn updates_are_checked_while_the_settings_allow() {
    let feed = release_feed(
        r#"[{"tag_name": "v99.0.0", "html_url": "https://example/99", "published_at": "2026-12-01T00:00:00Z", "body": "Notes"}]"#,
    )
    .await;
    let daemon = Daemon::start_with_feed(tempfile::tempdir().unwrap(), Some(feed));
    let client = daemon.client().await;

    let before = client
        .updates()
        .get_update_status(GetUpdateStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(before.current_version, contr_core::product::VERSION);
    assert!(before.available.is_none() && before.checked_at.is_none());

    let checked = client
        .updates()
        .check_for_updates(CheckForUpdatesRequest {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(checked.error, "");
    assert!(checked.checked_at.is_some());
    let available = checked.available.unwrap();
    assert_eq!(
        (
            available.version.as_str(),
            available.url.as_str(),
            available.notes.as_str()
        ),
        ("99.0.0", "https://example/99", "Notes")
    );
    assert_eq!(available.published_at.unwrap().seconds, 1_796_083_200);
    let kept = client
        .updates()
        .get_update_status(GetUpdateStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();
    assert_eq!(kept.available.unwrap().version, "99.0.0");

    let off = Settings {
        updates: Some(UpdatesSettings {
            check: false,
            prereleases: false,
        }),
        ..Default::default()
    };
    client
        .settings()
        .update_settings(masked(off, &["updates.check"]))
        .await
        .unwrap();
    let refused = client
        .updates()
        .check_for_updates(CheckForUpdatesRequest {})
        .await
        .unwrap_err();
    assert!(matches!(Error::from(refused), Error::Conflict(_)));

    daemon.stop().await;
}

#[tokio::test]
async fn a_failed_update_check_says_why() {
    let unreachable = Some("http://127.0.0.1:1/releases".to_owned());
    let daemon = Daemon::start_with_feed(tempfile::tempdir().unwrap(), unreachable);
    let client = daemon.client().await;

    let status = client
        .updates()
        .check_for_updates(CheckForUpdatesRequest {})
        .await
        .unwrap()
        .into_inner()
        .status
        .unwrap();

    assert!(status.available.is_none());
    assert!(status.checked_at.is_some());
    assert!(
        status.error.starts_with("cannot read the published releases: "),
        "{}",
        status.error
    );

    daemon.stop().await;
}

#[tokio::test]
async fn the_daemon_stops_while_clients_watch() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;
    let mut containers = client
        .containers()
        .watch_containers(WatchContainersRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut settings = client
        .settings()
        .watch_settings(WatchSettingsRequest {})
        .await
        .unwrap()
        .into_inner();
    let mut state = client
        .engine()
        .watch_state(WatchStateRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(containers.message().await.unwrap().is_some());
    assert!(settings.message().await.unwrap().is_some());
    assert!(state.message().await.unwrap().is_some());

    tokio::time::timeout(Duration::from_secs(4), daemon.stop())
        .await
        .expect("open watches ended before the grace period ran out");
    while let Ok(Some(_)) = containers.message().await {}
    while let Ok(Some(_)) = settings.message().await {}
    while let Ok(Some(_)) = state.message().await {}
}

#[tokio::test]
async fn container_stats_cover_every_running_container() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;
    let running: Vec<String> = client
        .list_containers()
        .await
        .unwrap()
        .into_iter()
        .filter(|container| container.state() == ContainerState::Running)
        .map(|container| container.id)
        .collect();

    let mut samples = client
        .containers()
        .watch_container_stats(WatchContainerStatsRequest {})
        .await
        .unwrap()
        .into_inner();
    let stats = loop {
        let stats = samples.message().await.unwrap().unwrap().stats;
        if stats.len() == running.len() {
            break stats;
        }
    };

    for id in &running {
        let sample = &stats[id];
        assert!((sample.cpu_percent - 25.0).abs() < 0.01, "{}", sample.cpu_percent);
        assert_eq!(sample.memory_usage_bytes, 52_428_800);
    }

    drop(samples);
    daemon.stop().await;
}

#[tokio::test]
async fn inspecting_an_image_reports_what_it_runs_and_exposes() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let inspected = client
        .images()
        .inspect_image(InspectImageRequest {
            reference: "nginx:1.29".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    let details = inspected.details.unwrap();
    assert_eq!(details.exposed_ports, ["80/tcp"]);
    assert_eq!(details.command, ["nginx", "-g", "daemon off;"]);
    assert_eq!(details.os, "linux");
    assert_eq!(inspected.image.unwrap().repo_tags, ["nginx:1.29"]);

    let missing = client
        .images()
        .inspect_image(InspectImageRequest {
            reference: "redis:8".to_owned(),
        })
        .await
        .unwrap_err();
    assert!(matches!(Error::from(missing), Error::NotFound(_)));

    daemon.stop().await;
}

#[tokio::test]
async fn host_ports_are_checked_while_the_engine_is_stopped() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    let held = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let held_port = u32::from(held.local_addr().unwrap().port());
    let free_port = {
        let probe = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        u32::from(probe.local_addr().unwrap().port())
    };
    let port = |port| HostPort {
        port,
        ..Default::default()
    };

    let statuses = client
        .networks()
        .check_host_ports(CheckHostPortsRequest {
            ports: vec![port(held_port), port(free_port)],
        })
        .await
        .unwrap()
        .into_inner()
        .ports;

    assert!(statuses[0].in_use);
    assert!(!statuses[1].in_use);
    assert_eq!(statuses[1].port.unwrap().port, free_port);

    daemon.stop().await;
}

fn engine_update(start_with_daemon: bool, etag: &str) -> UpdateSettingsRequest {
    UpdateSettingsRequest {
        settings: Some(Settings {
            engine: Some(EngineSettings {
                start_with_daemon,
                serve_default_pipe: false,
                ..Default::default()
            }),
            ..Default::default()
        }),
        update_mask: Some(prost_types::FieldMask {
            paths: vec!["engine.start_with_daemon".to_owned()],
        }),
        etag: etag.to_owned(),
    }
}

#[tokio::test]
async fn settings_persist_and_start_the_engine_with_the_daemon() {
    let dir = tempfile::tempdir().unwrap();
    let settings_file = dir.path().join("settings.json");
    let daemon = Daemon::start_in(dir);
    let client = daemon.client().await;

    let initial = client
        .settings()
        .get_settings(GetSettingsRequest {})
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    let engine = initial.settings.unwrap().engine.unwrap();
    assert!(!engine.start_with_daemon);
    assert!(engine.serve_default_pipe);

    let updated = client
        .settings()
        .update_settings(engine_update(true, &initial.etag))
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    let engine = updated.settings.unwrap().engine.unwrap();
    assert!(engine.start_with_daemon);
    assert!(engine.serve_default_pipe, "only the masked path changes");

    let stale = client
        .settings()
        .update_settings(engine_update(false, &initial.etag))
        .await
        .unwrap_err();
    assert_eq!(stale.code(), tonic::Code::Aborted);
    let written = std::fs::read_to_string(&settings_file).unwrap();
    assert!(written.contains("\"startWithDaemon\": true"), "{written}");

    let dir = daemon.stop_keeping_dir().await;
    let daemon = Daemon::start_in(dir);
    let client = daemon.client().await;
    let deadline = tokio::time::Instant::now() + STARTUP_TIMEOUT;
    while client.engine_state().await.unwrap().state() != EngineState::Running {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the engine did not start with the daemon"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let reset = client
        .settings()
        .reset_settings(ResetSettingsRequest {})
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert!(!reset.settings.unwrap().engine.unwrap().start_with_daemon);

    daemon.stop().await;
}

#[tokio::test]
async fn proxy_and_mirror_settings_are_checked_and_stored() {
    let daemon = Daemon::start();
    let client = daemon.client().await;

    let defaults = client
        .settings()
        .get_settings(GetSettingsRequest {})
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap()
        .settings
        .unwrap();
    assert_eq!(defaults.proxy.unwrap().mode(), ProxyMode::System);
    assert!(defaults.certificates.unwrap().trust_system);

    let manual = Settings {
        proxy: Some(ProxySettings {
            mode: ProxyMode::Manual.into(),
            http: "proxy.corp:3128".to_owned(),
            ..Default::default()
        }),
        registry: Some(RegistrySettings {
            mirrors: vec!["https://mirror.corp".to_owned()],
        }),
        ..Default::default()
    };
    let stored = client
        .settings()
        .update_settings(masked(manual, &["proxy.mode", "proxy.http", "registry.mirrors"]))
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap()
        .settings
        .unwrap();
    let proxy = stored.proxy.unwrap();
    assert_eq!(proxy.mode(), ProxyMode::Manual);
    assert_eq!(proxy.http, "proxy.corp:3128");
    assert_eq!(stored.registry.unwrap().mirrors, ["https://mirror.corp"]);

    let unusable = Settings {
        registry: Some(RegistrySettings {
            mirrors: vec!["mirror.corp".to_owned()],
        }),
        ..Default::default()
    };
    let refused = client
        .settings()
        .update_settings(masked(unusable, &["registry.mirrors"]))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);
    assert!(
        refused.message().starts_with("registry.mirrors: "),
        "{}",
        refused.message()
    );

    let unnamed = Settings {
        proxy: Some(ProxySettings::default()),
        ..Default::default()
    };
    let refused = client
        .settings()
        .update_settings(masked(unnamed, &["proxy.mode"]))
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::InvalidArgument);

    daemon.stop().await;
}

#[tokio::test]
async fn daemon_json_is_checked_and_stored() {
    let daemon = Daemon::start();
    let client = daemon.client().await;

    let daemon_json = |text: &str| Settings {
        engine: Some(EngineSettings {
            daemon_json: text.to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    let stored = client
        .settings()
        .update_settings(masked(daemon_json(r#"{"log-level": "warn"}"#), &["engine.daemon_json"]))
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    let text = stored.settings.unwrap().engine.unwrap().daemon_json;
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&text).unwrap(),
        serde_json::json!({"log-level": "warn"})
    );
    for (text, reason) in [
        (
            r#"{"hosts": ["tcp://0.0.0.0:2375"]}"#,
            "engine.daemon_json: hosts is not available",
        ),
        ("[]", "engine.daemon_json is not a JSON object"),
        ("{", "engine.daemon_json is not JSON"),
    ] {
        let refused = client
            .settings()
            .update_settings(masked(daemon_json(text), &["engine.daemon_json"]))
            .await
            .unwrap_err();
        assert_eq!(refused.code(), tonic::Code::InvalidArgument);
        assert!(refused.message().starts_with(reason), "{}", refused.message());
    }

    daemon.stop().await;
}

#[tokio::test]
async fn passing_the_proxy_to_containers_applies_at_the_next_engine_start() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    let defaults = client
        .settings()
        .get_settings(GetSettingsRequest {})
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert!(!defaults.settings.unwrap().proxy.unwrap().pass_to_containers);
    start_engine(&client).await;

    let passing = Settings {
        proxy: Some(ProxySettings {
            pass_to_containers: true,
            ..Default::default()
        }),
        ..Default::default()
    };
    let response = client
        .settings()
        .update_settings(masked(passing, &["proxy.pass_to_containers"]))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.restart_required, ["proxy.pass_to_containers"]);
    let proxy = response.state.unwrap().settings.unwrap().proxy.unwrap();
    assert!(proxy.pass_to_containers);
    assert_eq!(proxy.mode(), ProxyMode::System);

    let dir = daemon.stop_keeping_dir().await;
    let daemon = Daemon::start_in(dir);
    let stored = daemon
        .client()
        .await
        .settings()
        .get_settings(GetSettingsRequest {})
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert!(stored.settings.unwrap().proxy.unwrap().pass_to_containers);

    daemon.stop().await;
}

/// Update of the `paths` of `settings`, whatever changed since the client last read them.
fn masked(settings: Settings, paths: &[&str]) -> UpdateSettingsRequest {
    UpdateSettingsRequest {
        settings: Some(settings),
        update_mask: Some(prost_types::FieldMask {
            paths: paths.iter().map(|path| (*path).to_owned()).collect(),
        }),
        etag: String::new(),
    }
}

#[tokio::test]
async fn compose_projects_group_the_containers_compose_created() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let projects = client
        .compose()
        .list_projects(ListProjectsRequest {})
        .await
        .unwrap()
        .into_inner()
        .projects;

    assert_eq!(projects.len(), 1);
    let shop = &projects[0];
    assert_eq!(shop.name, "shop");
    assert!(!shop.added);
    let services: Vec<(&str, usize)> = shop
        .services
        .iter()
        .map(|service| (service.name.as_str(), service.containers.len()))
        .collect();
    assert_eq!(services, [("db", 1), ("web", 1)]);

    let mut watch = client
        .compose()
        .watch_projects(WatchProjectsRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(watch.message().await.unwrap().unwrap().projects, projects);

    let unknown = client
        .compose()
        .run_action(RunActionRequest {
            project: "nope".to_owned(),
            action: Action::Up.into(),
            ..RunActionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(unknown.code(), tonic::Code::NotFound);

    daemon.stop().await;
}

#[tokio::test]
async fn kubernetes_starts_disabled_and_switches_contexts_in_its_kubeconfig() {
    let dir = tempfile::tempdir().unwrap();
    let kubeconfig = dir.path().join("kubeconfig");
    std::fs::write(
        &kubeconfig,
        "apiVersion: v1\nkind: Config\ncurrent-context: staging-eu\ncontexts:\n- name: staging-eu\n  context: {}\n",
    )
    .unwrap();
    let daemon = Daemon::start_in(dir);
    let client = daemon.client().await;
    let mut kubernetes = client.kubernetes();

    let cluster = kubernetes
        .get_cluster(GetClusterRequest {})
        .await
        .unwrap()
        .into_inner()
        .cluster
        .unwrap();
    assert_eq!(cluster.state(), ClusterState::Disabled);
    assert_eq!(cluster.available_versions[0], "v1.37.1");
    assert_eq!(cluster.api_server, "https://127.0.0.1:6443");
    assert_eq!(cluster.current_context, "staging-eu");

    let switched = kubernetes
        .set_current_context(SetCurrentContextRequest { current: true })
        .await
        .unwrap()
        .into_inner()
        .cluster
        .unwrap();
    assert_eq!(switched.current_context, "contr");
    let restored = kubernetes
        .set_current_context(SetCurrentContextRequest { current: false })
        .await
        .unwrap()
        .into_inner()
        .cluster
        .unwrap();
    assert_eq!(restored.current_context, "staging-eu");

    let refused = kubernetes
        .import_images(ImportImagesRequest {
            images: vec!["alpine:3.20".to_owned()],
        })
        .await
        .unwrap_err();
    assert_eq!(refused.code(), tonic::Code::FailedPrecondition);

    daemon.stop().await;
}

#[tokio::test]
async fn the_fake_engine_streams_logs() {
    let daemon = Daemon::start();
    let client = daemon.client().await;
    start_engine(&client).await;

    let mut chunks = client
        .containers()
        .stream_logs(contr_sdk::api::containers::v1::StreamLogsRequest {
            id: "shop-web-1".to_owned(),
            tail_lines: Some(2),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let mut lines = Vec::new();
    while let Some(chunk) = chunks.message().await.unwrap() {
        lines.push((chunk.stream(), String::from_utf8_lossy(&chunk.data).trim().to_owned()));
    }
    assert_eq!(
        lines,
        [
            (
                LogStream::Stderr,
                "shop-web-1 found no configuration file; using defaults".to_owned()
            ),
            (LogStream::Stdout, "shop-web-1 is ready".to_owned()),
        ]
    );

    let mut project = client
        .compose()
        .stream_logs(contr_sdk::api::compose::v1::StreamLogsRequest {
            project: "shop".to_owned(),
            services: vec!["web".to_owned()],
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let first = project.message().await.unwrap().unwrap();
    assert_eq!(first.service, "web");

    daemon.stop().await;
}

#[tokio::test]
async fn administrators_lock_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("admin-settings.json"),
        r#"{"engine": {"startWithDaemon": false}}"#,
    )
    .unwrap();
    let daemon = Daemon::start_in(dir);
    let client = daemon.client().await;

    let state = client
        .settings()
        .get_settings(GetSettingsRequest {})
        .await
        .unwrap()
        .into_inner()
        .state
        .unwrap();
    assert_eq!(state.locked_paths, ["engine.start_with_daemon"]);

    let refused = client
        .settings()
        .update_settings(engine_update(true, ""))
        .await
        .unwrap_err();
    assert!(matches!(Error::from(refused), Error::SettingLocked(_)));

    daemon.stop().await;
}
