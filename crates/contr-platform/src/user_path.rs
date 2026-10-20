//! The `Path` a new process gets on Windows: the system value followed by the user's, both read from the registry,
//! where the user's value may hold unexpanded `%VARIABLE%` references.

use std::io;
use std::path::{Path, PathBuf};

use winreg::RegKey;
use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, KEY_SET_VALUE, REG_EXPAND_SZ, REG_SZ};
use winreg::{RegValue, types::FromRegValue};

const USER_ENVIRONMENT: &str = "Environment";
const SYSTEM_ENVIRONMENT: &str = r"SYSTEM\CurrentControlSet\Control\Session Manager\Environment";
const PATH: &str = "Path";

/// Appends `dir` to the user's `Path` unless an entry already names it, keeping the value's registry type, then
/// tells running programs that the environment changed. Returns whether the value changed.
pub fn add(dir: &Path) -> io::Result<bool> {
    let key = RegKey::predef(HKEY_CURRENT_USER).open_subkey_with_flags(USER_ENVIRONMENT, KEY_READ | KEY_SET_VALUE)?;
    let (value, kind) = match key.get_raw_value(PATH) {
        Ok(raw) => (String::from_reg_value(&raw)?, raw.vtype),
        Err(error) if error.kind() == io::ErrorKind::NotFound => (String::new(), REG_EXPAND_SZ),
        Err(error) => return Err(error),
    };
    if entries(&value).any(|entry| same_dir(&expand(entry, env_var), dir)) {
        return Ok(false);
    }

    let mut updated = value.trim_end_matches(';').to_owned();
    if !updated.is_empty() {
        updated.push(';');
    }
    updated.push_str(&dir.to_string_lossy());
    let kind = if kind == REG_SZ { REG_SZ } else { REG_EXPAND_SZ };
    key.set_raw_value(
        PATH,
        &RegValue {
            bytes: utf16_with_nul(&updated).into(),
            vtype: kind,
        },
    )?;
    broadcast_environment_change();

    Ok(true)
}

/// First directory of the `Path` a new process gets that contains `file`.
pub fn first_holding(file: &str) -> Option<PathBuf> {
    let system = read_path(&RegKey::predef(HKEY_LOCAL_MACHINE), SYSTEM_ENVIRONMENT);
    let user = read_path(&RegKey::predef(HKEY_CURRENT_USER), USER_ENVIRONMENT);

    entries(&system)
        .chain(entries(&user))
        .map(|entry| PathBuf::from(expand(entry, env_var)))
        .find(|dir| dir.join(file).is_file())
}

/// Whether two directories name the same place, ignoring case and trailing separators as Windows does.
pub fn same_dir(left: &str, right: &Path) -> bool {
    let normalize = |path: &str| path.trim_end_matches(['\\', '/']).replace('/', "\\").to_lowercase();

    normalize(left) == normalize(&right.to_string_lossy())
}

fn read_path(root: &RegKey, key: &str) -> String {
    root.open_subkey_with_flags(key, KEY_READ)
        .and_then(|key| key.get_raw_value(PATH))
        .and_then(|raw| String::from_reg_value(&raw))
        .unwrap_or_default()
}

fn entries(value: &str) -> impl Iterator<Item = &str> {
    value.split(';').map(str::trim).filter(|entry| !entry.is_empty())
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// Replaces each `%NAME%` with `lookup(NAME)`; names it does not know stay as written.
fn expand(entry: &str, lookup: impl Fn(&str) -> Option<String>) -> String {
    let mut expanded = String::new();
    let mut rest = entry;
    while let Some(start) = rest.find('%') {
        let Some(length) = rest[start + 1..].find('%') else {
            break;
        };
        let name = &rest[start + 1..start + 1 + length];
        expanded.push_str(&rest[..start]);
        match lookup(name).filter(|_| !name.is_empty()) {
            Some(value) => expanded.push_str(&value),
            None => expanded.push_str(&rest[start..start + length + 2]),
        }
        rest = &rest[start + length + 2..];
    }
    expanded.push_str(rest);
    expanded
}

fn utf16_with_nul(text: &str) -> Vec<u8> {
    text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

#[allow(unsafe_code)]
fn broadcast_environment_change() {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        HWND_BROADCAST, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SETTINGCHANGE,
    };

    let section: Vec<u16> = "Environment".encode_utf16().chain([0]).collect();
    // SAFETY: `section` is a NUL-terminated UTF-16 string that outlives the call, which only reads it.
    unsafe {
        SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SETTINGCHANGE,
            0,
            section.as_ptr() as isize,
            SMTO_ABORTIFHUNG,
            5000,
            std::ptr::null_mut(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expands_known_variables_only() {
        let lookup = |name: &str| (name == "USERPROFILE").then(|| r"C:\Users\dev".to_owned());

        assert_eq!(expand(r"%USERPROFILE%\bin", lookup), r"C:\Users\dev\bin");
        assert_eq!(expand(r"%UNSET%\bin", lookup), r"%UNSET%\bin");
        assert_eq!(expand(r"C:\100%", lookup), r"C:\100%");
    }

    #[test]
    fn compares_directories_like_windows() {
        assert!(same_dir(
            r"C:\Users\Dev\AppData\Local\Contr\cli\bin\",
            Path::new(r"c:/users/dev/appdata/local/contr/cli/bin")
        ));
        assert!(!same_dir(r"C:\Program Files\Docker", Path::new(r"C:\Users\dev\bin")));
    }
}
