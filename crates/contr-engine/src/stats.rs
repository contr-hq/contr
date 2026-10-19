//! Resource usage computed from engine stats samples with the formulas of the Docker CLI's `docker stats`
//! (docker/cli `cli/command/container/stats_helpers.go`, Apache-2.0), so both report the same numbers.

use bollard::models::{
    ContainerBlkioStats, ContainerCpuStats, ContainerMemoryStats, ContainerNetworkStats, ContainerStatsResponse,
};
use contr_api::containers::v1::ContainerStats;

use crate::convert::rfc3339;

pub(crate) fn sample(stats: &ContainerStatsResponse) -> ContainerStats {
    let memory_usage = stats.memory_stats.as_ref().map_or(0, memory_usage);
    let memory_limit = stats
        .memory_stats
        .as_ref()
        .and_then(|memory| memory.limit)
        .unwrap_or_default();
    let (block_read, block_write) = stats.blkio_stats.as_ref().map_or((0, 0), block_io);
    let (network_rx, network_tx) = stats
        .networks
        .as_ref()
        .map_or((0, 0), |networks| network_io(networks.values()));

    ContainerStats {
        read_at: stats.read.as_deref().and_then(rfc3339),
        cpu_percent: cpu_percent(stats.cpu_stats.as_ref(), stats.precpu_stats.as_ref()),
        memory_usage_bytes: memory_usage,
        memory_limit_bytes: memory_limit,
        memory_percent: percent(memory_usage, memory_limit),
        network_rx_bytes: network_rx,
        network_tx_bytes: network_tx,
        block_read_bytes: block_read,
        block_write_bytes: block_write,
        pids: stats
            .pids_stats
            .as_ref()
            .and_then(|pids| pids.current)
            .unwrap_or_default(),
    }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the Docker CLI computes these percentages in float64 as well"
)]
fn cpu_percent(current: Option<&ContainerCpuStats>, previous: Option<&ContainerCpuStats>) -> f64 {
    let total = |stats: Option<&ContainerCpuStats>| {
        stats
            .and_then(|stats| stats.cpu_usage.as_ref())
            .and_then(|usage| usage.total_usage)
            .unwrap_or_default() as f64
    };
    let system =
        |stats: Option<&ContainerCpuStats>| stats.and_then(|stats| stats.system_cpu_usage).unwrap_or_default() as f64;

    let cpu_delta = total(current) - total(previous);
    let system_delta = system(current) - system(previous);
    let per_cpu_count = || {
        current
            .and_then(|stats| stats.cpu_usage.as_ref())
            .and_then(|usage| usage.percpu_usage.as_ref())
            .map_or(0.0, |per_cpu| per_cpu.len() as f64)
    };
    let online_cpus = current
        .and_then(|stats| stats.online_cpus)
        .filter(|&cpus| cpus > 0)
        .map_or_else(per_cpu_count, f64::from);

    if cpu_delta > 0.0 && system_delta > 0.0 {
        cpu_delta / system_delta * online_cpus * 100.0
    } else {
        0.0
    }
}

/// Usage minus inactive page cache: `total_inactive_file` on cgroup v1, `inactive_file` on cgroup v2.
fn memory_usage(memory: &ContainerMemoryStats) -> u64 {
    let usage = memory.usage.unwrap_or_default();
    let stat = |key: &str| memory.stats.as_ref().and_then(|stats| stats.get(key)).copied();

    if let Some(inactive) = stat("total_inactive_file").filter(|&inactive| inactive < usage) {
        return usage - inactive;
    }
    let inactive = stat("inactive_file").unwrap_or_default();
    if inactive < usage { usage - inactive } else { usage }
}

#[expect(
    clippy::cast_precision_loss,
    reason = "the Docker CLI computes these percentages in float64 as well"
)]
fn percent(used: u64, limit: u64) -> f64 {
    if limit == 0 {
        0.0
    } else {
        used as f64 / limit as f64 * 100.0
    }
}

