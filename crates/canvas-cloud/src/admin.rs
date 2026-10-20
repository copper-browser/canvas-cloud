//! `canvas-cloud admin …` and `canvas-cloud apps …`: direct database administration. Every
//! change is recorded in `admin_audit` (actor `cli`).

use std::io::{BufRead as _, IsTerminal as _, Write as _};

use anyhow::{anyhow, bail, Context as _};
use canvas_cloud_core::config::Config;
use canvas_cloud_core::identity::{self, NewUser};
use serde_json::{json, Value};
use sqlx::PgPool;
use time::format_description::well_known::Rfc3339;
use uuid::Uuid;

use crate::cli::{AdminCommand, AppsCommand, SettingsCommand, UsersCommand};

const ACTOR: &str = "cli";

async fn connect(cfg: &Config) -> anyhow::Result<PgPool> {
    let pool = canvas_cloud_core::db::connect(cfg).await?;
    let (_, pending) = canvas_cloud_core::db::pending_migrations(&pool).await?;
    if !pending.is_empty() {
        bail!(
            "the database has pending migrations ({}) — run `canvas-cloud migrate` first",
            pending.join(", ")
        );
    }
    Ok(pool)
}

/// `canvas-cloud admin …`.
pub async fn admin(cfg: &Config, cmd: AdminCommand) -> anyhow::Result<()> {
    let pool = connect(cfg).await?;
    let result = match cmd {
        AdminCommand::Users(cmd) => users(&pool, cmd).await,
        AdminCommand::Settings(cmd) => settings(&pool, cmd).await,
    };
    pool.close().await;
    result
}

/// `canvas-cloud apps …`.
pub async fn apps(cfg: &Config, cmd: AppsCommand) -> anyhow::Result<()> {
    let pool = connect(cfg).await?;
    let result = apps_cmd(&pool, cmd).await;
    pool.close().await;
    result
}

