//! Text formatting that matches what the Docker CLI prints for the same data.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use contr_sdk::api::containers::v1::{PortMapping, Protocol};
use prost_types::Timestamp;

/// Left-aligned columns separated by three spaces, like the Docker CLI's tables.
pub(crate) fn table(header: &[&str], rows: &[Vec<String>]) -> String {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }

    let header: Vec<String> = header.iter().map(|&cell| cell.to_owned()).collect();
    std::iter::once(&header)
        .chain(rows)
        .map(|row| {
            let last = row.len().saturating_sub(1);
            row.iter()
                .zip(&widths)
                .enumerate()
                .map(|(index, (cell, width))| {
                    if index == last {
                        cell.clone()
                    } else {
                        format!("{cell:<width$}   ")
                    }
                })
                .collect::<String>()
        })
        .map(|line| line + "\n")
        .collect()
}

pub(crate) fn short_id(id: &str) -> &str {
    let id = id.strip_prefix("sha256:").unwrap_or(id);
    id.get(..12).unwrap_or(id)
}

/// Quoted and cut to 20 characters, as `docker ps` shows commands.
pub(crate) fn command(command: &str) -> String {
    const LIMIT: usize = 20;
    if command.chars().count() > LIMIT {
        let cut: String = command.chars().take(LIMIT - 1).collect();
        format!("\"{cut}…\"")
    } else {
        format!("\"{command}\"")
    }
}

pub(crate) fn ports(ports: &[PortMapping]) -> String {
    ports
        .iter()
        .map(|port| {
            let protocol = match port.protocol() {
                Protocol::Udp => "udp",
                Protocol::Sctp => "sctp",
                Protocol::Tcp | Protocol::Unspecified => "tcp",
            };
            if port.host_port == 0 {
                format!("{}/{protocol}", port.container_port)
            } else if port.host_ip.contains(':') {
                format!(
                    "[{}]:{}->{}/{protocol}",
                    port.host_ip, port.host_port, port.container_port
                )
            } else {
                format!(
                    "{}:{}->{}/{protocol}",
                    port.host_ip, port.host_port, port.container_port
                )
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

pub(crate) fn ago(timestamp: Option<&Timestamp>) -> String {
    let Some(timestamp) = timestamp else {
        return String::new();
    };
    let then = UNIX_EPOCH + Duration::from_secs(u64::try_from(timestamp.seconds).unwrap_or_default());
    let elapsed = SystemTime::now().duration_since(then).unwrap_or_default();
    format!("{} ago", human_duration(elapsed))
}

/// Port of `HumanDuration` from docker/go-units (Apache-2.0).
fn human_duration(duration: Duration) -> String {
    let seconds = duration.as_secs();
    let minutes = seconds / 60;
    let hours = (seconds + 1800) / 3600;

    match seconds {
        0 => "Less than a second".to_owned(),
        1 => "1 second".to_owned(),
        2..60 => format!("{seconds} seconds"),
        _ if minutes == 1 => "About a minute".to_owned(),
        _ if minutes < 60 => format!("{minutes} minutes"),
        _ if hours == 1 => "About an hour".to_owned(),
        _ if hours < 48 => format!("{hours} hours"),
        _ if hours < 24 * 7 * 2 => format!("{} days", hours / 24),
        _ if hours < 24 * 30 * 2 => format!("{} weeks", hours / 24 / 7),
        _ if hours < 24 * 365 * 2 => format!("{} months", hours / 24 / 30),
        _ => format!("{} years", seconds / 3600 / 24 / 365),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_like_docker() {
        let cases = [
            (0, "Less than a second"),
            (1, "1 second"),
            (59, "59 seconds"),
            (60, "About a minute"),
            (3599, "59 minutes"),
            (3600, "About an hour"),
            (7 * 3600, "7 hours"),
            (3 * 86_400, "3 days"),
            (21 * 86_400, "3 weeks"),
            (90 * 86_400, "3 months"),
            (3 * 365 * 86_400, "3 years"),
        ];
        for (seconds, expected) in cases {
            assert_eq!(human_duration(Duration::from_secs(seconds)), expected, "{seconds}");
        }
    }

    #[test]
    fn formats_ports_like_docker_ps() {
        let published = PortMapping {
            container_port: 80,
            host_port: 8080,
            host_ip: "0.0.0.0".to_owned(),
            protocol: Protocol::Tcp.into(),
        };
        let exposed = PortMapping {
            container_port: 53,
            protocol: Protocol::Udp.into(),
            ..Default::default()
        };

        assert_eq!(ports(&[published, exposed]), "0.0.0.0:8080->80/tcp, 53/udp");
    }

    #[test]
    fn quotes_and_cuts_commands() {
        assert_eq!(command("sh"), "\"sh\"");
        assert_eq!(command("/docker-entrypoint.sh nginx"), "\"/docker-entrypoint.…\"");
    }

    #[test]
    fn aligns_table_columns() {
        let rows = vec![vec!["a".to_owned(), "long value".to_owned(), "x".to_owned()]];

        assert_eq!(
            table(&["ONE", "TWO", "THREE"], &rows),
            "ONE   TWO          THREE\na     long value   x\n"
        );
    }
}
