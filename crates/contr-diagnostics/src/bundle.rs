use std::io::{self, Cursor, Write};

use serde_json::Value;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::Redactor;

/// A zip of diagnostic files; every file passes through the redactor before it is stored.
pub struct Bundle {
    zip: ZipWriter<Cursor<Vec<u8>>>,
    redactor: Redactor,
}

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl Bundle {
    pub fn new(redactor: Redactor) -> Self {
        Self {
            zip: ZipWriter::new(Cursor::new(Vec::new())),
            redactor,
        }
    }

    /// Adds `text` as the file `name`, a `/`-separated path.
    pub fn add_text(&mut self, name: &str, text: &str) -> Result<(), BundleError> {
        let redacted = self.redactor.text(text);
        self.zip.start_file(name, SimpleFileOptions::default())?;
        self.zip.write_all(redacted.as_bytes())?;
        Ok(())
    }

    /// Adds `value` as the pretty-printed JSON file `name`.
    pub fn add_json(&mut self, name: &str, value: Value) -> Result<(), BundleError> {
        let redacted = self.redactor.json(value);
        self.zip.start_file(name, SimpleFileOptions::default())?;
        serde_json::to_writer_pretty(&mut self.zip, &redacted).map_err(io::Error::from)?;
        self.zip.write_all(b"\n")?;
        Ok(())
    }

    /// The zip's bytes.
    pub fn finish(self) -> Result<Vec<u8>, BundleError> {
        Ok(self.zip.finish()?.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use std::io::Read;

    use serde_json::json;
    use zip::ZipArchive;

    use super::*;
    use crate::Identity;

    #[test]
    fn stores_redacted_files() {
        let redactor = Redactor::new(&Identity {
            home: Some("/home/jdoe".to_owned()),
            user: None,
            host: None,
        })
        .unwrap();
        let mut bundle = Bundle::new(redactor);

        bundle
            .add_text("logs/contrd.log", "socket /home/jdoe/x token=abc\n")
            .unwrap();
        bundle
            .add_json("settings.json", json!({"proxy": {"password": "pw"}}))
            .unwrap();
        let mut zip = ZipArchive::new(Cursor::new(bundle.finish().unwrap())).unwrap();

        let mut read = |name: &str| {
            let mut text = String::new();
            zip.by_name(name).unwrap().read_to_string(&mut text).unwrap();
            text
        };
        assert_eq!(read("logs/contrd.log"), "socket ~/x token=<redacted>\n");
        assert_eq!(
            read("settings.json"),
            "{\n  \"proxy\": {\n    \"password\": \"<redacted>\"\n  }\n}\n"
        );
    }
}
