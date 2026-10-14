//! "Agregar mi IP al firewall de Azure": a server-level firewall rule on an
//! Azure SQL server, created with the user's own Azure CLI (`az`) login.
//!
//! Nothing here goes through a shell: `az` is run with its arguments as a
//! list, every value that reaches it is validated first (the host is an Azure
//! SQL name, the IP a public IPv4, the rule name `[A-Za-z0-9_-]`, the
//! resource group and subscription as Azure writes them), and each call has
//! a time limit. When `az` is missing or not logged in, the UI shows the
//! command, the portal and the T-SQL alternative instead.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use serde::{Deserialize, Serialize};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tauri::State;

/// Azure SQL servers' DNS suffixes (public cloud, US Government, China).
const AZURE_SQL_SUFFIXES: &[&str] = &[".database.windows.net", ".database.usgovcloudapi.net", ".database.chinacloudapi.cn"];
/// Longest a read (`account show`, `sql server list`) may take.
const READ_LIMIT: Duration = Duration::from_secs(60);
/// Longest the rule's creation may take.
const CREATE_LIMIT: Duration = Duration::from_secs(120);
/// Subscriptions looked at when the server isn't in the current one.
const MAX_SUBSCRIPTIONS: usize = 50;

#[derive(Deserialize)]
pub struct FirewallPlanArgs {
    pub connection_id: String,
}

/// What the confirmation shows, and what the UI needs when `az` can't do it.
#[derive(Serialize, Debug)]
pub struct FirewallPlan {
    /// The server's name as Azure knows it (`ventas` of
    /// `ventas.database.windows.net`).
    pub server: String,
    pub host: String,
    /// The rule to create: `DBine-<user>-<yyyymmdd>`.
    pub rule_name: String,
    /// "ready" (installed and logged in), "missing" or "logged_out".
    pub az: &'static str,
}

#[derive(Deserialize)]
pub struct AddFirewallRuleArgs {
    pub connection_id: String,
    pub ip: String,
    pub rule_name: String,
}

#[derive(Serialize, Debug)]
pub struct FirewallRuleAdded {
    pub server: String,
    pub resource_group: String,
    /// Set when the server was found in another subscription than the current one.
    pub subscription: Option<String>,
    pub rule_name: String,
    pub ip: String,
}

/// The rule that would be created, and whether `az` can create it. Changes nothing.
#[tauri::command(rename_all = "camelCase")]
pub async fn azure_firewall_plan(state: State<'_, AppState>, args: FirewallPlanArgs) -> CommandResult<FirewallPlan> {
    let (host, server) = connection_server(&state, &args.connection_id)?;
    let az = match find_az() {
        None => "missing",
        Some(bin) => match run_az(&bin, &["account", "show", "--query", "id", "-o", "tsv"], READ_LIMIT).await {
            Ok(out) if out.ok => "ready",
            _ => "logged_out",
        },
    };
    Ok(FirewallPlan { server, host, rule_name: default_rule_name(), az })
}

/// Create the rule (the user confirmed it, with this IP, name and server).
#[tauri::command(rename_all = "camelCase")]
pub async fn azure_sql_add_firewall_rule(state: State<'_, AppState>, args: AddFirewallRuleArgs) -> CommandResult<FirewallRuleAdded> {
    let (host, server) = connection_server(&state, &args.connection_id)?;
    let ip = public_ipv4(&args.ip)?.to_string();
    // Only this machine's own public IP, as an HTTPS service sees it: never
    // one a server named (a 40615 can come from whoever answers on the
    // path when the connection isn't encrypted or its certificate isn't
    // checked), nor one typed or changed in the window.
    let mine = lookup_public_ip().await?;
    if !same_ip(&ip, &mine) {
        return Err(CommandError::BadRequest(format!(
            "la IP de la regla ({ip}) no es tu IP pública ({mine}): DBine solo agrega tu propia IP al firewall"
        )));
    }
    let rule_name = valid_rule_name(&args.rule_name)?;
    let bin = find_az().ok_or_else(|| CommandError::BadRequest("no se encontró la CLI de Azure (az) en esta máquina".into()))?;
    let (found, subscription) = find_server(&bin, &host).await?;
    let mut cmd: Vec<&str> = vec![
        "sql", "server", "firewall-rule", "create", "-g", &found.resource_group, "-s", &found.name, "-n", &rule_name,
        "--start-ip-address", &ip, "--end-ip-address", &ip, "-o", "json",
    ];
    if let Some(sub) = subscription.as_deref() {
        cmd.extend(["--subscription", sub]);
    }
    let out = run_az(&bin, &cmd, CREATE_LIMIT).await?;
    if !out.ok {
        return Err(CommandError::BadRequest(format!("Azure no creó la regla: {}", az_error(&out.stderr))));
    }
    tracing::info!(server = %found.name, rule = %rule_name, "azure firewall rule created");
    Ok(FirewallRuleAdded { server, resource_group: found.resource_group, subscription, rule_name, ip })
}

