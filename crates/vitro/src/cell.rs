use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use tracing::{info, warn};

const CELLS_PATH: &str = "/var/lib/vitro/cells";
const IP_POOL: &str = "/var/lib/vitro/ip-pool.json";

// IP pool

pub fn allocate_ip(name: &str) -> Result<String> {
    let mut pool = load_ip_pool();

    if let Some(ip) = pool.get(name) {
        return Ok(ip.clone());
    }

    let used: std::collections::HashSet<&str> = pool.values().map(|s| s.as_str()).collect();
    for i in 11..=254u16 {
        let ip = format!("192.168.83.{i}");
        if !used.contains(ip.as_str()) {
            pool.insert(name.to_string(), ip.clone());
            save_ip_pool(&pool);
            info!(ip = %ip, cell = %name, "allocated IP");
            return Ok(ip);
        }
    }

    anyhow::bail!("IP pool exhausted (244 cells max)")
}

pub fn release_ip(name: &str) {
    let mut pool = load_ip_pool();
    pool.remove(name);
    save_ip_pool(&pool);
}

fn load_ip_pool() -> HashMap<String, String> {
    std::fs::read_to_string(IP_POOL)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_ip_pool(pool: &HashMap<String, String>) {
    let json = serde_json::to_string_pretty(pool).unwrap_or_default();
    if let Err(e) = std::fs::write(IP_POOL, json) {
        warn!(error = %e, "failed to save IP pool");
    }
}

// Path helpers

pub fn cell_dir(name: &str) -> PathBuf {
    PathBuf::from(CELLS_PATH).join(name)
}

pub fn cell_repo_dir(name: &str) -> PathBuf {
    cell_dir(name).join("repo")
}

pub fn cell_flake_dir(name: &str) -> PathBuf {
    cell_dir(name).join("flake")
}

pub fn runtime_dir(name: &str) -> PathBuf {
    let base = std::env::var("XDG_RUNTIME_DIR")
        .unwrap_or_else(|_| "/tmp".to_string());
    let dir = Path::new(&base).join("vitro").join(name);
    if !dir.join("ip").exists() {
        let fallback = Path::new("/tmp").join("vitro").join(name);
        if fallback.join("ip").exists() {
            return fallback;
        }
    }
    dir
}

pub fn list_cells() -> Result<Vec<String>> {
    let dir = PathBuf::from(CELLS_PATH);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut names = Vec::new();
    for entry in std::fs::read_dir(&dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            if let Some(s) = entry.file_name().to_str() {
                names.push(s.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

