use std::path::Path;
use std::{fs, io};

use serde::Deserialize;
use serde_json::Value;

/// Engine objects a [`crate::FakeEngine`] starts with, in Docker Engine API JSON as the list endpoints return them.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Fixtures {
    #[serde(default)]
    pub(crate) containers: Vec<Value>,
    #[serde(default)]
    pub(crate) images: Vec<Value>,
    #[serde(default)]
    pub(crate) volumes: Vec<Value>,
    #[serde(default)]
    pub(crate) networks: Vec<Value>,
}

impl Fixtures {
    /// Three containers of which two run, their images, one volume and two networks.
    pub fn basic() -> Self {
        Self::parse(include_str!("../fixtures/basic.json")).unwrap_or_default()
    }

    pub fn from_file(path: &Path) -> io::Result<Self> {
        Self::parse(&fs::read_to_string(path)?).map_err(io::Error::other)
    }

    fn parse(text: &str) -> serde_json::Result<Self> {
        serde_json::from_str(text)
    }

    pub fn container_count(&self) -> usize {
        self.containers.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_fixtures_parse() {
        let fixtures = Fixtures::parse(include_str!("../fixtures/basic.json")).unwrap();

        assert_eq!(fixtures.containers.len(), 3);
        assert_eq!(fixtures.images.len(), 3);
        assert_eq!(fixtures.volumes.len(), 1);
        assert_eq!(fixtures.networks.len(), 2);
    }
}
