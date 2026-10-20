use std::fs;
use std::io;
use std::path::Path;

/// Whether `path` is the mount point of a filesystem in this mount namespace.
pub(crate) fn is_mount_point(path: &Path) -> io::Result<bool> {
    let mount_info = fs::read_to_string("/proc/self/mountinfo")?;

    Ok(mount_points(&mount_info).any(|point| Path::new(&point) == path))
}

/// Mount points listed in `/proc/self/mountinfo`, with octal escapes such as `\040` decoded.
fn mount_points(mount_info: &str) -> impl Iterator<Item = String> + '_ {
    mount_info
        .lines()
        .filter_map(|line| line.split(' ').nth(4))
        .map(unescape)
}

fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let octal = bytes
            .get(index + 1..index + 4)
            .filter(|digits| digits.iter().all(u8::is_ascii_digit));
        match (bytes[index], octal) {
            (b'\\', Some(digits)) => {
                let value = digits
                    .iter()
                    .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                decoded.push(u8::try_from(value).unwrap_or(b'?'));
                index += 4;
            }
            (byte, _) => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MOUNT_INFO: &str = "\
22 1 8:48 / / rw,relatime - ext4 /dev/sdd rw
95 22 0:52 / /mnt/wsl rw,relatime shared:1 - tmpfs none rw
131 95 8:64 /var/lib/contr-data /mnt/wsl/contr/data rw,relatime shared:1 - ext4 /dev/sde rw
140 22 0:60 / /mnt/c/Program\\040Files rw - 9p drvfs rw";

    #[test]
    fn lists_mount_points_and_decodes_spaces() {
        let points: Vec<_> = mount_points(MOUNT_INFO).collect();

        assert_eq!(points, ["/", "/mnt/wsl", "/mnt/wsl/contr/data", "/mnt/c/Program Files"]);
    }
}
