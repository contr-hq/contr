//! Resources the whole engine uses: the CPU and memory of its running containers, summed from one stats stream per
//! container, and the disk its data takes. Sampling runs only while a watcher exists.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use contr_api::containers::v1::ContainerStats;
use contr_api::engine::v1::EngineUsage;
use futures_util::StreamExt;
use futures_util::stream::{self, BoxStream};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{self, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::Engine;

const SAMPLE_INTERVAL: Duration = Duration::from_secs(2);
const DISK_INTERVAL: Duration = Duration::from_secs(60);

/// IDs of the containers that currently run.
pub(crate) type RunningIds = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

type Samples = Arc<Mutex<HashMap<String, ContainerStats>>>;

/// The latest usage and whether a sampling task produces it.
#[derive(Default)]
pub(crate) struct Usage {
    latest: watch::Sender<Option<EngineUsage>>,
    sampling: Mutex<bool>,
}

impl Usage {
    /// Usage every two seconds until `cancel` fires, starting the sampling task when none runs.
    pub(crate) fn watch(
        self: &Arc<Self>,
        engine: &Engine,
        running: RunningIds,
        cancel: CancellationToken,
    ) -> BoxStream<'static, EngineUsage> {
        let receiver = {
            let mut sampling = lock(&self.sampling);
            let receiver = self.latest.subscribe();
            if !*sampling {
                *sampling = true;
                tokio::spawn(sample(Arc::clone(self), engine.clone(), running, cancel.clone()));
            }
            receiver
        };

        stream::unfold(receiver, |mut receiver| async move {
            receiver.changed().await.ok()?;
            let usage = (*receiver.borrow_and_update())?;
            Some((usage, receiver))
        })
        .take_until(cancel.cancelled_owned())
        .boxed()
    }
}

/// Publishes usage until no watcher is left or `cancel` fires; the watcher check and [`Usage::watch`] share a lock,
/// so a new watcher either keeps this task alive or starts the next one.
async fn sample(usage: Arc<Usage>, engine: Engine, running: RunningIds, cancel: CancellationToken) {
    let (cpus, memory_total_bytes) = engine.capacity().await.unwrap_or_else(|error| {
        tracing::debug!(%error, "cannot read the engine's capacity");
        (0, 0)
    });
    let disk = Arc::new(AtomicU64::new(0));
    let disk_task = tokio::spawn(measure_disk(engine.clone(), Arc::clone(&disk)));
    let samples = Samples::default();
    let mut streams: HashMap<String, JoinHandle<()>> = HashMap::new();
    let mut ticks = time::interval(SAMPLE_INTERVAL);
    ticks.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            _ = ticks.tick() => {}
            () = cancel.cancelled() => {
                *lock(&usage.sampling) = false;
                break;
            }
        }

        let ids = running();
        streams.retain(|id, task| {
            let keep = ids.contains(id) && !task.is_finished();
            if !keep {
                task.abort();
                lock(&samples).remove(id);
            }
            keep
        });
        for id in &ids {
            streams
                .entry(id.clone())
                .or_insert_with(|| tokio::spawn(follow(engine.clone(), id.clone(), Arc::clone(&samples))));
        }

        let (cpu_percent, memory_used_bytes) = lock(&samples).values().fold((0.0, 0), |(cpu, memory), stats| {
            (cpu + stats.cpu_percent, memory + stats.memory_usage_bytes)
        });
        usage.latest.send_replace(Some(EngineUsage {
            cpu_percent,
            cpus,
            memory_used_bytes,
            memory_total_bytes,
            disk_used_bytes: disk.load(Ordering::Relaxed),
            running_containers: u32::try_from(ids.len()).unwrap_or(u32::MAX),
        }));

        let mut sampling = lock(&usage.sampling);
        if usage.latest.receiver_count() == 0 {
            *sampling = false;
            break;
        }
    }

    disk_task.abort();
    for task in streams.into_values() {
        task.abort();
    }
}

/// Keeps the latest sample of container `id` in `samples` while its stats stream lasts.
async fn follow(engine: Engine, id: String, samples: Samples) {
    let mut stats = engine.stats(&id, false);
    while let Some(Ok(sample)) = stats.next().await {
        lock(&samples).insert(id.clone(), sample);
    }
    lock(&samples).remove(&id);
}

async fn measure_disk(engine: Engine, disk: Arc<AtomicU64>) {
    loop {
        match engine.disk_usage().await {
            Ok(bytes) => disk.store(bytes, Ordering::Relaxed),
            Err(error) => tracing::debug!(%error, "cannot measure the engine's disk usage"),
        }
        time::sleep(DISK_INTERVAL).await;
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
