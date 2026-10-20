//! Docker Engine API endpoints the fake engine serves, backed by mutable fixture data.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use http::{Method, Request, Response, StatusCode, header};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::{BodyExt, Full, StreamBody};
use hyper::body::{Frame, Incoming};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

use crate::Fixtures;

pub(crate) type Body = UnsyncBoxBody<Bytes, Infallible>;

const API_VERSION: &str = "1.51";
const STATS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);
const FAKE_CPUS: u64 = 4;
const FAKE_MEMORY: u64 = 8_589_934_592;
const EVENT_QUEUE: usize = 256;
const EVENT_HISTORY: usize = 1000;

pub(crate) struct Engine {
    store: Mutex<Fixtures>,
    history: Mutex<Vec<Value>>,
    events: broadcast::Sender<Value>,
    next_id: AtomicU64,
}

impl Engine {
    pub(crate) fn new(fixtures: Fixtures) -> Self {
        Self {
            store: Mutex::new(fixtures),
            history: Mutex::new(Vec::new()),
            events: broadcast::channel(EVENT_QUEUE).0,
            next_id: AtomicU64::new(1),
        }
    }

    fn new_id(&self) -> String {
        format!("fa{:062x}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    fn store(&self) -> MutexGuard<'_, Fixtures> {
        self.store.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, kind: &str, action: &str, id: &str, attributes: &Value) {
        let now = unix_now();
        let event = json!({
            "Type": kind,
            "Action": action,
            "Actor": { "ID": id, "Attributes": attributes },
            "scope": "local",
            "time": now,
            "timeNano": i128::from(now) * 1_000_000_000,
        });

        let mut history = self.history.lock().unwrap_or_else(PoisonError::into_inner);
        if history.len() == EVENT_HISTORY {
            history.remove(0);
        }
        history.push(event.clone());
        // No subscriber means nobody watches events; that is not an error.
        let _ = self.events.send(event);
    }

    /// Live events, preceded like the real engine's by the recorded ones at or after `since`.
    fn subscribe(&self, since: Option<i64>) -> (Vec<Value>, broadcast::Receiver<Value>) {
        let history = self.history.lock().unwrap_or_else(PoisonError::into_inner);
        let past = since.map_or_else(Vec::new, |since| {
            history
                .iter()
                .filter(|event| event["time"].as_i64().is_some_and(|time| time >= since))
                .cloned()
                .collect()
        });
        (past, self.events.subscribe())
    }
}

pub(crate) async fn route(engine: Arc<Engine>, request: Request<Incoming>) -> Response<Body> {
    let method = request.method().clone();
    let path = strip_api_version(request.uri().path()).to_owned();
    let query = parse_query(request.uri().query().unwrap_or_default());
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();

    match (&method, segments.as_slice()) {
        (&Method::GET | &Method::HEAD, ["_ping"]) => ping(),
        (&Method::GET, ["version"]) => ok_json(&version()),
        (&Method::GET, ["info"]) => ok_json(&info(&engine)),
        (&Method::GET, ["events"]) => events(&engine, query.get("since").and_then(|since| since.parse().ok())),
        (&Method::GET, ["containers", "json"]) => list_containers(&engine, &query),
        (&Method::GET, ["containers", id, "json"]) => inspect_container(&engine, id),
        (&Method::GET, ["containers", id, "stats"]) => stats(
            &engine,
            id,
            query
                .get("stream")
                .is_none_or(|stream| stream != "0" && stream != "false"),
        ),
        (&Method::POST, ["containers", "create"]) => match read_json(request).await {
            Ok(body) => create_container(&engine, query.get("name").map(String::as_str), &body),
            Err(message) => error(StatusCode::BAD_REQUEST, message),
        },
        (&Method::POST, ["containers", "prune"]) => prune_containers(&engine),
        (
            &Method::POST,
            [
                "containers",
                id,
                action @ ("start" | "stop" | "restart" | "pause" | "unpause" | "kill"),
            ],
        ) => change_container(&engine, id, action),
        (&Method::POST, ["containers", id, "rename"]) => {
            rename_container(&engine, id, query.get("name").map_or("", String::as_str))
        }
        (&Method::DELETE, ["containers", id]) => remove_container(&engine, id, flag(&query, "force")),
        (&Method::GET, ["images", "json"]) => ok_json(&Value::from(engine.store().images.clone())),
        (&Method::GET, ["volumes"]) => ok_json(&json!({ "Volumes": engine.store().volumes, "Warnings": [] })),
        (&Method::POST, ["volumes", "create"]) => match read_json(request).await {
            Ok(body) => create_volume(&engine, &body),
            Err(message) => error(StatusCode::BAD_REQUEST, message),
        },
        (&Method::GET, ["volumes", name]) => find(&engine.store().volumes, "Name", name).map_or_else(
            || error(StatusCode::NOT_FOUND, &format!("get {name}: no such volume")),
            |volume| ok_json(&volume),
        ),
        (&Method::DELETE, ["volumes", name]) => remove_volume(&engine, name),
        (&Method::GET, ["networks"]) => ok_json(&Value::from(engine.store().networks.clone())),
        (&Method::POST, ["networks", "create"]) => match read_json(request).await {
            Ok(body) => create_network(&engine, &body),
            Err(message) => error(StatusCode::BAD_REQUEST, message),
        },
        (&Method::GET, ["networks", id]) => find_network(&engine.store().networks, id).map_or_else(
            || error(StatusCode::NOT_FOUND, &format!("network {id} not found")),
            |network| ok_json(&network),
        ),
        (&Method::DELETE, ["networks", id]) => remove_network(&engine, id),
        _ => error(StatusCode::NOT_FOUND, "page not found"),
    }
}

fn ping() -> Response<Body> {
    Response::builder()
        .header("Api-Version", API_VERSION)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(full(Bytes::from_static(b"OK")))
        .unwrap_or_default()
}

fn version() -> Value {
    json!({
        "Version": "29.0.0-fake",
        "ApiVersion": API_VERSION,
        "MinAPIVersion": "1.24",
        "Os": "linux",
        "Arch": "amd64",
        "KernelVersion": "fake",
        "GitCommit": "fake",
        "GoVersion": "fake",
    })
}

fn info(engine: &Engine) -> Value {
    let store = engine.store();
    let running = store.containers.iter().filter(|c| c["State"] == "running").count();
    json!({
        "ID": "fake",
        "Name": "fake-engine",
        "Containers": store.containers.len(),
        "ContainersRunning": running,
        "Images": store.images.len(),
        "OperatingSystem": "Contr fake engine",
        "NCPU": FAKE_CPUS,
        "MemTotal": FAKE_MEMORY,
        "SecurityOptions": ["name=seccomp,profile=builtin"],
    })
}

fn events(engine: &Engine, since: Option<i64>) -> Response<Body> {
    let (past, live) = engine.subscribe(since);
    let frames = futures_util::stream::iter(past)
        .chain(BroadcastStream::new(live).filter_map(|event| std::future::ready(event.ok())))
        .map(|event| Ok(Frame::data(Bytes::from(format!("{event}\n")))));

    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(BodyExt::boxed_unsync(StreamBody::new(frames)))
        .unwrap_or_default()
}

fn list_containers(engine: &Engine, query: &HashMap<String, String>) -> Response<Body> {
    let all = flag(query, "all");
    let ids = query
        .get("filters")
        .and_then(|filters| serde_json::from_str::<HashMap<String, Vec<String>>>(filters).ok())
        .and_then(|mut filters| filters.remove("id"));

    let containers: Vec<Value> = engine
        .store()
        .containers
        .iter()
        .filter(|container| all || container["State"] == "running")
        .filter(|container| {
            let id = container["Id"].as_str().unwrap_or_default();
            ids.as_ref()
                .is_none_or(|ids| ids.iter().any(|wanted| id.starts_with(wanted.as_str())))
        })
        .cloned()
        .collect();

    ok_json(&Value::from(containers))
}

fn inspect_container(engine: &Engine, reference: &str) -> Response<Body> {
    let store = engine.store();
    let Some(container) = find_container(&store.containers, reference) else {
        return no_such_container(reference);
    };

    let state = container["State"].as_str().unwrap_or_default();
    let created = rfc3339(container["Created"].as_i64().unwrap_or_default());
    let name = container["Names"][0].as_str().unwrap_or_default();

    ok_json(&json!({
        "Id": container["Id"],
        "Created": created,
        "Path": container["Command"],
        "Args": [],
        "State": {
            "Status": state,
            "Running": state == "running",
            "Paused": state == "paused",
            "Restarting": false,
            "OOMKilled": false,
            "Dead": false,
            "Pid": 0,
            "ExitCode": 0,
            "Error": "",
            "StartedAt": created,
            "FinishedAt": "0001-01-01T00:00:00Z",
        },
        "Image": container["ImageID"],
        "Name": name,
        "RestartCount": 0,
        "Config": {
            "Image": container["Image"],
            "Tty": false,
            "Env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
            "WorkingDir": "",
            "Labels": container["Labels"],
        },
        "Mounts": container.get("Mounts").cloned().unwrap_or_else(|| json!([])),
    }))
}

/// Samples in which a running container uses a quarter of one CPU and 50 MiB, one per second while `stream` is set.
fn stats(engine: &Engine, reference: &str, stream: bool) -> Response<Body> {
    let (id, name, running) = {
        let store = engine.store();
        let Some(container) = find_container(&store.containers, reference) else {
            return no_such_container(reference);
        };
        (
            container["Id"].clone(),
            container["Names"][0].clone(),
            container["State"] == "running",
        )
    };
    let sample = move |tick: u64| {
        let cpu = |tick: u64| if running { tick * 250_000_000 } else { 0 };
        let system = |tick: u64| tick * FAKE_CPUS * 1_000_000_000;
        json!({
            "id": id,
            "name": name,
            "read": rfc3339(unix_now()),
            "cpu_stats": {
                "cpu_usage": { "total_usage": cpu(tick) },
                "system_cpu_usage": system(tick),
                "online_cpus": FAKE_CPUS,
            },
            "precpu_stats": {
                "cpu_usage": { "total_usage": cpu(tick - 1) },
                "system_cpu_usage": system(tick - 1),
            },
            "memory_stats": {
                "usage": if running { 52_428_800_u64 } else { 0 },
                "limit": FAKE_MEMORY,
                "stats": { "inactive_file": 0 },
            },
            "pids_stats": { "current": u64::from(running) * 3 },
            "networks": { "eth0": { "rx_bytes": tick * 1024, "tx_bytes": tick * 512 } },
        })
    };

    if !stream {
        return ok_json(&sample(1));
    }
    let frames = futures_util::stream::unfold(1_u64, move |tick| {
        let frame = Bytes::from(format!("{}\n", sample(tick)));
        async move {
            if tick > 1 {
                tokio::time::sleep(STATS_INTERVAL).await;
            }
            Some((Ok(Frame::data(frame)), tick + 1))
        }
    });
    Response::builder()
        .header(header::CONTENT_TYPE, "application/json")
        .body(BodyExt::boxed_unsync(StreamBody::new(frames)))
        .unwrap_or_default()
}

fn change_container(engine: &Engine, reference: &str, action: &str) -> Response<Body> {
    let (id, name, events) = {
        let mut store = engine.store();
        let Some(container) = find_container_mut(&mut store.containers, reference) else {
            return no_such_container(reference);
        };

        let running = container["State"] == "running";
        let paused = container["State"] == "paused";
        let (state, status, events): (&str, &str, &[&str]) = match action {
            "start" if running => return empty(StatusCode::NOT_MODIFIED),
            "stop" if !running => return empty(StatusCode::NOT_MODIFIED),
            "pause" | "kill" if !running => {
                return error(StatusCode::CONFLICT, &format!("container {reference} is not running"));
            }
            "unpause" if !paused => {
                return error(StatusCode::CONFLICT, &format!("container {reference} is not paused"));
            }
            "start" => ("running", "Up Less than a second", &["start"]),
            "unpause" => ("running", "Up Less than a second", &["unpause"]),
            "stop" => ("exited", "Exited (0) Less than a second ago", &["die", "stop"]),
            "pause" => ("paused", "Up Less than a second (Paused)", &["pause"]),
            "kill" => ("exited", "Exited (137) Less than a second ago", &["kill", "die"]),
            _ => ("running", "Up Less than a second", &["die", "stop", "start", "restart"]),
        };
        container["State"] = state.into();
        container["Status"] = status.into();

        (container["Id"].clone(), container["Names"][0].clone(), events)
    };

    let id = id.as_str().unwrap_or_default();
    let attributes = json!({ "name": name.as_str().unwrap_or_default().trim_start_matches('/') });
    for action in events {
        engine.emit("container", action, id, &attributes);
    }

    empty(StatusCode::NO_CONTENT)
}

fn remove_container(engine: &Engine, reference: &str, force: bool) -> Response<Body> {
    let removed = {
        let mut store = engine.store();
        let Some(index) = position(&store.containers, |c| matches_container(c, reference)) else {
            return no_such_container(reference);
        };
        if store.containers[index]["State"] == "running" && !force {
            let message = format!(
                "cannot remove container {reference}: container is running: stop the container before removing or force remove"
            );
            return error(StatusCode::CONFLICT, &message);
        }
        store.containers.remove(index)
    };

    let id = removed["Id"].as_str().unwrap_or_default();
    engine.emit("container", "destroy", id, &json!({}));
    empty(StatusCode::NO_CONTENT)
}

fn create_container(engine: &Engine, name: Option<&str>, body: &Value) -> Response<Body> {
    let Some(image) = body["Image"].as_str().filter(|image| !image.is_empty()) else {
        return error(
            StatusCode::BAD_REQUEST,
            "config cannot be empty in order to create a container",
        );
    };
    let id = engine.new_id();
    let name = name
        .filter(|name| !name.is_empty())
        .map_or_else(|| format!("fake_{}", &id[..12]), str::to_owned);

    {
        let mut store = engine.store();
        let image_id = store.images.iter().find_map(|candidate| {
            let tagged = candidate["RepoTags"]
                .as_array()
                .is_some_and(|tags| tags.iter().any(|tag| tag == image));
            (tagged || candidate["Id"] == image).then(|| candidate["Id"].clone())
        });
        let Some(image_id) = image_id else {
            return error(StatusCode::NOT_FOUND, &format!("No such image: {image}"));
        };
        if find_container(&store.containers, &name).is_some() {
            let message = format!("Conflict. The container name \"/{name}\" is already in use");
            return error(StatusCode::CONFLICT, &message);
        }
        store.containers.push(json!({
            "Id": id,
            "Names": [format!("/{name}")],
            "Image": image,
            "ImageID": image_id,
            "Command": body["Cmd"].as_array().map(|cmd| {
                cmd.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(" ")
            }).unwrap_or_default(),
            "Created": unix_now(),
            "Ports": [],
            "Labels": body.get("Labels").cloned().filter(Value::is_object).unwrap_or_else(|| json!({})),
            "State": "created",
            "Status": "Created",
        }));
    }

    engine.emit("container", "create", &id, &json!({ "name": name }));
    json_response(StatusCode::CREATED, &json!({ "Id": id, "Warnings": [] }))
}

/// Removes every container that is neither running nor paused.
fn prune_containers(engine: &Engine) -> Response<Body> {
    let deleted: Vec<Value> = {
        let mut store = engine.store();
        let (kept, deleted) = std::mem::take(&mut store.containers)
            .into_iter()
            .partition(|container| container["State"] == "running" || container["State"] == "paused");
        store.containers = kept;
        deleted
    };

    let ids: Vec<&str> = deleted
        .iter()
        .filter_map(|container| container["Id"].as_str())
        .collect();
    for id in &ids {
        engine.emit("container", "destroy", id, &json!({}));
    }
    ok_json(&json!({ "ContainersDeleted": ids, "SpaceReclaimed": 0 }))
}

fn rename_container(engine: &Engine, reference: &str, name: &str) -> Response<Body> {
    if name.is_empty() {
        return error(StatusCode::BAD_REQUEST, "Neither old nor new names may be empty");
    }
    let id = {
        let mut store = engine.store();
        if find_container(&store.containers, name).is_some() {
            let message = format!("Conflict. The container name \"/{name}\" is already in use");
            return error(StatusCode::CONFLICT, &message);
        }
        let Some(container) = find_container_mut(&mut store.containers, reference) else {
            return no_such_container(reference);
        };
        container["Names"] = json!([format!("/{name}")]);
        container["Id"].clone()
    };

    engine.emit(
        "container",
        "rename",
        id.as_str().unwrap_or_default(),
        &json!({ "name": name }),
    );
    empty(StatusCode::NO_CONTENT)
}

fn create_volume(engine: &Engine, body: &Value) -> Response<Body> {
    let name = body["Name"]
        .as_str()
        .filter(|name| !name.is_empty())
        .map_or_else(|| engine.new_id(), str::to_owned);
    let driver = body["Driver"]
        .as_str()
        .filter(|driver| !driver.is_empty())
        .unwrap_or("local");

    let volume = json!({
        "Name": name,
        "Driver": driver,
        "Mountpoint": format!("/var/lib/docker/volumes/{name}/_data"),
        "CreatedAt": rfc3339(unix_now()),
        "Labels": body.get("Labels").cloned().unwrap_or_else(|| json!({})),
        "Scope": "local",
        "Options": body.get("DriverOpts").cloned().unwrap_or_else(|| json!({})),
    });

    {
        let mut store = engine.store();
        if let Some(existing) = find(&store.volumes, "Name", &name) {
            return json_response(StatusCode::CREATED, &existing);
        }
        store.volumes.push(volume.clone());
    }

    engine.emit("volume", "create", &name, &json!({ "driver": driver }));
    json_response(StatusCode::CREATED, &volume)
}

fn remove_volume(engine: &Engine, name: &str) -> Response<Body> {
    {
        let mut store = engine.store();
        let Some(index) = position(&store.volumes, |v| v["Name"] == name) else {
            return error(StatusCode::NOT_FOUND, &format!("get {name}: no such volume"));
        };
        store.volumes.remove(index);
    }

    engine.emit("volume", "destroy", name, &json!({}));
    empty(StatusCode::NO_CONTENT)
}

fn create_network(engine: &Engine, body: &Value) -> Response<Body> {
    let Some(name) = body["Name"].as_str().filter(|name| !name.is_empty()) else {
        return error(StatusCode::BAD_REQUEST, "network name is required");
    };
    let id = engine.new_id();
    let network = json!({
        "Name": name,
        "Id": id,
        "Created": rfc3339(unix_now()),
        "Scope": "local",
        "Driver": body["Driver"].as_str().filter(|driver| !driver.is_empty()).unwrap_or("bridge"),
        "EnableIPv4": true,
        "EnableIPv6": body["EnableIPv6"].as_bool().unwrap_or_default(),
        "Internal": body["Internal"].as_bool().unwrap_or_default(),
        "Attachable": body["Attachable"].as_bool().unwrap_or_default(),
        "Ingress": false,
        "IPAM": body.get("IPAM").cloned().unwrap_or_else(|| json!({ "Driver": "default", "Config": [] })),
        "Options": {},
        "Labels": body.get("Labels").cloned().unwrap_or_else(|| json!({})),
    });

    {
        let mut store = engine.store();
        if find(&store.networks, "Name", name).is_some() {
            return error(
                StatusCode::CONFLICT,
                &format!("network with name {name} already exists"),
            );
        }
        store.networks.push(network);
    }

    engine.emit("network", "create", &id, &json!({ "name": name, "type": "bridge" }));
    json_response(StatusCode::CREATED, &json!({ "Id": id, "Warning": "" }))
}

fn remove_network(engine: &Engine, reference: &str) -> Response<Body> {
    let removed = {
        let mut store = engine.store();
        let Some(index) = position(&store.networks, |n| matches_network(n, reference)) else {
            return error(StatusCode::NOT_FOUND, &format!("network {reference} not found"));
        };
        store.networks.remove(index)
    };

    let id = removed["Id"].as_str().unwrap_or_default();
    engine.emit("network", "destroy", id, &json!({ "name": removed["Name"] }));
    empty(StatusCode::NO_CONTENT)
}

fn find_container<'a>(containers: &'a [Value], reference: &str) -> Option<&'a Value> {
    containers
        .iter()
        .find(|container| matches_container(container, reference))
}

