//! One-off correction for the bbb (group) book: void 18 duplicate single-import
//! entries that double-posted the credit-card payments.
//!
//! What happened: after the 9 checking→2001 payments posted correctly as
//! `transfer:` entries, the 9 transfer *candidates* were rejected — which reset
//! their already-imported legs back to `pending` — and a later "Import All"
//! posted all 18 legs AGAIN as individual transactions against Uncategorized
//! (referenced by the bare Plaid id, so the `transfer:` dedup didn't catch them).
//! Net effect: Business Checking double-credited and Business Credit Card (2001)
//! double-debited by the payments total ($133,497.35).
//!
//! The fix is to void those 18 entries. bbb.db is a REPLICA of the remote group
//! server, so the voids must go THROUGH the server (a direct DB edit is wiped on
//! the next sync). This tool authenticates and calls the instance's
//! `void-entry` command for each id.
//!
//! Auth: if the desktop app has saved a refresh token
//! (`~/.local/state/accountir/session.json`), it is used silently; otherwise the
//! tool logs in (password, then an MFA code if the account has one). Password is
//! read from `$ACCOUNTIR_PASSWORD` if set, else prompted.
//!
//! Usage:
//!   cargo run --example void_bbb_duplicates -- <user_id>            # dry run
//!   cargo run --example void_bbb_duplicates -- <user_id> --apply    # void via server

use accountir::sync::SyncClient;

/// The 18 duplicate entries (9 checking→Uncategorized + 9 → 2001), all posted
/// after head 768 with bare-Plaid-id references. Voiding exactly these restores
/// checking to $7,125.06, 2001 to −$11,241.88, Uncategorized to $4,116.82.
const DUPLICATE_ENTRY_IDS: &[&str] = &[
    // checking → Uncategorized
    "e4bfd2e6-587a-47df-8ba1-aef6d3d3039a",
    "1d2c7d50-05d0-4d13-b915-93e794bb3efa",
    "25f0519b-5001-4c46-9d27-099e69f23252",
    "d8b1fede-bc6e-48aa-b47a-4b540039c76a",
    "bab451c2-09e2-4c55-9ec7-8fedb0aebebf",
    "67792b56-61de-4ee6-99cd-fb501c2547b5",
    "d2d639aa-96e6-4cd8-a023-95e0637725d0",
    "5b2628a8-9794-4b59-9db3-94e003fa243a",
    "d0c557af-43d2-494a-bd00-00d3a48a833b",
    // 2001 (parent card) → Uncategorized
    "0cdb0097-3e26-4ea7-b0cd-345287c53e59",
    "5892c7f3-0bc0-4184-9c72-0a76ad0e1a4e",
    "5355b070-53b8-49e1-8ea0-bf4ec5a1eaef",
    "08b1db6a-df41-4521-8857-f66a590c32d5",
    "9545c297-c85b-46f1-92fe-a887551ea04b",
    "700f5916-a6fc-4705-90bb-e91ce01f369d",
    "a8e95ff7-4e90-4aaa-8f4c-bc464110b4a2",
    "b07d3662-b2ff-4d07-8525-77cdcf68573e",
    "b5f85b67-65dd-46c3-bf0b-d47fe916f4e3",
];

