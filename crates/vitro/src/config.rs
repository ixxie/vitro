use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Deserialize, Serialize)]
pub struct CellConfig {
    #[serde(default = "default_memory")]
    pub memory: String,
    #[serde(default = "default_vcpu")]
    pub vcpu: u32,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub post_push: Option<String>,
    #[serde(default)]
    pub secrets: SecretsConfig,
    #[serde(default)]
    pub egress: CellEgressConfig,
    #[serde(default)]
    pub acp: Option<AcpConfig>,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct AcpConfig {
    /// Command + args that produce an ACP (JSON-RPC over stdio) server
    /// when invoked inside the cell. Run by `vitro acp <cell>` which
    /// forwards stdin/stdout to the spawning ACP client (e.g. Paseo).
    pub command: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct SecretsConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct CellEgressConfig {
    #[serde(default)]
    pub writes: Option<CellEgressRules>,
    #[serde(default)]
    pub reads: Option<CellEgressRules>,
    #[serde(default)]
    pub credentials: Vec<CredentialConfig>,
}

#[derive(Debug, Default, Deserialize, Serialize, Clone)]
pub struct CellEgressRules {
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

impl Default for CellConfig {
    fn default() -> Self {
        Self {
            memory: default_memory(),
            vcpu: default_vcpu(),
            ports: Vec::new(),
            server: None,
            post_push: None,
            secrets: SecretsConfig::default(),
            egress: CellEgressConfig::default(),
            acp: None,
        }
    }
}

pub fn load(repo_root: &Path) -> Result<CellConfig> {
    let config_path = repo_root.join(".vitro/config.toml");
    if config_path.exists() {
        let content = std::fs::read_to_string(&config_path)
            .context("reading config")?;
        toml::from_str(&content).context("parsing config")
    } else {
        Ok(CellConfig::default())
    }
}