fn find_container_mut<'a>(containers: &'a mut [Value], reference: &str) -> Option<&'a mut Value> {
    containers
        .iter_mut()
        .find(|container| matches_container(container, reference))
}

fn matches_container(container: &Value, reference: &str) -> bool {
    let id_matches = container["Id"]
        .as_str()
        .is_some_and(|id| !reference.is_empty() && id.starts_with(reference));
    let name_matches = container["Names"]
        .as_array()
        .is_some_and(|names| names.iter().any(|name| name.as_str() == Some(&format!("/{reference}"))));
    id_matches || name_matches
}

fn find_network(networks: &[Value], reference: &str) -> Option<Value> {
    networks
        .iter()
        .find(|network| matches_network(network, reference))
        .cloned()
}

fn matches_network(network: &Value, reference: &str) -> bool {
    let id_matches = network["Id"]
        .as_str()
        .is_some_and(|id| !reference.is_empty() && id.starts_with(reference));
    id_matches || network["Name"] == reference
}

fn find(items: &[Value], key: &str, value: &str) -> Option<Value> {
    items.iter().find(|item| item[key] == value).cloned()
}

fn position(items: &[Value], predicate: impl Fn(&Value) -> bool) -> Option<usize> {
    items.iter().position(predicate)
}

fn no_such_container(reference: &str) -> Response<Body> {
    error(StatusCode::NOT_FOUND, &format!("No such container: {reference}"))
}

