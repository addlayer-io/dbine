//! What AI tools this machine has, and whether each can answer now.

use crate::{ollama, openai, ProviderKind};
use serde::Serialize;

#[derive(Debug, Clone)]
pub struct Endpoints {
    pub ollama: String,
    pub lmstudio: String,
    /// Where the built-in model's files go.
    pub models_dir: std::path::PathBuf,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            ollama: std::env::var("OLLAMA_HOST")
                .ok()
                .filter(|h| !h.is_empty())
                .map(|h| if h.starts_with("http") { h } else { format!("http://{h}") })
                .unwrap_or_else(|| "http://127.0.0.1:11434".into()),
            lmstudio: "http://127.0.0.1:1234".into(),
            models_dir: std::env::temp_dir().join("dbine-models"),
        }
    }
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ModelInfo {
    /// What goes in the request (`""`: the tool's default).
    pub id: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProviderInfo {
    pub kind: ProviderKind,
    pub label: &'static str,
    /// Found on the machine.
    pub installed: bool,
    /// Can answer now.
    pub available: bool,
    /// Runs entirely on this machine.
    pub local: bool,
    /// One line for the UI: why it isn't available, or what it has.
    pub status: String,
    pub models: Vec<ModelInfo>,
    /// `start_ollama` / `pull_model`: what the UI can offer to fix it.
    pub action: Option<&'static str>,
    pub path: Option<String>,
}

fn claude_models() -> Vec<ModelInfo> {
    [("sonnet", "Sonnet: equilibrado"), ("opus", "Opus: el más capaz"), ("haiku", "Haiku: el más rápido")]
        .into_iter()
        .map(|(id, d)| ModelInfo { id: id.into(), detail: Some(d.into()) })
        .collect()
}

async fn detect_ollama(ep: &Endpoints) -> ProviderInfo {
    let path = crate::env::find("ollama");
    let app = cfg!(target_os = "macos") && std::path::Path::new("/Applications/Ollama.app").exists();
    let installed = path.is_some() || app;
    let mut p = ProviderInfo {
        kind: ProviderKind::Ollama,
        label: "Ollama",
        installed,
        available: false,
        local: true,
        status: String::new(),
        models: vec![],
        action: None,
        path: path.map(|p| p.display().to_string()),
    };
    match ollama::models(&ep.ollama).await {
        Ok(models) if models.is_empty() => {
            p.installed = true;
            p.status = "está corriendo pero no tiene modelos descargados".into();
            p.action = Some("pull_model");
        }
        Ok(models) => {
            p.installed = true;
            p.available = true;
            p.status = format!("{} modelo(s) · local, nada sale de esta máquina", models.len());
            p.models = models;
        }
        Err(_) if installed => {
            p.status = "instalado, pero no está abierto".into();
            p.action = Some("start_ollama");
        }
        Err(_) => p.status = "no instalado".into(),
    }
    p
}

async fn detect_lmstudio(ep: &Endpoints) -> ProviderInfo {
    let app = cfg!(target_os = "macos") && std::path::Path::new("/Applications/LM Studio.app").exists();
    let cli = crate::env::find("lms");
    let mut p = ProviderInfo {
        kind: ProviderKind::LmStudio,
        label: "LM Studio",
        installed: app || cli.is_some(),
        available: false,
        local: true,
        status: String::new(),
        models: vec![],
        action: None,
        path: cli.map(|p| p.display().to_string()),
    };
    match openai::models(&ep.lmstudio).await {
        Ok(models) if !models.is_empty() => {
            p.installed = true;
            p.available = true;
            p.status = format!("{} modelo(s) · local, nada sale de esta máquina", models.len());
            p.models = models;
        }
        Ok(_) => {
            p.installed = true;
            p.status = "el servidor está corriendo pero no hay modelos cargados".into();
        }
        Err(_) if p.installed => p.status = "instalado, pero su servidor local no está iniciado (Developer › Start Server)".into(),
        Err(_) => p.status = "no instalado".into(),
    }
    p
}

fn detect_cli(kind: ProviderKind, bin: &str, account: &str) -> ProviderInfo {
    let path = crate::env::find(bin);
    let installed = path.is_some();
    ProviderInfo {
        kind,
        label: kind.label(),
        installed,
        available: installed,
        local: false,
        status: if installed { format!("instalado · usa tu cuenta de {account}") } else { "no instalado".into() },
        models: match kind {
            ProviderKind::ClaudeCode => claude_models(),
            _ => vec![ModelInfo { id: String::new(), detail: Some("el modelo configurado en Codex".into()) }],
        },
        action: None,
        path: path.map(|p| p.display().to_string()),
    }
}

fn detect_embedded(ep: &Endpoints) -> ProviderInfo {
    use crate::embedded;
    let have = embedded::installed(&ep.models_dir);
    let mut p = ProviderInfo {
        kind: ProviderKind::Embedded,
        label: "Integrado en DBine",
        installed: embedded::ENABLED,
        available: embedded::ENABLED && !have.is_empty(),
        local: true,
        status: String::new(),
        models: have.iter().map(|m| ModelInfo { id: m.id.into(), detail: Some(m.detail.into()) }).collect(),
        action: None,
        path: None,
    };
    p.status = if !embedded::ENABLED {
        "esta versión se compiló sin el modelo integrado".into()
    } else if have.is_empty() {
        p.action = Some("download_model");
        "sin instalar nada: descargá un modelo (una sola vez)".into()
    } else {
        format!("{} modelo(s) · local, nada sale de esta máquina", have.len())
    };
    p
}

/// Every provider, in the order the UI offers them.
pub async fn detect(ep: &Endpoints) -> Vec<ProviderInfo> {
    // The PATH lookup may run the login shell: off the async threads.
    let clis = tokio::task::spawn_blocking(|| {
        (detect_cli(ProviderKind::ClaudeCode, "claude", "Claude"), detect_cli(ProviderKind::Codex, "codex", "ChatGPT / OpenAI"))
    })
    .await
    .ok();
    let (ollama, lmstudio) = tokio::join!(detect_ollama(ep), detect_lmstudio(ep));
    let mut out = vec![detect_embedded(ep), ollama];
    if let Some((claude, codex)) = clis {
        out.push(claude);
        out.push(codex);
    }
    out.push(lmstudio);
    out
}
