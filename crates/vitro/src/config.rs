use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize)]
pub struct EnvConfig {
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_vcpu")]
    pub vcpu: u32,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub egress: EnvEgressConfig,
    #[serde(default)]
    pub persist: Vec<PersistConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PersistConfig {
    /// Absolute path inside the env to persist across restarts
    pub path: String,
    /// Human-readable description of what this stores
    pub purpose: Option<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct SecretsConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct EnvEgressConfig {
    #[serde(default)]
    pub writes: Option<EnvEgressRules>,
    #[serde(default)]
    pub reads: Option<EnvEgressRules>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct EnvEgressRules {
    pub allowed: Option<Vec<String>>,
    pub denied: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct CredentialConfig {
    pub host: String,
    pub header: String,
    pub env_var: String,
}

fn default_memory() -> String { "2G".to_string() }
fn default_vcpu() -> u32 { 2 }

impl Default for EnvConfig {
    fn default() -> Self {
        Self {
            memory: default_memory(),
            vcpu: default_vcpu(),
            ports: Vec::new(),
            server: None,
            secrets: SecretsConfig::default(),
            egress: EnvEgressConfig::default(),
            persist: Vec::new(),
        }
    }
}

pub fn load(repo_root: &Path) -> Result<EnvConfig> {
    let config_path = repo_root.join(".vitro/config.toml");
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)
            .context("reading config")?;
        toml::from_str(&content).context("parsing config")
    } else {
        Ok(EnvConfig::default())
    }
}
