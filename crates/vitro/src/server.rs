use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::PathBuf;

// Server registry is declarative: NixOS users set `vitro.client.servers`
// (which materializes the file); non-NixOS users hand-edit it. The CLI
// only reads — there's no `server add`/`server remove` to muddy the
// state with imperative writes.
const REGISTRY_FILE: &str = ".config/vitro/servers.toml";
const CLIENT_CONFIG: &str = ".config/vitro/config.toml";

// Client config

#[derive(Debug, Default, serde::Deserialize)]
pub struct ClientConfig {
    #[serde(default)]
    pub sync: Vec<String>,
    #[serde(default)]
    pub server: Option<String>,
}

pub fn load_client_config() -> ClientConfig {
    let path = home_dir().join(CLIENT_CONFIG);
    if !path.exists() {
        return ClientConfig::default();
    }
    std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

#[derive(Debug, Clone)]
pub enum ActiveServer {
    Localhost,
    Remote { name: String },
}

impl ActiveServer {
    pub fn target(&self) -> Result<Option<String>> {
        match self {
            ActiveServer::Localhost => Ok(None),
            ActiveServer::Remote { name } => {
                let registry = load_registry()?;
                let entry = registry.get(name.as_str())
                    .ok_or_else(|| anyhow::anyhow!("server '{name}' not in registry"))?;
                Ok(Some(entry.target.clone()))
            }
        }
    }

    pub fn is_server(&self) -> bool {
        matches!(self, ActiveServer::Remote { .. })
    }

}

// Server registry

#[derive(Debug, serde::Deserialize, serde::Serialize)]
pub struct ServerEntry {
    pub target: String,
}

fn home_dir() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".to_string()))
}

fn registry_path() -> PathBuf {
    home_dir().join(REGISTRY_FILE)
}

pub fn load_registry() -> Result<HashMap<String, ServerEntry>> {
    let path = registry_path();
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let content = std::fs::read_to_string(&path)
        .context("reading server registry")?;
    toml::from_str(&content).context("parsing server registry")
}

pub fn list() -> Result<Vec<(String, String)>> {
    let registry = load_registry()?;
    let mut entries: Vec<(String, String)> = registry
        .into_iter()
        .map(|(name, entry)| (name, entry.target))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(entries)
}


pub fn resolve(name: &str) -> Result<ActiveServer> {
    if name == "localhost" {
        return Ok(ActiveServer::Localhost);
    }

    let registry = load_registry()?;
    if registry.contains_key(name) {
        Ok(ActiveServer::Remote { name: name.to_string() })
    } else {
        anyhow::bail!(
            "server '{name}' not in registry.\n  \
             NixOS:  set vitro.client.servers.{name} = \"<ssh-target>\";\n  \
             else:   add to ~/{REGISTRY_FILE} as [{name}] target = \"<ssh-target>\""
        )
    }
}