/// The app named by `reference`: an id, or a name (case-insensitive). When several apps share
/// the name, the one live app among them; otherwise the id is required.
async fn resolve_app(pool: &PgPool, reference: &str) -> anyhow::Result<(Uuid, String)> {
    let reference = reference.trim();
    if let Ok(id) = Uuid::parse_str(reference) {
        let name: Option<String> = sqlx::query_scalar("SELECT name FROM apps WHERE id = $1")
            .bind(id)
            .fetch_optional(pool)
            .await?;
        return name
            .map(|name| (id, name))
            .ok_or_else(|| anyhow!("no app {id}"));
    }
    let rows: Vec<(Uuid, String, bool)> = sqlx::query_as(
        "SELECT id, name, revoked_at IS NULL FROM apps WHERE lower(name) = lower($1)
         ORDER BY created_at DESC, id DESC",
    )
    .bind(reference)
    .fetch_all(pool)
    .await?;
    let live: Vec<&(Uuid, String, bool)> = rows.iter().filter(|r| r.2).collect();
    match (rows.as_slice(), live.as_slice()) {
        ([], _) => bail!("no app named {reference:?} (see `canvas-cloud apps list`)"),
        ([(id, name, _)], _) | (_, [(id, name, _)]) => Ok((*id, name.clone())),
        _ => bail!(
            "{} apps are named {reference:?}; give the id: {}",
            rows.len(),
            rows.iter()
                .map(|r| r.0.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// `canvas-cloud apps set <app> [--allow-import true|false] [--trust-email true|false]
/// [--allow-sessions true|false]`.
async fn set_app(pool: &PgPool, reference: &str, flags: AppFlagChanges) -> anyhow::Result<()> {
    if flags.allow_import.is_none() && flags.trust_email.is_none() && flags.allow_sessions.is_none()
    {
        bail!(
            "nothing to set (use --allow-import, --trust-email and/or --allow-sessions true|false)"
        );
    }
    let (id, name) = resolve_app(pool, reference).await?;
    let row: Option<(bool, bool, bool)> = sqlx::query_as(
        "UPDATE apps SET allow_import = coalesce($2, allow_import),
                         trust_email = coalesce($3, trust_email),
                         allow_sessions = coalesce($4, allow_sessions)
         WHERE id = $1 RETURNING allow_import, trust_email, allow_sessions",
    )
    .bind(id)
    .bind(flags.allow_import)
    .bind(flags.trust_email)
    .bind(flags.allow_sessions)
    .fetch_optional(pool)
    .await?;
    let (allow_import, trust_email, allow_sessions) = row.ok_or_else(|| anyhow!("no app {id}"))?;
    identity::audit(
        pool,
        ACTOR,
        "app.set",
        &id.to_string(),
        json!({
            "allow_import": allow_import,
            "trust_email": trust_email,
            "allow_sessions": allow_sessions,
        }),
    )
    .await;
    println!(
        "app {id} ({name}): allow_import={allow_import} trust_email={trust_email} allow_sessions={allow_sessions}"
    );
    if allow_sessions && !trust_email {
        eprintln!("note: the app cannot sign users in until trust_email is true as well");
    }
    if flags.allow_sessions == Some(false) {
        eprintln!(
            "note: sessions it already minted stay signed in; `canvas-cloud apps rotate {id}` ends them"
        );
    }
    Ok(())
}

/// The `--allow-import` / `--trust-email` / `--allow-sessions` of `apps set` (`None` = keep).
#[derive(Clone, Copy, Debug, Default)]
struct AppFlagChanges {
    allow_import: Option<bool>,
    trust_email: Option<bool>,
    allow_sessions: Option<bool>,
}

fn yes_no(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

/// Why an app can or cannot sign its users in, for `apps show`.
fn session_status(a: &identity::AppRow) -> &'static str {
    match (a.revoked_at.is_some(), a.allow_sessions, a.trust_email) {
        (true, _, _) => "no (revoked)",
        (false, true, true) => "yes",
        (false, true, false) => "no (allow_sessions is on but trust_email is off)",
        (false, false, _) => "no (allow_sessions is off)",
    }
}

async fn find_user(pool: &PgPool, email: &str) -> anyhow::Result<Uuid> {
    identity::user_id_by_email(pool, email.trim())
        .await?
        .ok_or_else(|| anyhow!("no user with email {email}"))
}

fn ts(t: time::OffsetDateTime) -> String {
    t.format(&Rfc3339).unwrap_or_default()
}

type UserListRow = (Uuid, String, String, bool, bool, time::OffsetDateTime, i64);

#[allow(clippy::too_many_lines)] // one arm per subcommand
async fn users(pool: &PgPool, cmd: UsersCommand) -> anyhow::Result<()> {
    match cmd {
        UsersCommand::List => {
            let rows: Vec<UserListRow> = sqlx::query_as(
                "SELECT u.id, u.email, u.display_name, u.disabled, u.password_hash IS NOT NULL,
                        u.created_at,
                        (SELECT count(*) FROM canvases c WHERE c.owner_id = u.id)
                 FROM users u ORDER BY u.created_at, u.id",
            )
            .fetch_all(pool)
            .await?;
            println!(
                "{:<36}  {:<32}  {:<20}  {:<8}  {:<8}  {:>8}  created",
                "id", "email", "name", "status", "login", "canvases"
            );
            for (id, email, name, disabled, password, created, canvases) in &rows {
                println!(
                    "{id}  {email:<32}  {name:<20}  {:<8}  {:<8}  {canvases:>8}  {}",
                    if *disabled { "disabled" } else { "active" },
                    if *password { "password" } else { "linked" },
                    ts(*created),
                );
            }
            println!("{} user(s)", rows.len());
        }
        UsersCommand::Create {
            email,
            password,
            display_name,
        } => {
            let email = identity::normalize_email(&email).map_err(api_err)?;
            let pw = password.read()?;
            identity::validate_password(&pw).map_err(api_err)?;
            let hash = identity::hash_password(pw).await.map_err(api_err)?;
            let name = identity::clean_name(display_name.as_deref(), 200);
            let id = identity::insert_user(
                pool,
                &NewUser {
                    email: &email,
                    display_name: name.as_deref(),
                    password_hash: Some(&hash),
                    ..NewUser::default()
                },
            )
            .await
            .map_err(api_err)?;
            identity::audit(
                pool,
                ACTOR,
                "user.create",
                &id.to_string(),
                json!({ "email": email }),
            )
            .await;
            println!("created {email} ({id})");
        }
        UsersCommand::Disable { email } => {
            let id = find_user(pool, &email).await?;
            let mut tx = pool.begin().await?;
            sqlx::query("UPDATE users SET disabled = true, updated_at = now() WHERE id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?;
            let sessions = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            let tokens = sqlx::query(
                "UPDATE access_tokens SET revoked_at = now()
                 WHERE user_id = $1 AND revoked_at IS NULL",
            )
            .bind(id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            tx.commit().await?;
            identity::audit(pool, ACTOR, "user.disable", &id.to_string(), json!({})).await;
            println!("disabled {email}; revoked {sessions} session(s), {tokens} token(s)");
        }
        UsersCommand::Enable { email } => {
            let id = find_user(pool, &email).await?;
            sqlx::query("UPDATE users SET disabled = false, updated_at = now() WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            identity::audit(pool, ACTOR, "user.enable", &id.to_string(), json!({})).await;
            println!("enabled {email}");
        }
        UsersCommand::Delete { email, yes } => {
            let id = find_user(pool, &email).await?;
            let owned: i64 =
                sqlx::query_scalar("SELECT count(*) FROM canvases WHERE owner_id = $1")
                    .bind(id)
                    .fetch_one(pool)
                    .await?;
            if !yes
                && !confirm(&format!(
                    "Delete {email} and the {owned} canvas(es) they own? [y/N] "
                ))?
            {
                bail!("aborted");
            }
            sqlx::query("DELETE FROM users WHERE id = $1")
                .bind(id)
                .execute(pool)
                .await?;
            identity::audit(
                pool,
                ACTOR,
                "user.delete",
                &id.to_string(),
                json!({ "email": email, "canvases": owned }),
            )
            .await;
            println!("deleted {email} and {owned} canvas(es)");
        }
        UsersCommand::SetPassword { email, password } => {
            let id = find_user(pool, &email).await?;
            let pw = password.read()?;
            identity::validate_password(&pw).map_err(api_err)?;
            let hash = identity::hash_password(pw).await.map_err(api_err)?;
            let mut tx = pool.begin().await?;
            sqlx::query("UPDATE users SET password_hash = $2, updated_at = now() WHERE id = $1")
                .bind(id)
                .bind(&hash)
                .execute(&mut *tx)
                .await?;
            let revoked = sqlx::query("DELETE FROM sessions WHERE user_id = $1")
                .bind(id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            tx.commit().await?;
            identity::audit(pool, ACTOR, "user.set_password", &id.to_string(), json!({})).await;
            println!("password set for {email}; revoked {revoked} session(s)");
        }
    }
    Ok(())
}

/// `allow_signup` value: true/false (also yes/no/on/off/1/0).
fn parse_bool(s: &str) -> anyhow::Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => bail!("expected true or false, got {s:?}"),
    }
}

/// `signup_domains` value: comma-separated domains (`@` prefixes dropped, lower-cased).
fn parse_domains(s: &str) -> anyhow::Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for d in s.split(',') {
        let d = d.trim().trim_start_matches('@').to_ascii_lowercase();
        if d.is_empty() {
            continue;
        }
        let ok = d.len() <= 253
            && d.contains('.')
            && d.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
        if !ok {
            bail!("{d:?} is not a domain");
        }
        if !out.contains(&d) {
            out.push(d);
        }
    }
    Ok(out)
}

async fn settings(pool: &PgPool, cmd: SettingsCommand) -> anyhow::Result<()> {
    match cmd {
        SettingsCommand::Get => {
            let policy = identity::SignupPolicy::load(pool).await.map_err(api_err)?;
            println!("allow_signup = {}", policy.allow_signup);
            println!("signup_domains = {}", policy.signup_domains.join(","));
            if policy.first_user {
                println!("(no password account yet: the first signup is always allowed)");
            }
        }
        SettingsCommand::Set { key, value } => {
            let v: Value = match key.as_str() {
                "allow_signup" => json!(parse_bool(&value)?),
                "signup_domains" => json!(parse_domains(&value)?),
                other => bail!("unknown setting {other}"),
            };
            identity::set_setting(pool, &key, &v)
                .await
                .map_err(api_err)?;
            identity::audit(pool, ACTOR, "settings.set", &key, json!({ "value": v })).await;
            println!("{key} = {v}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_lines)] // one arm per subcommand
async fn apps_cmd(pool: &PgPool, cmd: AppsCommand) -> anyhow::Result<()> {
    match cmd {
        AppsCommand::Create {
            name,
            allow_import,
            no_trust_email,
            allow_sessions,
        } => {
            if allow_sessions && no_trust_email {
                bail!("--allow-sessions needs trust_email (drop --no-trust-email)");
            }
            let flags = identity::AppFlags {
                allow_import,
                trust_email: !no_trust_email,
                allow_sessions,
            };
            let (id, key) = identity::create_app_with(pool, &name, flags)
                .await
                .map_err(api_err)?;
            identity::audit(
                pool,
                ACTOR,
                "app.create",
                &id.to_string(),
                json!({
                    "name": name,
                    "allow_import": flags.allow_import,
                    "trust_email": flags.trust_email,
                    "allow_sessions": flags.allow_sessions,
                }),
            )
            .await;
            eprintln!("created app {id} ({name}); its key is shown once:");
            println!("{key}");
        }
        AppsCommand::List => {
            let rows = identity::list_apps(pool).await.map_err(api_err)?;
            // Canvases owned by each app's users.
            let canvases: std::collections::HashMap<Uuid, i64> = sqlx::query_as(
                "SELECT au.app_id, count(c.id) FROM app_users au
                 JOIN canvases c ON c.owner_id = au.user_id GROUP BY au.app_id",
            )
            .fetch_all(pool)
            .await?
            .into_iter()
            .collect();
            // `sessions`: may sign its users in (allow_sessions + trust_email, live);
            // `minted`: live sessions it minted / live access tokens created with them.
            println!(
                "{:<36}  {:<24}  {:<7}  {:<6}  {:<6}  {:<8}  {:>6}  {:>8}  {:>9}  last used",
                "id",
                "name",
                "status",
                "import",
                "email",
                "sessions",
                "users",
                "canvases",
                "minted"
            );
            for a in &rows {
                println!(
                    "{}  {:<24}  {:<7}  {:<6}  {:<6}  {:<8}  {:>6}  {:>8}  {:>9}  {}",
                    a.id,
                    a.name,
                    if a.revoked_at.is_some() {
                        "revoked"
                    } else {
                        "active"
                    },
                    yes_no(a.allow_import),
                    if a.trust_email { "trust" } else { "strict" },
                    yes_no(a.can_mint_sessions()),
                    a.users,
                    canvases.get(&a.id).copied().unwrap_or(0),
                    format!("{}/{}", a.live_sessions, a.live_tokens),
                    a.last_used_at.map_or_else(|| "never".into(), ts),
                );
            }
            println!("{} app(s)", rows.len());
        }
        AppsCommand::Show { app } => {
            let (id, _) = resolve_app(pool, &app).await?;
            let a = identity::get_app(pool, id)
                .await
                .map_err(api_err)?
                .ok_or_else(|| anyhow!("no app {id}"))?;
            println!("id              {}", a.id);
            println!("name            {}", a.name);
            println!(
                "status          {}",
                a.revoked_at
                    .map_or_else(|| "active".into(), |t| format!("revoked {}", ts(t)))
            );
            println!("allow_import    {}", a.allow_import);
            println!("trust_email     {}", a.trust_email);
            println!("allow_sessions  {}", a.allow_sessions);
            println!("can sign in     {}", session_status(&a));
            println!("users           {}", a.users);
            println!("minted sessions {} live", a.live_sessions);
            println!("minted tokens   {} live", a.live_tokens);
            println!("created         {}", ts(a.created_at));
            println!(
                "last used       {}",
                a.last_used_at.map_or_else(|| "never".into(), ts)
            );
        }
        AppsCommand::Revoke { app } => {
            let (id, name) = resolve_app(pool, &app).await?;
            let (was_live, ended) = identity::revoke_app(pool, id)
                .await
                .map_err(api_err)?
                .ok_or_else(|| anyhow!("no app {id}"))?;
            identity::audit(
                pool,
                ACTOR,
                "app.revoke",
                &id.to_string(),
                json!({
                    "already_revoked": !was_live,
                    "sessions_ended": ended.sessions,
                    "tokens_revoked": ended.tokens,
                }),
            )
            .await;
            println!(
                "{} app {id} ({name}); ended {} session(s) it minted and revoked {} access token(s) created with them",
                if was_live { "revoked" } else { "already revoked:" },
                ended.sessions,
                ended.tokens,
            );
        }
        AppsCommand::Set {
            app,
            allow_import,
            trust_email,
            allow_sessions,
        } => {
            set_app(
                pool,
                &app,
                AppFlagChanges {
                    allow_import,
                    trust_email,
                    allow_sessions,
                },
            )
            .await?;
        }
        AppsCommand::Rotate { app, keep_sessions } => {
            let (id, name) = resolve_app(pool, &app).await?;
            let (key, ended) = identity::rotate_app_key(pool, id, !keep_sessions)
                .await
                .map_err(api_err)?
                .ok_or_else(|| anyhow!("no app {id}"))?;
            identity::audit(
                pool,
                ACTOR,
                "app.rotate",
                &id.to_string(),
                json!({
                    "keep_sessions": keep_sessions,
                    "sessions_ended": ended.sessions,
                    "tokens_revoked": ended.tokens,
                }),
            )
            .await;
            if keep_sessions {
                eprintln!(
                    "rotated app {id} ({name}); kept the sessions it minted (--keep-sessions)"
                );
            } else {
                eprintln!(
                    "rotated app {id} ({name}); ended {} session(s) it minted and revoked {} access token(s) created with them",
                    ended.sessions, ended.tokens
                );
            }
            eprintln!("the new key is shown once:");
            println!("{key}");
        }
    }
    Ok(())
}

/// Surface an API error's message (e.g. `bad_request` details) on the CLI.
#[allow(clippy::needless_pass_by_value)] // used as `map_err(api_err)`
fn api_err(e: canvas_cloud_core::error::ApiError) -> anyhow::Error {
    use canvas_cloud_core::error::ApiError;
    match e {
        ApiError::BadRequest(msg) => anyhow!(msg),
        ApiError::Conflict(detail) => {
            anyhow!("{}", detail["message"].as_str().unwrap_or("conflict"))
        }
        other => anyhow!(other.to_string()),
    }
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("refusing to delete without a terminal; pass --yes");
    }
    print!("{prompt}");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .context("reading confirmation")?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setting_values() {
        assert!(parse_bool("TRUE").unwrap());
        assert!(!parse_bool(" off ").unwrap());
        assert!(parse_bool("maybe").is_err());
        assert_eq!(
            parse_domains("Example.com, @b.co,,example.com").unwrap(),
            ["example.com", "b.co"]
        );
        assert_eq!(parse_domains(" , ").unwrap(), Vec::<String>::new());
        assert!(parse_domains("not a domain").is_err());
        assert!(parse_domains("localhost").is_err());
    }
}
