use std::{fs, path::PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

const DEFAULT_CONFIG_PATH: &str = "aether.toml";

#[derive(Debug, Deserialize)]
pub struct Config {
    pub path: PathBuf,
    pub originals_dir: PathBuf,
    pub thumbnails_dir: PathBuf,
    pub thumbnail_width: u32,
    pub thumbnail_format: ThumbnailFormat,
    pub thumbnail_quality: u8,
    pub enable_blurhash: bool,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThumbnailFormat {
    Jpeg,
    Png,
    Webp,
}

impl Config {
    pub fn load() -> Result<Self> {
        let raw = fs::read_to_string(DEFAULT_CONFIG_PATH)
            .with_context(|| format!("failed to read {DEFAULT_CONFIG_PATH}"))?;
        let config: Self = toml::from_str(&raw)
            .with_context(|| format!("failed to parse {DEFAULT_CONFIG_PATH}"))?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.thumbnail_width == 0 {
            bail!("thumbnail_width must be greater than 0");
        }

        if self.thumbnail_quality == 0 || self.thumbnail_quality > 100 {
            bail!("thumbnail_quality must be between 1 and 100");
        }

        Ok(())
    }

    pub fn root_dir(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn originals_path(&self) -> PathBuf {
        resolve_under_root(&self.root_dir(), &self.originals_dir)
    }

    pub fn thumbnails_path(&self) -> PathBuf {
        resolve_under_root(&self.root_dir(), &self.thumbnails_dir)
    }

    pub fn manifest_path(&self) -> PathBuf {
        self.root_dir().join("manifest.json")
    }

    pub fn state_path(&self) -> PathBuf {
        self.root_dir().join("state.json")
    }
}

impl ThumbnailFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
            Self::Webp => "webp",
        }
    }
}

fn resolve_under_root(root: &std::path::Path, path: &PathBuf) -> PathBuf {
    if path.is_absolute() {
        path.clone()
    } else {
        root.join(path)
    }
}