fn block_io(blkio: &ContainerBlkioStats) -> (u64, u64) {
    let entries = blkio.io_service_bytes_recursive.iter().flatten();

    entries.fold((0, 0), |(read, write), entry| {
        let value = entry.value.unwrap_or_default();
        match entry.op.as_deref().and_then(|op| op.chars().next()) {
            Some('r' | 'R') => (read + value, write),
            Some('w' | 'W') => (read, write + value),
            _ => (read, write),
        }
    })
}

fn network_io<'a>(networks: impl Iterator<Item = &'a ContainerNetworkStats>) -> (u64, u64) {
    networks.fold((0, 0), |(rx, tx), network| {
        (
            rx + network.rx_bytes.unwrap_or_default(),
            tx + network.tx_bytes.unwrap_or_default(),
        )
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stats(value: serde_json::Value) -> ContainerStatsResponse {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn matches_docker_stats_on_cgroup_v2() {
        let sample = sample(&stats(json!({
            "read": "2025-10-09T08:53:21Z",
            "cpu_stats": {
                "cpu_usage": { "total_usage": 2_000_000_000u64 },
                "system_cpu_usage": 20_000_000_000u64,
                "online_cpus": 4,
            },
            "precpu_stats": {
                "cpu_usage": { "total_usage": 1_000_000_000u64 },
                "system_cpu_usage": 10_000_000_000u64,
            },
            "memory_stats": {
                "usage": 100 * 1024 * 1024,
                "limit": 1024 * 1024 * 1024,
                "stats": { "inactive_file": 20 * 1024 * 1024, "active_file": 5 },
            },
            "blkio_stats": {
                "io_service_bytes_recursive": [
                    { "major": 8, "minor": 0, "op": "read", "value": 4096 },
                    { "major": 8, "minor": 0, "op": "write", "value": 8192 },
                    { "major": 8, "minor": 0, "op": "discard", "value": 1 },
                ],
            },
            "networks": {
                "eth0": { "rx_bytes": 100, "tx_bytes": 200 },
                "eth1": { "rx_bytes": 10, "tx_bytes": 20 },
            },
            "pids_stats": { "current": 7 },
        })));

        assert!((sample.cpu_percent - 40.0).abs() < 1e-9);
        assert_eq!(sample.memory_usage_bytes, 80 * 1024 * 1024);
        assert!((sample.memory_percent - 7.8125).abs() < 1e-9);
        assert_eq!((sample.block_read_bytes, sample.block_write_bytes), (4096, 8192));
        assert_eq!((sample.network_rx_bytes, sample.network_tx_bytes), (110, 220));
        assert_eq!(sample.pids, 7);
        assert_eq!(sample.read_at.map(|t| t.seconds), Some(1_760_000_001));
    }

    #[test]
    fn uses_total_inactive_file_and_per_cpu_count_on_cgroup_v1() {
        let sample = sample(&stats(json!({
            "cpu_stats": {
                "cpu_usage": { "total_usage": 300, "percpu_usage": [150, 150] },
                "system_cpu_usage": 1_000,
            },
            "precpu_stats": { "cpu_usage": { "total_usage": 200 }, "system_cpu_usage": 800 },
            "memory_stats": {
                "usage": 1000,
                "limit": 4000,
                "stats": { "total_inactive_file": 200, "inactive_file": 900 },
            },
        })));

        assert!((sample.cpu_percent - 100.0).abs() < 1e-9);
        assert_eq!(sample.memory_usage_bytes, 800);
        assert!((sample.memory_percent - 20.0).abs() < 1e-9);
    }

    #[test]
    fn reports_zero_for_a_stopped_container() {
        let sample = sample(&stats(json!({
            "cpu_stats": { "cpu_usage": { "total_usage": 0 }, "system_cpu_usage": 0 },
            "precpu_stats": {},
            "memory_stats": {},
        })));

        assert!(sample.cpu_percent.abs() < f64::EPSILON);
        assert!(sample.memory_percent.abs() < f64::EPSILON);
    }
}
