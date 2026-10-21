//! Settings an administrator fixes for every user of this computer, as the text of a JSON object shaped like the
//! stored settings. Only administrators can write where it lives.

use std::io;

/// Value of `HKEY_LOCAL_MACHINE\SOFTWARE\Policies\<product>` that holds the policy on Windows.
#[cfg(windows)]
const POLICY_VALUE: &str = "Settings";

/// The policy text; `None` when no administrator set one.
#[cfg(windows)]
pub fn admin_settings() -> io::Result<Option<String>> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_LOCAL_MACHINE, KEY_READ};

    let policies = format!(r"SOFTWARE\Policies\{}", contr_core::product::DISPLAY_NAME);
    let key = match RegKey::predef(HKEY_LOCAL_MACHINE).open_subkey_with_flags(policies, KEY_READ) {
        Ok(key) => key,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    match key.get_value::<String, _>(POLICY_VALUE) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// The policy text; `None` when no administrator set one.
#[cfg(unix)]
pub fn admin_settings() -> io::Result<Option<String>> {
    let path = std::path::Path::new("/etc")
        .join(contr_core::product::SLUG)
        .join("admin-settings.json");
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