const DB_PATH: &str = "/home/zak/accounting/server/bbb.db";

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let user_id = args
        .get(1)
        .cloned()
        .expect("usage: void_bbb_duplicates <user_id> [--apply]");
    let apply = args.iter().any(|a| a == "--apply");

    let conn = rusqlite::Connection::open(DB_PATH)?;
    let (instance_url, cp_url, group): (String, String, String) = conn.query_row(
        "SELECT instance_url, control_plane_url, group_id FROM group_binding LIMIT 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
    )?;

    println!("instance:      {instance_url}");
    println!("control plane: {cp_url}");
    println!("group:         {group}");
    println!("\n{} duplicate entries to void:", DUPLICATE_ENTRY_IDS.len());
    for id in DUPLICATE_ENTRY_IDS {
        println!("  {id}");
    }

    if !apply {
        println!("\n(dry run — re-run with --apply to void these through the server)");
        return Ok(());
    }

    let token = obtain_token(&cp_url, &user_id, &group).await?;

    let mut client = SyncClient::new(instance_url, token);
    let reason = "Void duplicate single-import: credit-card payments double-posted (see recovery notes)";
    let (mut ok, mut failed) = (0usize, 0usize);
    for id in DUPLICATE_ENTRY_IDS {
        match client.void_entry(*id, reason).await {
            Ok(head) => {
                ok += 1;
                println!("voided {id}  (server head {head})");
            }
            Err(e) => {
                failed += 1;
                println!("FAILED {id}: {e}");
            }
        }
    }
    println!("\ndone: {ok} voided, {failed} failed");
    if failed == 0 {
        println!("Reopen the desktop app; after it syncs, checking should read $7,125.06 and 2001 $11,241.88 owed.");
    }
    Ok(())
}

/// A control-plane access token: from a saved refresh token if there is one, else
/// by logging in.
async fn obtain_token(
    cp_url: &str,
    user_id: &str,
    group: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let http = reqwest::Client::new();

    // 1) Saved refresh token (from the desktop app), if present.
    if let Some(refresh) = saved_refresh_token() {
        let resp: serde_json::Value = http
            .post(format!("{}/auth/refresh", cp_url.trim_end_matches('/')))
            .json(&serde_json::json!({ "refresh_token": refresh }))
            .send()
            .await?
            .json()
            .await
            .unwrap_or(serde_json::Value::Null);
        if let Some(t) = resp.get("token").and_then(|t| t.as_str()) {
            println!("(authenticated with the saved refresh token)");
            return Ok(t.to_string());
        }
        eprintln!("saved refresh token didn't work; falling back to password login");
    }

    // 2) Password login (+ MFA if the account has it).
    let password = std::env::var("ACCOUNTIR_PASSWORD")
        .ok()
        .unwrap_or_else(|| prompt("Password: ").unwrap_or_default());
    let resp: serde_json::Value = http
        .post(format!("{}/auth/login", cp_url.trim_end_matches('/')))
        .json(&serde_json::json!({ "user_id": user_id, "password": password, "group": group }))
        .send()
        .await?
        .json()
        .await?;

    if resp.get("mfa_required").and_then(|m| m.as_bool()).unwrap_or(false) {
        let challenge = resp
            .get("challenge")
            .and_then(|c| c.as_str())
            .ok_or("MFA required but no challenge returned")?
            .to_string();
        let code = prompt("Verification code: ")?;
        let mfa: serde_json::Value = http
            .post(format!("{}/auth/login/mfa", cp_url.trim_end_matches('/')))
            .json(&serde_json::json!({
                "user_id": user_id, "group": group, "challenge": challenge, "code": code.trim()
            }))
            .send()
            .await?
            .json()
            .await?;
        return Ok(mfa
            .get("token")
            .and_then(|t| t.as_str())
            .ok_or("no token after MFA — check the code and try again")?
            .to_string());
    }

    Ok(resp
        .get("token")
        .and_then(|t| t.as_str())
        .ok_or("login failed — wrong user id or password")?
        .to_string())
}

/// The desktop app's saved refresh token, if any.
fn saved_refresh_token() -> Option<String> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/state")))?;
    let path = base.join("accountir").join("session.json");
    let bytes = std::fs::read(path).ok()?;
    let v: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    v.get("refresh_token").and_then(|t| t.as_str()).map(str::to_string)
}

fn prompt(p: &str) -> std::io::Result<String> {
    use std::io::Write;
    print!("{p}");
    std::io::stdout().flush()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s)?;
    Ok(s.trim_end().to_string())
}
