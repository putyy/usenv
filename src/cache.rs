use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Serialize, de::DeserializeOwned};

use crate::runtimes::home_directory;
use crate::settings::atomic_write;

fn directory() -> Option<PathBuf> {
    home_directory().map(|home| home.join("Library/Caches/io.github.putyy.usenv"))
}

pub fn read<T: DeserializeOwned>(name: &str) -> Option<T> {
    let path = directory()?.join(name);
    let metadata = path.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > 4 * 1024 * 1024 {
        return None;
    }
    let data = fs::read(path).ok()?;
    serde_json::from_slice(&data).ok()
}

pub fn write<T: Serialize>(name: &str, data: &T) -> Result<()> {
    if let Some(directory) = directory() {
        fs::create_dir_all(&directory)?;
        atomic_write(&directory.join(name), &serde_json::to_vec(data)?)?;
    }
    Ok(())
}