/// Where the dialog sends the user to add the rule by hand.
pub const PORTAL_URL: &str = "https://portal.azure.com/#browse/Microsoft.Sql%2Fservers";
/// What answers this machine's public IP (only when the user asks).
const PUBLIC_IP_URL: &str = "https://api.ipify.org";

/// This machine's public IPv4 as the internet sees it ("Ver mi IP pública").
#[tauri::command(rename_all = "camelCase")]
pub async fn public_ip() -> CommandResult<String> {
    lookup_public_ip().await
}

/// Two IPv4 addresses written the same way once parsed.
fn same_ip(a: &str, b: &str) -> bool {
    matches!((a.trim().parse::<Ipv4Addr>(), b.trim().parse::<Ipv4Addr>()), (Ok(x), Ok(y)) if x == y)
}

async fn lookup_public_ip() -> CommandResult<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| CommandError::Internal(e.to_string()))?;
    let text = client
        .get(PUBLIC_IP_URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| CommandError::Connect(format!("no se pudo averiguar la IP pública: {e}")))?
        .text()
        .await
        .map_err(|e| CommandError::Connect(format!("no se pudo averiguar la IP pública: {e}")))?;
    let ip: Ipv4Addr = text.trim().parse().map_err(|_| CommandError::Connect("no se pudo averiguar la IP pública: respuesta inesperada".into()))?;
    Ok(ip.to_string())
}

/// Open the Azure portal's list of SQL servers in the browser (that address only).
#[tauri::command(rename_all = "camelCase")]
pub async fn open_azure_portal(app: tauri::AppHandle) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(PORTAL_URL, None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))
}

/// The saved connection's Azure SQL host and server name.
fn connection_server(state: &AppState, connection_id: &str) -> CommandResult<(String, String)> {
    let saved = state
        .store
        .get_connection(connection_id)?
        .ok_or_else(|| CommandError::NotFound(format!("no existe la conexión '{connection_id}'")))?;
    azure_sql_host(&saved.config.host)
        .ok_or_else(|| CommandError::BadRequest("la conexión no es a un servidor de Azure SQL (*.database.windows.net)".into()))
}

/// `tcp:ventas.database.windows.net,1433` → (`ventas.database.windows.net`, `ventas`).
/// `None` unless it's an Azure SQL server name made of `[a-z0-9-]` labels.
pub fn azure_sql_host(raw: &str) -> Option<(String, String)> {
    let mut h = raw.trim().to_ascii_lowercase();
    if let Some(rest) = h.strip_prefix("tcp:") {
        h = rest.to_string();
    }
    let h = h.split([',', ':', '\\', '/']).next()?.trim_end_matches('.').to_string();
    if h.is_empty() || h.len() > 253 || !h.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-') {
        return None;
    }
    let suffix = AZURE_SQL_SUFFIXES.iter().find(|s| h.ends_with(*s))?;
    let server = &h[..h.len() - suffix.len()];
    let ok = !server.is_empty() && server.len() <= 63 && !server.contains('.') && !server.starts_with('-') && !server.ends_with('-');
    ok.then(|| (h.clone(), server.to_string()))
}