async fn read_json(request: Request<Incoming>) -> Result<Value, &'static str> {
    let bytes = request
        .into_body()
        .collect()
        .await
        .map_err(|_| "cannot read the request body")?
        .to_bytes();
    serde_json::from_slice(&bytes).map_err(|_| "the request body is not JSON")
}

fn ok_json(value: &Value) -> Response<Body> {
    json_response(StatusCode::OK, value)
}

fn json_response(status: StatusCode, value: &Value) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "application/json")
        .body(full(Bytes::from(value.to_string())))
        .unwrap_or_default()
}

fn error(status: StatusCode, message: &str) -> Response<Body> {
    json_response(status, &json!({ "message": message }))
}

fn empty(status: StatusCode) -> Response<Body> {
    Response::builder()
        .status(status)
        .body(full(Bytes::new()))
        .unwrap_or_default()
}

fn full(bytes: Bytes) -> Body {
    Full::new(bytes).boxed_unsync()
}

fn flag(query: &HashMap<String, String>, key: &str) -> bool {
    query.get(key).is_some_and(|value| value == "1" || value == "true")
}

fn strip_api_version(path: &str) -> &str {
    let Some(rest) = path.strip_prefix("/v") else {
        return path;
    };
    let (version, remainder) = rest.split_once('/').unwrap_or((rest, ""));
    if !version.is_empty() && version.chars().all(|c| c.is_ascii_digit() || c == '.') {
        &path[path.len() - remainder.len() - 1..]
    } else {
        path
    }
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(key), percent_decode(value))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;

    while index < bytes.len() {
        let hex = bytes.get(index + 1..index + 3).and_then(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
        });
        match (bytes[index], hex) {
            (b'%', Some(byte)) => {
                decoded.push(byte);
                index += 3;
            }
            (b'+', _) => {
                decoded.push(b' ');
                index += 1;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
}

/// Formats Unix seconds as RFC 3339 in UTC with Howard Hinnant's `civil_from_days` algorithm.
fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let time = seconds.rem_euclid(86_400);

    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);

    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time % 3600 / 60,
        time % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_the_api_version_prefix() {
        assert_eq!(strip_api_version("/v1.51/containers/json"), "/containers/json");
        assert_eq!(strip_api_version("/_ping"), "/_ping");
        assert_eq!(strip_api_version("/volumes"), "/volumes");
    }

    #[test]
    fn decodes_query_values() {
        let query = parse_query("all=1&filters=%7B%22id%22%3A%5B%22c0%22%5D%7D");

        assert_eq!(query["all"], "1");
        assert_eq!(query["filters"], r#"{"id":["c0"]}"#);
    }

    #[test]
    fn formats_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_760_000_000), "2025-10-09T08:53:20Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
    }
}
