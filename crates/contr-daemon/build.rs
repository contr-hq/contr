//! Windows version information: the product name and the build's version, in the text and numeric fields alike, as
//! code signing checks them.

#[cfg_attr(
    not(windows),
    expect(clippy::unnecessary_wraps, reason = "only Windows hosts compile resources")
)]
fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-env-changed=CONTR_VERSION");
    // Resources compile only on a Windows host, where the Windows SDK's resource compiler lives.
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        use contr_core::product;
        use winresource::{VersionInfo, WindowsResource};

        let numeric = product::VERSION
            .split(['-', '+'])
            .next()
            .unwrap_or_default()
            .split('.')
            .take(3)
            .enumerate()
            .fold(0_u64, |version, (index, part)| {
                version | (part.parse::<u64>().unwrap_or(0) << (48 - 16 * index))
            });
        WindowsResource::new()
            .set("ProductName", product::DISPLAY_NAME)
            .set("FileDescription", &format!("{} daemon", product::DISPLAY_NAME))
            .set("ProductVersion", product::VERSION)
            .set("FileVersion", product::VERSION)
            .set("OriginalFilename", &format!("{}.exe", product::DAEMON_BINARY))
            .set_version_info(VersionInfo::PRODUCTVERSION, numeric)
            .set_version_info(VersionInfo::FILEVERSION, numeric)
            .compile()?;
    }
    Ok(())
}