/// A public IPv4 address, written canonically (`203.0.113.7`): what the
/// rule opens. Private, loopback and other non-routable addresses are
/// refused: Azure sees the public one.
pub fn public_ipv4(s: &str) -> CommandResult<Ipv4Addr> {
    let s = s.trim();
    let ip: Ipv4Addr = s.parse().map_err(|_| CommandError::BadRequest(format!("«{s}» no es una dirección IPv4 válida")))?;
    let o = ip.octets();
    let shared = o[0] == 100 && (64..128).contains(&o[1]);
    if ip.to_string() != s || ip.is_private() || ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast() || shared || o[0] == 0 || o[0] >= 240 {
        return Err(CommandError::BadRequest(format!("{s} no es una IP pública: Azure ve la IP pública de tu red (usá «Ver mi IP pública»)")));
    }
    Ok(ip)
}

/// A rule name as Azure takes it and this app writes it: `[A-Za-z0-9_-]`, 1 to 128.
pub fn valid_rule_name(s: &str) -> CommandResult<String> {
    let s = s.trim();
    let ok = !s.is_empty() && s.len() <= 128 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then(|| s.to_string()).ok_or_else(|| CommandError::BadRequest("el nombre de la regla solo puede tener letras, números, «-» y «_»".into()))
}

/// `DBine-<user>-<yyyymmdd>`, the user as the OS names it.
fn default_rule_name() -> String {
    let user = std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_default();
    rule_name_for(&user, &chrono::Local::now().format("%Y%m%d").to_string())
}

pub fn rule_name_for(user: &str, date: &str) -> String {
    let mut u = String::new();
    for c in user.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' };
        if !(c == '-' && (u.is_empty() || u.ends_with('-'))) {
            u.push(c);
        }
    }
    let u: String = u.trim_end_matches('-').chars().take(40).collect();
    let u = u.trim_end_matches('-');
    format!("DBine-{}-{date}", if u.is_empty() { "usuario" } else { u })
}

/// An Azure SQL server as `az sql server list` describes it.
#[derive(Debug, PartialEq)]
pub struct AzServer {
    pub name: String,
    pub resource_group: String,
}

/// The server whose `fullyQualifiedDomainName` is `host`, from `az sql
/// server list -o json`; values that aren't Azure names are dropped.
pub fn parse_servers(json: &[u8], host: &str) -> Vec<AzServer> {
    let Ok(list) = serde_json::from_slice::<Vec<serde_json::Value>>(json) else { return Vec::new() };
    list.iter()
        .filter(|s| s["fullyQualifiedDomainName"].as_str().is_some_and(|f| f.eq_ignore_ascii_case(host)))
        .filter_map(|s| {
            let name = s["name"].as_str()?;
            let rg = s["resourceGroup"].as_str()?;
            let name_ok = !name.is_empty() && name.len() <= 63 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
            let rg_ok = !rg.is_empty()
                && rg.len() <= 90
                && rg.chars().all(|c| c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '(' | ')'))
                && !rg.starts_with('-');
            (name_ok && rg_ok).then(|| AzServer { name: name.to_string(), resource_group: rg.to_string() })
        })
        .collect()
}

