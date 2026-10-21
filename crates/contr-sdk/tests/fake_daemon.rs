#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, reason = "test helpers")]

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use contr_daemon::{BackendChoice, Config};
use contr_sdk::api::compose::v1::{Action, ListProjectsRequest, RunActionRequest, WatchProjectsRequest};
use contr_sdk::api::containers::v1::watch_containers_response::Event;
use contr_sdk::api::containers::v1::{
    ContainerState, CreateContainerRequest, InspectContainerRequest, KillContainerRequest, PauseContainerRequest,
    PruneContainersRequest, RemoveContainerRequest, RenameContainerRequest, StopContainerRequest,
    UnpauseContainerRequest, WatchContainerStatsRequest, WatchContainersRequest,
};
use contr_sdk::api::daemon::v1::{BackendKind, Capability};
use contr_sdk::api::diagnostics::v1::{CheckStatus, RunChecksRequest, RunRemediationRequest};
use contr_sdk::api::engine::v1::{
    EngineState, ResetRequest, ResetScope, StartRequest, StopRequest, StreamUsageRequest, reset_response,
    start_response, stop_response,
};
use contr_sdk::api::images::v1::InspectImageRequest;
use contr_sdk::api::kubernetes::v1::{ClusterState, GetClusterRequest, ImportImagesRequest, SetCurrentContextRequest};
use contr_sdk::api::networks::v1::{CheckHostPortsRequest, HostPort};
use contr_sdk::api::settings::v1::{
    EngineSettings, GetSettingsRequest, ProxyMode, ProxySettings, RegistrySettings, ResetSettingsRequest, Settings,
    UpdateSettingsRequest,
};
use contr_sdk::{Client, ClientIdentity, Endpoint, Error};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

struct Daemon {
    endpoint: Endpoint,
    shutdown: Option<oneshot::Sender<()>>,
    task: JoinHandle<Result<(), contr_daemon::DaemonError>>,
    dir: tempfile::TempDir,
}

impl Daemon {
    fn start() -> Self {
        Self::start_in(tempfile::tempdir().unwrap())
    }

    /// Daemon whose settings and administrator's settings are `settings.json` and `admin-settings.json` in `dir`.
    fn start_in(dir: tempfile::TempDir) -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let instance = NEXT.fetch_add(1, Ordering::Relaxed);

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
            default_pipe: false,
            cli_dir: None,
            docker_context: false,
            settings_file: Some(dir.path().join("settings.json")),
            admin_settings: Some(dir.path().join("admin-settings.json")),
            compose: None,
            kubeconfig: Some(dir.path().join("kubeconfig")),
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

    drop(watch);
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
