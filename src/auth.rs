//! Microsoft device-code login, token refresh, and local account storage.

use crate::cli::AccountAction;
use crate::config::{msa_client_id, Paths, Result, MSA_SCOPE};
use crate::http::{get_json, post_form, post_json};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AuthData {
    pub username: String,
    pub uuid: String,
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: u64,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
pub struct AuthStore {
    pub active_account: Option<String>,
    pub accounts: HashMap<String, AuthData>,
}

fn now_epoch() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

pub fn load_auth_store(paths: &Paths) -> AuthStore {
    if let Ok(content) = fs::read_to_string(&paths.auth_file) {
        if let Ok(store) = serde_json::from_str::<AuthStore>(&content) {
            return store;
        }
        if let Ok(old_auth) = serde_json::from_str::<AuthData>(&content) {
            let mut store = AuthStore {
                active_account: Some(old_auth.username.clone()),
                ..AuthStore::default()
            };
            store.accounts.insert(old_auth.username.clone(), old_auth);
            return store;
        }
    }
    AuthStore::default()
}

pub fn save_auth_store(paths: &Paths, store: &AuthStore) -> Result<()> {
    if let Some(p) = paths.auth_file.parent() {
        fs::create_dir_all(p)?;
    }
    fs::write(&paths.auth_file, serde_json::to_string_pretty(store)?)?;
    Ok(())
}

async fn authenticate_tokens(
    client: &Client,
    msa_token: &str,
    refresh_token: &str,
) -> Result<AuthData> {
    // Microsoft access token -> Xbox Live -> XSTS -> Minecraft access token.
    // Minecraft itself receives only the final Minecraft services token.
    println!("  -> Authenticating with Xbox Live...");
    let rps = if msa_token.starts_with("t=") {
        msa_token.to_string()
    } else {
        format!("t={}", msa_token)
    };
    let xbl_res = post_json(
        client,
        "https://user.auth.xboxlive.com/user/authenticate",
        &json!({
            "Properties": { "AuthMethod": "RPS", "SiteName": "user.auth.xboxlive.com", "RpsTicket": rps },
            "RelyingParty": "http://auth.xboxlive.com",
            "TokenType": "JWT"
        }),
    )
    .await?;

    let xbl_token = xbl_res["Token"].as_str().ok_or("Xbox Live Auth failed")?;
    let user_hash = xbl_res["DisplayClaims"]["xui"][0]["uhs"]
        .as_str()
        .ok_or("Missing uhs")?;

    println!("  -> Requesting XSTS Token...");
    let xsts_res = post_json(
        client,
        "https://xsts.auth.xboxlive.com/xsts/authorize",
        &json!({
            "Properties": { "SandboxId": "RETAIL", "UserTokens": [xbl_token] },
            "RelyingParty": "rp://api.minecraftservices.com/",
            "TokenType": "JWT"
        }),
    )
    .await?;

    let xsts_token = xsts_res["Token"].as_str().ok_or("XSTS Auth failed")?;

    println!("  -> Logging into Minecraft Services...");
    let mc_res = post_json(
        client,
        "https://api.minecraftservices.com/authentication/login_with_xbox",
        &json!({
            "identityToken": format!("XBL3.0 x={};{}", user_hash, xsts_token)
        }),
    )
    .await?;

    let mc_token = mc_res["access_token"]
        .as_str()
        .ok_or("Minecraft Auth failed")?;
    let expires_in = mc_res["expires_in"].as_u64().unwrap_or(86400);

    println!("  -> Fetching Minecraft Profile...");
    let profile_res = get_json(
        client,
        "https://api.minecraftservices.com/minecraft/profile",
        Some(mc_token),
    )
    .await?;

    Ok(AuthData {
        username: profile_res["name"]
            .as_str()
            .ok_or("Missing username")?
            .to_string(),
        uuid: profile_res["id"]
            .as_str()
            .ok_or("Account does not own Minecraft!")?
            .to_string(),
        access_token: mc_token.to_string(),
        refresh_token: refresh_token.to_string(),
        expires_at: now_epoch() + expires_in.saturating_sub(60),
    })
}

pub async fn login_microsoft(client: &Client, paths: &Paths) -> Result<()> {
    let client_id = msa_client_id();
    println!("=== Microsoft Account Login ===");
    let code_res = post_form(
        client,
        "https://login.live.com/oauth20_connect.srf",
        &[
            ("client_id", client_id.as_str()),
            ("scope", MSA_SCOPE),
            ("response_type", "device_code"),
        ],
    )
    .await?;

    let device_code = code_res["device_code"]
        .as_str()
        .ok_or("Failed to get device_code")?;
    let user_code = code_res["user_code"]
        .as_str()
        .ok_or("Failed to get user_code")?;
    let uri = code_res["verification_uri"]
        .as_str()
        .unwrap_or("https://microsoft.com/link");
    let mut interval = code_res["interval"].as_u64().unwrap_or(5);

    println!("\n[!] Open this URL in your browser: {}", uri);
    println!("[!] Enter the code: {}\n", user_code);
    println!("Waiting for you to sign in...");

    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let token_res = post_form(
            client,
            "https://login.live.com/oauth20_token.srf",
            &[
                ("client_id", client_id.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("device_code", device_code),
            ],
        )
        .await?;

        if let Some(access_token) = token_res["access_token"].as_str() {
            let refresh = token_res["refresh_token"].as_str().unwrap_or("");
            let auth_data = authenticate_tokens(client, access_token, refresh).await?;

            let mut store = load_auth_store(paths);
            let user = auth_data.username.clone();
            store.active_account = Some(user.clone());
            store.accounts.insert(user.clone(), auth_data);
            save_auth_store(paths, &store)?;

            println!("\n[+] Successfully logged in as {}!", user);
            return Ok(());
        } else if token_res["error"].as_str() == Some("authorization_pending") {
            continue;
        } else if token_res["error"].as_str() == Some("slow_down") {
            interval += 5;
            continue;
        } else {
            return Err(format!("Login failed: {}", token_res).into());
        }
    }
}

pub async fn get_auth(
    client: &Client,
    paths: &Paths,
    target_account: Option<&str>,
) -> Option<AuthData> {
    let mut store = load_auth_store(paths);
    let target = target_account
        .map(|s| s.to_string())
        .or_else(|| store.active_account.clone())?;
    let auth = store.accounts.get(&target)?.clone();

    // Avoid a network request while the cached Minecraft token is still valid.
    if now_epoch() < auth.expires_at {
        return Some(auth);
    }

    if !auth.refresh_token.is_empty() {
        let client_id = msa_client_id();
        println!("[!] Refreshing Microsoft session for {}...", target);
        match post_form(
            client,
            "https://login.live.com/oauth20_token.srf",
            &[
                ("client_id", client_id.as_str()),
                ("scope", MSA_SCOPE),
                ("grant_type", "refresh_token"),
                ("refresh_token", &auth.refresh_token),
            ],
        )
        .await
        {
            // Only delete credentials when Microsoft conclusively says the
            // refresh token is invalid; transient network failures are retained.
            Ok(token_res) if token_res["error"].as_str() == Some("invalid_grant") => {
                store.accounts.remove(&target);
                if store.active_account.as_deref() == Some(&target) {
                    store.active_account = store.accounts.keys().next().cloned();
                }
                if let Err(error) = save_auth_store(paths, &store) {
                    eprintln!("[!] Could not remove expired account '{target}': {error}");
                }
                println!("[!] Session for {target} expired. Run 'mcli account login' again.");
            }
            Ok(token_res) => {
                if let Some(access_token) = token_res["access_token"].as_str() {
                    let new_refresh = token_res["refresh_token"]
                        .as_str()
                        .unwrap_or(&auth.refresh_token);
                    if let Ok(new_auth) =
                        authenticate_tokens(client, access_token, new_refresh).await
                    {
                        store.accounts.insert(target, new_auth.clone());
                        let _ = save_auth_store(paths, &store);
                        return Some(new_auth);
                    }
                }
            }
            Err(error) => eprintln!("[!] Could not refresh session for {target}: {error}"),
        }
        println!(
            "[!] Warning: Could not refresh token for {}. Run 'mcli account login' again.",
            target
        );
    }
    None
}

pub async fn handle_account_cmd(
    action: AccountAction,
    client: &Client,
    paths: &Paths,
) -> Result<()> {
    let mut store = load_auth_store(paths);
    match action {
        AccountAction::Login => login_microsoft(client, paths).await?,
        AccountAction::List => {
            println!("=== Microsoft Accounts ===");
            if store.accounts.is_empty() {
                println!("No accounts logged in. (Offline Mode)");
            }
            for (name, data) in &store.accounts {
                let active = if store.active_account.as_deref() == Some(name) {
                    "[ACTIVE]"
                } else {
                    ""
                };
                println!("- {} (UUID: {}) {}", name, data.uuid, active);
            }
        }
        AccountAction::Switch { name } => {
            if store.accounts.contains_key(&name) {
                store.active_account = Some(name.clone());
                save_auth_store(paths, &store)?;
                println!("[+] Switched active account to: {}", name);
            } else {
                println!(
                    "[!] Account '{}' not found. Run 'mcli account login' first.",
                    name
                );
            }
        }
        AccountAction::Logout { all } => {
            if all || store.accounts.is_empty() {
                if paths.auth_file.exists() {
                    fs::remove_file(&paths.auth_file)?;
                }
                println!("[+] Removed all saved accounts.");
            } else if let Some(active) = store.active_account.take() {
                store.accounts.remove(&active);
                store.active_account = store.accounts.keys().next().cloned();
                save_auth_store(paths, &store)?;
                println!(
                    "[+] Logged out of {}. {} account(s) remaining.",
                    active,
                    store.accounts.len()
                );
            }
        }
    }
    Ok(())
}
