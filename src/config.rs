use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    Anthropic,
    OpenAi,
    CommandCode,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct ProviderConf {
    pub kind: ProviderKind,
    pub model: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    pub max_tokens: Option<u32>,
    pub headers: BTreeMap<String, String>,
}

impl Default for ProviderConf {
    fn default() -> Self {
        Self {
            kind: ProviderKind::OpenAi,
            model: String::new(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_tokens: None,
            headers: BTreeMap::new(),
        }
    }
}

impl ProviderConf {
    pub fn resolved_key(&self) -> Option<String> {
        if let Some(env) = &self.api_key_env {
            if let Ok(v) = std::env::var(env) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        if self.api_key.is_some() {
            return self.api_key.clone();
        }
        if self.kind == ProviderKind::CommandCode {
            return commandcode_key();
        }
        None
    }

    pub fn resolved_base(&self) -> String {
        if let Some(b) = &self.base_url {
            return b.trim_end_matches('/').to_string();
        }
        match self.kind {
            ProviderKind::Anthropic => "https://api.anthropic.com".into(),
            ProviderKind::OpenAi => "https://api.openai.com/v1".into(),
            ProviderKind::CommandCode => "https://api.commandcode.ai".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApprovalMode {
    Ask,
    Auto,
    Allowlist,
}

/// web_search / web_fetch tool settings.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SearchConf {
    /// "duckduckgo" (no key) | "brave" | "tavily" | "searxng"
    pub backend: String,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    /// SearXNG instance base URL
    pub base_url: Option<String>,
    pub max_results: usize,
    pub fetch_max_chars: usize,
}

impl Default for SearchConf {
    fn default() -> Self {
        Self {
            backend: "duckduckgo".into(),
            api_key: None,
            api_key_env: None,
            base_url: None,
            max_results: 5,
            fetch_max_chars: 20_000,
        }
    }
}

impl SearchConf {
    /// api_key_env > api_key > the backend's conventional env var.
    pub fn resolved_key(&self) -> Option<String> {
        if let Some(env) = &self.api_key_env {
            if let Ok(v) = std::env::var(env) {
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
        if let Some(k) = &self.api_key {
            if !k.is_empty() {
                return Some(k.clone());
            }
        }
        let env = match self.backend.as_str() {
            "brave" => "BRAVE_API_KEY",
            "tavily" => "TAVILY_API_KEY",
            _ => return None,
        };
        std::env::var(env).ok().filter(|s| !s.is_empty())
    }
}

fn default_true() -> bool {
    true
}

/// Compatible with Claude Desktop's `mcpServers` entries (stdio transport).
/// Hosted/streamable-HTTP servers set `url` (and optionally OAuth fields)
/// and leave `command` empty.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct McpServerConf {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// streamable-http endpoint for hosted servers
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub oauth_client_id: Option<String>,
    #[serde(default)]
    pub oauth_client_secret: Option<String>,
    /// space-separated OAuth scopes to request
    #[serde(default)]
    pub oauth_scopes: Option<String>,
    /// resolved token endpoint (persisted so refresh needs no discovery)
    #[serde(default)]
    pub oauth_token_url: Option<String>,
}

impl Default for McpServerConf {
    fn default() -> Self {
        Self {
            command: String::new(),
            args: vec![],
            env: Default::default(),
            enabled: true,
            url: None,
            oauth_client_id: None,
            oauth_client_secret: None,
            oauth_scopes: None,
            oauth_token_url: None,
        }
    }
}

pub const DEFAULT_SYSTEM: &str = "You are amty, a lightweight desktop AI agent. \
You can read/write files, run shell commands, and change this app's own settings \
through the provided tools. Use web_search to look up current information and \
web_fetch to read a page; cite source URLs in your answer. Ask before destructive \
actions unless the user already approved them. Keep answers concise. Always reply \
in the same language the user writes in.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Active provider name (key into `providers`).
    pub provider: String,
    pub approval: ApprovalMode,
    /// Shell command prefixes allowed without asking (allowlist mode).
    pub allow_commands: Vec<String>,
    /// Path prefixes writable without asking (allowlist mode).
    pub allow_paths: Vec<String>,
    pub system_prompt: String,
    /// UI language: "en" | "ja".
    pub lang: String,
    pub search: SearchConf,
    pub providers: BTreeMap<String, ProviderConf>,
    /// Same shape as claude_desktop_config.json `mcpServers`.
    pub mcp_servers: BTreeMap<String, McpServerConf>,
}

impl Default for Config {
    fn default() -> Self {
        let mut providers = BTreeMap::new();
        providers.insert(
            "anthropic".into(),
            ProviderConf {
                kind: ProviderKind::Anthropic,
                model: "claude-sonnet-4-5".into(),
                api_key_env: Some("ANTHROPIC_API_KEY".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "openai".into(),
            ProviderConf {
                kind: ProviderKind::OpenAi,
                model: "gpt-5-mini".into(),
                api_key_env: Some("OPENAI_API_KEY".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "commandcode".into(),
            ProviderConf {
                kind: ProviderKind::CommandCode,
                model: "deepseek/deepseek-v4-flash".into(),
                ..Default::default()
            },
        );
        providers.insert(
            "ollama".into(),
            ProviderConf {
                kind: ProviderKind::OpenAi,
                model: "qwen3".into(),
                base_url: Some("http://localhost:11434/v1".into()),
                ..Default::default()
            },
        );
        Self {
            provider: "anthropic".into(),
            approval: ApprovalMode::Ask,
            allow_commands: vec!["ls".into(), "pwd".into(), "git status".into()],
            allow_paths: vec![],
            system_prompt: DEFAULT_SYSTEM.into(),
            lang: "ja".into(),
            search: SearchConf::default(),
            providers,
            mcp_servers: BTreeMap::new(),
        }
    }
}

/// Read the apiKey stored by the `cmdc` CLI (`~/.commandcode/auth.json`).
fn commandcode_key() -> Option<String> {
    let path = dirs::home_dir()?.join(".commandcode/auth.json");
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    v.get("apiKey")?.as_str().map(String::from).filter(|s| !s.is_empty())
}

pub fn config_dir() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| PathBuf::from(".")).join("amty")
}

pub fn data_dir() -> PathBuf {
    dirs::data_dir().unwrap_or_else(|| PathBuf::from(".")).join("amty")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn runtime_path() -> PathBuf {
    data_dir().join("runtime.json")
}

impl Config {
    /// Load config, writing a default file on first run.
    pub fn load() -> Result<(Self, PathBuf)> {
        let path = config_path();
        if !path.exists() {
            let cfg = Config::default();
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            if let Ok(text) = toml::to_string_pretty(&cfg) {
                std::fs::write(&path, text).ok();
            }
            return Ok((cfg, path));
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("read {}", path.display()))?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("parse {}", path.display()))?;
        Ok((cfg, path))
    }

    pub fn save(&self, path: &PathBuf) -> Result<()> {
        let text = toml::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// `config_set` tool / `POST /v1/config` backend.
    /// Keys: provider | model | approval | system_prompt | max_tokens |
    ///       allow_commands | allow_paths | +allow_commands | +allow_paths |
    ///       provider.<name>.{model,api_key_env,api_key,base_url} | mcp.<name>.enabled |
    ///       search.{backend,api_key,api_key_env,base_url,max_results,fetch_max_chars}
    pub fn apply_set(&mut self, key: &str, value: &str) -> Result<String> {
        match key {
            "provider" => {
                if !self.providers.contains_key(value) {
                    bail!("unknown provider '{value}' (have: {})", self.providers.keys().cloned().collect::<Vec<_>>().join(", "));
                }
                self.provider = value.into();
                Ok(format!("provider = {value}"))
            }
            "model" => {
                let p = self.active_mut()?;
                p.model = value.into();
                Ok(format!("model = {value}"))
            }
            "max_tokens" => {
                let n: u32 = value.parse().context("max_tokens must be a number")?;
                self.active_mut()?.max_tokens = Some(n);
                Ok(format!("max_tokens = {n}"))
            }
            "approval" => {
                self.approval = match value {
                    "ask" => ApprovalMode::Ask,
                    "auto" => ApprovalMode::Auto,
                    "allowlist" => ApprovalMode::Allowlist,
                    _ => bail!("approval must be ask|auto|allowlist"),
                };
                Ok(format!("approval = {value}"))
            }
            "system_prompt" => {
                self.system_prompt = value.into();
                Ok("system_prompt updated".into())
            }
            "lang" => {
                if !crate::i18n::LANGS.contains(&value) {
                    bail!("lang must be one of: {}", crate::i18n::LANGS.join(", "));
                }
                self.lang = value.into();
                Ok(format!("lang = {value}"))
            }
            "allow_commands" | "allow_paths" | "+allow_commands" | "+allow_paths" => {
                let (append, field) = if key.starts_with('+') {
                    (true, &key[1..])
                } else {
                    (false, key)
                };
                let list = if field == "allow_commands" { &mut self.allow_commands } else { &mut self.allow_paths };
                let items: Vec<String> = value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
                if append {
                    list.extend(items);
                } else {
                    *list = items;
                }
                list.sort();
                list.dedup();
                Ok(format!("{field} = [{}]", list.join(", ")))
            }
            _ if key.starts_with("provider.") => {
                let parts: Vec<&str> = key.splitn(3, '.').collect();
                if parts.len() != 3 {
                    bail!("usage: provider.<name>.<field>");
                }
                let p = self.providers.get_mut(parts[1]).ok_or_else(|| anyhow!("unknown provider '{}'", parts[1]))?;
                match parts[2] {
                    "model" => p.model = value.into(),
                    "api_key" => p.api_key = Some(value.into()),
                    "api_key_env" => p.api_key_env = Some(value.into()),
                    "base_url" => p.base_url = Some(value.into()),
                    _ => bail!("unknown provider field '{}'", parts[2]),
                }
                Ok(format!("{key} updated"))
            }
            _ if key.starts_with("search.") => {
                let field = &key["search.".len()..];
                match field {
                    "backend" => {
                        if !["duckduckgo", "brave", "tavily", "searxng"].contains(&value) {
                            bail!("search.backend must be duckduckgo|brave|tavily|searxng");
                        }
                        self.search.backend = value.into();
                    }
                    "api_key" => self.search.api_key = Some(value.into()),
                    "api_key_env" => self.search.api_key_env = Some(value.into()),
                    "base_url" => self.search.base_url = (!value.is_empty()).then(|| value.into()),
                    "max_results" => self.search.max_results = value.parse().context("max_results must be a number")?,
                    "fetch_max_chars" => self.search.fetch_max_chars = value.parse().context("fetch_max_chars must be a number")?,
                    _ => bail!("unknown search field '{field}'"),
                }
                Ok(format!("{key} updated"))
            }
            _ if key.starts_with("mcp.") => {
                let parts: Vec<&str> = key.splitn(3, '.').collect();
                if parts.len() == 3 && parts[2] == "enabled" {
                    let s = self.mcp_servers.get_mut(parts[1]).ok_or_else(|| anyhow!("unknown mcp server '{}'", parts[1]))?;
                    s.enabled = matches!(value, "true" | "1" | "on" | "yes");
                    Ok(format!("{key} = {}", s.enabled))
                } else {
                    bail!("supported mcp key: mcp.<name>.enabled")
                }
            }
            _ => bail!("unknown key '{key}'"),
        }
    }

    pub fn get(&self, key: &str) -> Result<String> {
        match key {
            "provider" => Ok(self.provider.clone()),
            "model" => Ok(self.providers.get(&self.provider).map(|p| p.model.clone()).unwrap_or_default()),
            "approval" => Ok(format!("{:?}", self.approval).to_lowercase()),
            "system_prompt" => Ok(self.system_prompt.clone()),
            "lang" => Ok(self.lang.clone()),
            "allow_commands" => Ok(self.allow_commands.join(", ")),
            "allow_paths" => Ok(self.allow_paths.join(", ")),
            "providers" => Ok(self.providers.keys().cloned().collect::<Vec<_>>().join(", ")),
            "mcp_servers" => Ok(self.mcp_servers.keys().cloned().collect::<Vec<_>>().join(", ")),
            "search" => Ok(self.search.backend.clone()),
            _ if key.starts_with("search.") => {
                let s = &self.search;
                match &key["search.".len()..] {
                    "backend" => Ok(s.backend.clone()),
                    "api_key" => Ok(if s.resolved_key().is_some() { "***set***".into() } else { String::new() }),
                    "api_key_env" => Ok(s.api_key_env.clone().unwrap_or_default()),
                    "base_url" => Ok(s.base_url.clone().unwrap_or_default()),
                    "max_results" => Ok(s.max_results.to_string()),
                    "fetch_max_chars" => Ok(s.fetch_max_chars.to_string()),
                    _ => bail!("unknown search field"),
                }
            }
            _ if key.starts_with("provider.") => {
                let parts: Vec<&str> = key.splitn(3, '.').collect();
                let p = self.providers.get(parts.get(1).copied().unwrap_or("")).ok_or_else(|| anyhow!("unknown provider"))?;
                match parts.get(2).copied() {
                    Some("model") => Ok(p.model.clone()),
                    Some("base_url") => Ok(p.base_url.clone().unwrap_or_default()),
                    Some("api_key_env") => Ok(p.api_key_env.clone().unwrap_or_default()),
                    Some("api_key") => Ok(if p.api_key.is_some() { "***set***".into() } else { "".into() }),
                    _ => bail!("unknown provider field"),
                }
            }
            _ => bail!("unknown key '{key}'"),
        }
    }

    fn active_mut(&mut self) -> Result<&mut ProviderConf> {
        let name = self.provider.clone();
        self.providers.get_mut(&name).ok_or_else(|| anyhow!("active provider '{name}' not defined"))
    }

    pub fn active(&self) -> Result<&ProviderConf> {
        self.providers.get(&self.provider).ok_or_else(|| anyhow!("active provider '{}' not defined", self.provider))
    }
}

/// Import `mcpServers` from a Claude Desktop config file. Returns added names.
pub fn import_claude(cfg: &mut Config) -> Result<Vec<String>> {
    let candidates = [
        dirs::home_dir().map(|h| h.join("Library/Application Support/Claude/claude_desktop_config.json")),
        std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("Claude/claude_desktop_config.json")),
        dirs::config_dir().map(|c| c.join("Claude/claude_desktop_config.json")),
    ];
    let path = candidates
        .into_iter()
        .flatten()
        .find(|p| p.exists())
        .ok_or_else(|| anyhow!("claude_desktop_config.json not found"))?;
    let text = std::fs::read_to_string(&path)?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    let servers = v.get("mcpServers").and_then(|m| m.as_object()).cloned().unwrap_or_default();
    let mut added = vec![];
    for (name, s) in servers {
        if cfg.mcp_servers.contains_key(&name) {
            continue;
        }
        let command = s.get("command").and_then(|c| c.as_str()).unwrap_or_default().to_string();
        if command.is_empty() {
            continue;
        }
        cfg.mcp_servers.insert(
            name.clone(),
            McpServerConf {
                command,
                args: s.get("args").and_then(|a| a.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default(),
                env: s.get("env").and_then(|e| e.as_object()).map(|e| e.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect()).unwrap_or_default(),
                enabled: true,
                ..Default::default()
            },
        );
        added.push(name);
    }
    Ok(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_set_provider_and_model() {
        let mut cfg = Config::default();
        cfg.apply_set("provider", "openai").unwrap();
        assert_eq!(cfg.provider, "openai");
        cfg.apply_set("model", "gpt-x").unwrap();
        assert_eq!(cfg.get("model").unwrap(), "gpt-x");
        assert!(cfg.apply_set("provider", "nope").is_err());
    }

    #[test]
    fn apply_set_allowlist() {
        let mut cfg = Config::default();
        cfg.apply_set("allow_commands", "ls, git status").unwrap();
        cfg.apply_set("+allow_commands", "cargo test").unwrap();
        assert!(cfg.allow_commands.contains(&"cargo test".to_string()));
    }

    #[test]
    fn apply_set_nested_keys() {
        let mut cfg = Config::default();
        cfg.apply_set("provider.ollama.model", "llama4").unwrap();
        assert_eq!(cfg.providers["ollama"].model, "llama4");
        cfg.apply_set("mcp.foo.enabled", "false").err().unwrap(); // unknown server
        cfg.mcp_servers.insert("srv".into(), McpServerConf {
            command: "x".into(), ..Default::default()
        });
        cfg.apply_set("mcp.srv.enabled", "false").unwrap();
        assert!(!cfg.mcp_servers["srv"].enabled);
    }

    #[test]
    fn toml_roundtrip() {
        let cfg = Config::default();
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back.provider, cfg.provider);
        assert_eq!(back.providers.len(), cfg.providers.len());
    }
}