/// Subscription ids (GUIDs) from `az account list --query "[?state=='Enabled'].id" -o json`.
pub fn parse_subscriptions(json: &[u8]) -> Vec<String> {
    let Ok(list) = serde_json::from_slice::<Vec<String>>(json) else { return Vec::new() };
    list.into_iter()
        .filter(|s| s.len() == 36 && s.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-'))
        .take(MAX_SUBSCRIPTIONS)
        .collect()
}

/// The server in the current subscription, else in the others the login
/// sees (then with that subscription, for the create).
async fn find_server(bin: &Path, host: &str) -> CommandResult<(AzServer, Option<String>)> {
    let query = format!("[?fullyQualifiedDomainName=='{host}']");
    let out = run_az(bin, &["sql", "server", "list", "--query", &query, "-o", "json"], READ_LIMIT).await?;
    if !out.ok {
        let e = az_error(&out.stderr);
        if e.contains("az login") {
            return Err(CommandError::BadRequest("la CLI de Azure no tiene una sesión iniciada: corré «az login» y volvé a intentar".into()));
        }
        return Err(CommandError::BadRequest(format!("no se pudo listar los servidores de Azure SQL: {e}")));
    }
    if let Some(s) = parse_servers(&out.stdout, host).into_iter().next() {
        return Ok((s, None));
    }
    let subs = run_az(bin, &["account", "list", "--query", "[?state=='Enabled'].id", "-o", "json"], READ_LIMIT).await?;
    let current = run_az(bin, &["account", "show", "--query", "id", "-o", "tsv"], READ_LIMIT).await?;
    let current = String::from_utf8_lossy(&current.stdout).trim().to_string();
    for sub in parse_subscriptions(&subs.stdout).into_iter().filter(|s| *s != current) {
        let out = run_az(bin, &["sql", "server", "list", "--subscription", &sub, "--query", &query, "-o", "json"], READ_LIMIT).await?;
        if let Some(s) = out.ok.then(|| parse_servers(&out.stdout, host)).and_then(|v| v.into_iter().next()) {
            return Ok((s, Some(sub)));
        }
    }
    Err(CommandError::BadRequest(format!(
        "no se encontró el servidor {host} en las suscripciones de tu cuenta de Azure (la sesión de az puede ser de otra cuenta o de otro tenant)"
    )))
}

/// Where `az` is: on the PATH (the login shell's too) or where its
/// installers put it.
fn find_az() -> Option<PathBuf> {
    if let Some(p) = dbine_ai::env::find("az") {
        return Some(p);
    }
    let mut dirs: Vec<PathBuf> = vec!["/opt/homebrew/bin".into(), "/usr/local/bin".into(), "/usr/bin".into()];
    for var in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(pf) = std::env::var_os(var) {
            dirs.push(PathBuf::from(pf).join(r"Microsoft SDKs").join(r"Azure").join(r"CLI2").join(r"wbin"));
        }
    }
    let path = std::env::join_paths(&dirs).ok()?;
    dbine_ai::env::find_in("az", &path.to_string_lossy())
}

struct AzOutput {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

/// Run `az` with these arguments (no shell), within `limit`.
async fn run_az(bin: &Path, args: &[&str], limit: Duration) -> CommandResult<AzOutput> {
    let mut cmd = tokio::process::Command::new(bin);
    cmd.args(args)
        .env("PATH", dbine_ai::env::search_path())
        .env("AZURE_CORE_ONLY_SHOW_ERRORS", "true")
        .env("AZURE_CORE_NO_COLOR", "true")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console window next to the app.
        cmd.creation_flags(0x0800_0000);
    }
    let child = cmd.spawn().map_err(|e| CommandError::Internal(format!("no se pudo ejecutar la CLI de Azure (az): {e}")))?;
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(Ok(o)) => Ok(AzOutput { ok: o.status.success(), stdout: o.stdout, stderr: String::from_utf8_lossy(&o.stderr).trim().to_string() }),
        Ok(Err(e)) => Err(CommandError::Internal(format!("no se pudo ejecutar la CLI de Azure (az): {e}"))),
        Err(_) => Err(CommandError::Connect(format!("la CLI de Azure (az) no terminó en {} s", limit.as_secs()))),
    }
}

/// az's error, short: its last meaningful line.
fn az_error(stderr: &str) -> String {
    let line = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .find(|l| l.starts_with("ERROR:"))
        .or_else(|| stderr.lines().map(str::trim).rfind(|l| !l.is_empty()))
        .unwrap_or("error desconocido");
    let line = line.trim_start_matches("ERROR:").trim();
    line.chars().take(400).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn azure_hosts_are_recognized() {
        let h = |s: &str| azure_sql_host(s).map(|(_, server)| server);
        assert_eq!(h("ventas.database.windows.net").as_deref(), Some("ventas"));
        assert_eq!(h("tcp:Ventas-Prod.database.windows.net,1433").as_deref(), Some("ventas-prod"));
        assert_eq!(h("srv.database.usgovcloudapi.net").as_deref(), Some("srv"));
        assert_eq!(azure_sql_host("ventas.database.windows.net.").unwrap().0, "ventas.database.windows.net");
        // A VM (its NSG, not this firewall), other hosts and anything odd.
        assert_eq!(h("sqlvm.eastus.cloudapp.azure.com"), None);
        assert_eq!(h("localhost"), None);
        assert_eq!(h("a.b.database.windows.net"), None);
        assert_eq!(h(".database.windows.net"), None);
        assert_eq!(h("x'];drop.database.windows.net"), None);
        assert_eq!(h("ventas.database.windows.net.evil.com"), None);
    }

    #[test]
    fn only_public_ipv4_is_accepted() {
        assert_eq!(public_ipv4(" 203.0.113.7 ").unwrap().to_string(), "203.0.113.7");
        assert_eq!(public_ipv4("8.8.8.8").unwrap().to_string(), "8.8.8.8");
        for bad in [
            "10.0.0.1", "192.168.1.10", "172.16.0.1", "127.0.0.1", "169.254.1.1", "0.0.0.0", "255.255.255.255", "224.0.0.1",
            "100.64.0.1", "240.0.0.1", "1.2.3", "1.2.3.4.5", "01.2.3.4", "1.2.3.256", "::1", "2001:db8::1", "1.2.3.4; rm -rf /",
            "", "abc",
        ] {
            assert!(public_ipv4(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rule_names_are_safe() {
        assert_eq!(rule_name_for("marcos", "20261013"), "DBine-marcos-20261013");
        assert_eq!(rule_name_for("CONTOSO\\Ana María", "20261013"), "DBine-CONTOSO-Ana-Mar-a-20261013");
        assert_eq!(rule_name_for("", "20261013"), "DBine-usuario-20261013");
        assert_eq!(rule_name_for("--", "20261013"), "DBine-usuario-20261013");
        assert!(valid_rule_name(&rule_name_for("x y;z", "20261013")).is_ok());
        assert!(valid_rule_name("DBine-ok_1").is_ok());
        for bad in ["", "a b", "a;b", "a'b", "ñandú", &"x".repeat(129)] {
            assert!(valid_rule_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn az_server_list_is_parsed() {
        let json = br#"[
            {"name": "ventas", "resourceGroup": "rg-datos(prod)", "fullyQualifiedDomainName": "ventas.database.windows.net", "location": "eastus"},
            {"name": "otro", "resourceGroup": "rg", "fullyQualifiedDomainName": "otro.database.windows.net"},
            {"name": "Bad Name", "resourceGroup": "rg", "fullyQualifiedDomainName": "ventas.database.windows.net"},
            {"name": "ventas2", "resourceGroup": "-rg", "fullyQualifiedDomainName": "ventas.database.windows.net"}
        ]"#;
        assert_eq!(
            parse_servers(json, "ventas.database.windows.net"),
            vec![AzServer { name: "ventas".into(), resource_group: "rg-datos(prod)".into() }]
        );
        assert!(parse_servers(b"[]", "ventas.database.windows.net").is_empty());
        assert!(parse_servers(b"not json", "ventas.database.windows.net").is_empty());
    }

    #[test]
    fn subscriptions_are_parsed() {
        let json = br#"["0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0", "--subscription x", "nope"]"#;
        assert_eq!(parse_subscriptions(json), vec!["0b1c2d3e-4f50-6172-8394-a5b6c7d8e9f0".to_string()]);
        assert!(parse_subscriptions(b"{}").is_empty());
    }

    /// Against the machine's real `az` login, read-only (lists servers,
    /// creates nothing): `DBINE_AZ_HOST=<server>.database.windows.net
    /// cargo test -p dbine --lib finds_a_real_server -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn finds_a_real_server() {
        let host = std::env::var("DBINE_AZ_HOST").expect("DBINE_AZ_HOST");
        let bin = find_az().expect("az installed");
        let (server, sub) = find_server(&bin, &host).await.unwrap();
        assert!(host.starts_with(&server.name), "{server:?}");
        println!("found {} in {} (other subscription: {})", server.name, server.resource_group, sub.is_some());
    }

    #[test]
    fn az_errors_are_shortened() {
        let e = "WARNING: something\nERROR: (AuthorizationFailed) The client does not have authorization.\nCode: AuthorizationFailed";
        assert_eq!(az_error(e), "(AuthorizationFailed) The client does not have authorization.");
        assert_eq!(az_error("plain failure\n"), "plain failure");
        assert_eq!(az_error(""), "error desconocido");
    }

    #[test]
    fn only_the_same_ip_matches() {
        assert!(super::same_ip("203.0.113.7", " 203.0.113.7 "));
        assert!(!super::same_ip("203.0.113.7", "203.0.113.8"));
        assert!(!super::same_ip("203.0.113.7", "nope"));
    }
}
