//! Command-line interface.

// Doc comments here are `--help` text, so no Markdown backticks.
#![allow(clippy::doc_markdown)]

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{bail, Context as _};
use canvas_cloud_core::config::{self, Config, ConfigSource, GenerateOptions, TlsMode};
use clap::{Args, Parser, Subcommand};
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(
    name = "canvas-cloud",
    version = crate_version(),
    about = "Canvas Cloud: a self-hostable, real-time collaborative canvas server",
    long_about = None
)]
pub struct Cli {
    /// Config file (default /etc/canvas-cloud/canvas-cloud.toml; env CANVAS_CLOUD_CONFIG).
    #[arg(long, global = true, value_name = "PATH")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

fn crate_version() -> &'static str {
    static V: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    V.get_or_init(crate::version)
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the server (default).
    Serve(ServeArgs),
    /// Apply pending database migrations and exit.
    Migrate,
    /// Check config, database, migrations, TLS, the listener and metrics; non-zero exit on
    /// failure.
    Doctor,
    /// Wait until the local server answers /healthz (used by install.sh).
    Healthcheck {
        /// Seconds to keep retrying.
        #[arg(long, default_value_t = 0)]
        wait: u64,
    },
    /// Create the self-signed certificate (tls.cert_path / tls.key_path) if missing.
    TlsInit {
        /// Replace an existing certificate.
        #[arg(long)]
        force: bool,
        /// Extra SAN (DNS name or IP); repeatable. The public_url host is always included.
        #[arg(long = "name", value_name = "NAME")]
        names: Vec<String>,
    },
    /// Write a new config file (mode 0600) with a freshly generated master key.
    GenConfig(GenConfigArgs),
    /// Print the version.
    Version,
    /// Users and server settings.
    #[command(subcommand)]
    Admin(AdminCommand),
    /// Linked apps: services that act for their own users with an app key (cva_…).
    #[command(subcommand)]
    Apps(AppsCommand),
}

#[derive(Args, Debug, Default)]
pub struct ServeArgs {
    /// Do not apply pending migrations at startup.
    #[arg(long)]
    pub no_migrate: bool,
}

#[derive(Args, Debug)]
pub struct GenConfigArgs {
    /// Where to write the config (default: --config, CANVAS_CLOUD_CONFIG or
    /// /etc/canvas-cloud/canvas-cloud.toml). "-" prints it to stdout instead.
    #[arg(long, value_name = "PATH")]
    pub write: Option<PathBuf>,
    /// Overwrite an existing file (generates a NEW master key unless --master-key: every
    /// stored canvas becomes unreadable).
    #[arg(long)]
    pub force: bool,
    /// Base URL clients use, e.g. https://canvas.example.com (default: derived from listen).
    #[arg(long)]
    pub public_url: Option<String>,
    /// Database URL (env CANVAS_CLOUD_DATABASE_URL keeps passwords out of `ps`).
    #[arg(
        long,
        env = "CANVAS_CLOUD_DATABASE_URL",
        hide_env_values = true,
        default_value = "postgres://canvas_cloud@localhost:5432/canvas_cloud"
    )]
    pub database_url: String,
    #[arg(long, default_value = "0.0.0.0:443")]
    pub listen: String,
    /// self-signed | acme | off
    #[arg(long, default_value = "self-signed", value_parser = parse_tls_mode)]
    pub tls_mode: TlsMode,
    /// Domain for ACME (also the certificate name).
    #[arg(long)]
    pub domain: Option<String>,
    #[arg(long)]
    pub acme_email: Option<String>,
    /// Use this master key instead of generating one (env CANVAS_CLOUD_MASTER_KEY).
    #[arg(long, env = "CANVAS_CLOUD_MASTER_KEY", hide_env_values = true)]
    pub master_key: Option<String>,
    /// State directory for the certificate and the ACME cache.
    #[arg(long, value_name = "DIR", default_value = config::DEFAULT_STATE_DIR)]
    pub state_dir: PathBuf,
}

fn parse_tls_mode(s: &str) -> Result<TlsMode, String> {
    match s {
        "self-signed" | "selfsigned" | "self_signed" => Ok(TlsMode::SelfSigned),
        "acme" => Ok(TlsMode::Acme),
        "off" => Ok(TlsMode::Off),
        _ => Err("expected self-signed, acme or off".into()),
    }
}

#[derive(Subcommand, Debug)]
pub enum AdminCommand {
    /// User accounts.
    #[command(subcommand)]
    Users(UsersCommand),
    /// Runtime server settings (stored in the database).
    #[command(subcommand)]
    Settings(SettingsCommand),
}

#[derive(Subcommand, Debug)]
pub enum UsersCommand {
    /// List users.
    List,
    /// Create a password user (works even when signup is closed).
    Create {
        #[arg(long)]
        email: String,
        #[command(flatten)]
        password: PasswordFile,
        #[arg(long)]
        display_name: Option<String>,
    },
    /// Block sign-in for a user, revoke their sessions and tokens. Live canvas connections
    /// end when they next authenticate.
    Disable {
        #[arg(long)]
        email: String,
    },
    /// Re-enable a disabled user.
    Enable {
        #[arg(long)]
        email: String,
    },
    /// Permanently delete a user, the canvases they own and their memberships.
    Delete {
        #[arg(long)]
        email: String,
        /// Do not ask for confirmation.
        #[arg(long)]
        yes: bool,
    },
    /// Set a new password and revoke every session of the user.
    SetPassword {
        #[arg(long)]
        email: String,
        #[command(flatten)]
        password: PasswordFile,
    },
}

#[derive(Subcommand, Debug)]
pub enum SettingsCommand {
    /// Print every runtime setting.
    Get,
    /// Set a runtime setting: allow_signup true|false, or signup_domains a.com,b.com ("" for
    /// none).
    Set {
        #[arg(value_parser = ["allow_signup", "signup_domains"])]
        key: String,
        value: String,
    },
}

#[derive(Subcommand, Debug)]
pub enum AppsCommand {
    /// Register a linked app and print its key (cva_…) once.
    Create {
        #[arg(long)]
        name: String,
        /// May create canvases with POST /v1/apps/self/import.
        #[arg(long)]
        allow_import: bool,
        /// Do not link existing users by email when provisioning (409 email_taken instead).
        #[arg(long)]
        no_trust_email: bool,
    },
    /// List linked apps (never their keys).
    List,
    /// Revoke an app's key (its users and their canvases stay).
    Revoke { id: Uuid },
    /// Replace an app's key and print the new one once (the old key stops working).
    Rotate { id: Uuid },
    /// Change an app's policy: `--allow-import true|false`, `--trust-email true|false`.
    Set {
        id: Uuid,
        /// May create canvases with POST /v1/apps/self/import.
        #[arg(long, value_name = "true|false")]
        allow_import: Option<bool>,
        /// Link existing users by email when provisioning (false: 409 email_taken).
        #[arg(long, value_name = "true|false")]
        trust_email: Option<bool>,
    },
}

#[derive(Args, Debug)]
pub struct PasswordFile {
    /// File whose first line is the password ("-" reads stdin). Never on argv.
    #[arg(long, value_name = "FILE")]
    pub password_file: PathBuf,
}

impl PasswordFile {
    pub fn read(&self) -> anyhow::Result<String> {
        read_secret_line(&self.password_file)
    }
}

/// First line of `path` ("-" = stdin), without the line ending; must not be empty.
pub fn read_secret_line(path: &Path) -> anyhow::Result<String> {
    let mut text = String::new();
    if path.as_os_str() == "-" {
        std::io::stdin()
            .read_to_string(&mut text)
            .context("reading stdin")?;
    } else {
        text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    }
    let line = text.lines().next().unwrap_or_default().to_owned();
    if line.is_empty() {
        bail!("{}: empty password", path.display());
    }
    Ok(line)
}

/// Entry point used by `main`.
pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn runtime() -> anyhow::Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("canvas-cloud")
        .build()
        .context("starting tokio runtime")
}

fn load_config(cli_path: Option<PathBuf>) -> anyhow::Result<Config> {
    Config::load(&ConfigSource::resolve(cli_path))
}

pub fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    canvas_cloud_core::tls::install_default_provider();
    let command = cli.command.unwrap_or(Command::Serve(ServeArgs::default()));
    match command {
        Command::Version => {
            println!("canvas-cloud {}", crate::version());
        }
        Command::GenConfig(args) => gen_config(&args, cli.config)?,
        Command::Serve(args) => {
            let cfg = load_config(cli.config)?;
            runtime()?.block_on(crate::serve::run(cfg, !args.no_migrate))?;
        }
        Command::Migrate => {
            let cfg = load_config(cli.config)?;
            runtime()?.block_on(async {
                let pool = canvas_cloud_core::db::connect(&cfg).await?;
                canvas_cloud_core::db::migrate(&pool).await?;
                let (applied, pending) = canvas_cloud_core::db::pending_migrations(&pool).await?;
                println!("migrations applied: {applied}, pending: {}", pending.len());
                pool.close().await;
                anyhow::Ok(())
            })?;
        }
        Command::TlsInit { force, names } => {
            let cfg = load_config(cli.config)?;
            tls_init(&cfg, force, &names)?;
        }
        Command::Doctor => {
            let ok = runtime()?.block_on(crate::doctor::run(&ConfigSource::resolve(cli.config)));
            if !ok {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Healthcheck { wait } => {
            let cfg = load_config(cli.config)?;
            if !runtime()?.block_on(crate::doctor::wait_healthy(&cfg, wait)) {
                eprintln!("not healthy: /healthz did not answer on {}", cfg.listen);
                return Ok(ExitCode::FAILURE);
            }
            println!("healthy");
        }
        Command::Admin(cmd) => {
            let cfg = load_config(cli.config)?;
            runtime()?.block_on(crate::admin::admin(&cfg, cmd))?;
        }
        Command::Apps(cmd) => {
            let cfg = load_config(cli.config)?;
            runtime()?.block_on(crate::admin::apps(&cfg, cmd))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn tls_init(cfg: &Config, force: bool, extra: &[String]) -> anyhow::Result<()> {
    use canvas_cloud_core::tls;
    if cfg.tls.mode != TlsMode::SelfSigned {
        println!("tls.mode = {}: nothing to generate", cfg.tls.mode);
        return Ok(());
    }
    let exists = cfg.tls.cert_path.exists() && cfg.tls.key_path.exists();
    if exists && !force {
        println!(
            "certificate exists: {} (fp={})",
            cfg.tls.cert_path.display(),
            tls::leaf_fingerprint(&cfg.tls.cert_path)?
        );
        return Ok(());
    }
    let mut names = tls::self_signed_names(cfg);
    for n in extra {
        let n = n.trim().to_ascii_lowercase();
        if !n.is_empty() && !names.contains(&n) {
            names.push(n);
        }
    }
    tls::generate_self_signed(&names, &cfg.tls.cert_path, &cfg.tls.key_path)?;
    println!(
        "generated self-signed certificate {} for {} (fp={})",
        cfg.tls.cert_path.display(),
        names.join(", "),
        tls::leaf_fingerprint(&cfg.tls.cert_path)?
    );
    Ok(())
}

fn gen_config(args: &GenConfigArgs, cli_config: Option<PathBuf>) -> anyhow::Result<()> {
    let text = config::generate_toml(&GenerateOptions {
        public_url: args.public_url.clone().filter(|s| !s.trim().is_empty()),
        database_url: args.database_url.clone(),
        listen: args.listen.clone(),
        tls_mode: args.tls_mode,
        domain: args.domain.clone(),
        acme_email: args.acme_email.clone(),
        master_key: args.master_key.clone().filter(|s| !s.trim().is_empty()),
        cert_path: args.state_dir.join("tls/cert.pem"),
        key_path: args.state_dir.join("tls/key.pem"),
        acme_cache_dir: args.state_dir.join("acme"),
    })?;
    let path = args
        .write
        .clone()
        .unwrap_or_else(|| ConfigSource::resolve(cli_config).path);
    if path.as_os_str() == "-" {
        print!("{text}");
        return Ok(());
    }
    if path.exists() && !args.force {
        bail!(
            "{} already exists (use --force to overwrite; that generates a new master key)",
            path.display()
        );
    }
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    canvas_cloud_core::tls::write_atomic(&path, text.as_bytes(), 0o600)?;
    println!("wrote {}", path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory as _;

    #[test]
    fn cli_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn commands_parse() {
        let parse = |args: &[&str]| {
            let mut v = vec!["canvas-cloud"];
            v.extend_from_slice(args);
            Cli::try_parse_from(v)
        };
        assert!(parse(&["--config", "/tmp/x.toml"])
            .unwrap()
            .command
            .is_none());
        assert!(matches!(
            parse(&[
                "admin",
                "users",
                "create",
                "--email",
                "a@b.co",
                "--password-file",
                "-"
            ])
            .unwrap()
            .command,
            Some(Command::Admin(AdminCommand::Users(
                UsersCommand::Create { .. }
            )))
        ));
        // Passwords never on argv.
        assert!(parse(&[
            "admin",
            "users",
            "create",
            "--email",
            "a@b.co",
            "--password",
            "x"
        ])
        .is_err());
        assert!(parse(&["admin", "settings", "set", "allow_signup", "true"]).is_ok());
        assert!(parse(&["admin", "settings", "set", "signup_domains", "a.com,b.com"]).is_ok());
        assert!(parse(&["admin", "settings", "set", "nope", "1"]).is_err());
        let cli = parse(&[
            "apps",
            "create",
            "--name",
            "Linker",
            "--allow-import",
            "--no-trust-email",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Apps(AppsCommand::Create {
                allow_import: true,
                no_trust_email: true,
                ..
            }))
        ));
        assert!(parse(&["apps", "revoke", "not-a-uuid"]).is_err());
        assert!(parse(&["apps", "rotate", "00000000-0000-0000-0000-000000000001"]).is_ok());
        let cli = parse(&[
            "apps",
            "set",
            "00000000-0000-0000-0000-000000000001",
            "--allow-import",
            "false",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Apps(AppsCommand::Set {
                allow_import: Some(false),
                trust_email: None,
                ..
            }))
        ));
        assert!(parse(&[
            "apps",
            "set",
            "00000000-0000-0000-0000-000000000001",
            "--allow-import"
        ])
        .is_err());
        assert!(parse(&["healthcheck", "--wait", "30"]).is_ok());
        assert!(parse(&["gen-config", "--write", "-", "--tls-mode", "off"]).is_ok());
        assert!(parse(&["gen-config", "--tls-mode", "plain"]).is_err());
    }

    #[test]
    fn gen_config_writes_a_private_loadable_file() {
        let dir = std::env::temp_dir().join(format!("cvc-gen-{}", Uuid::now_v7()));
        let path = dir.join("canvas-cloud.toml");
        let args = |force: bool| GenConfigArgs {
            write: Some(path.clone()),
            force,
            public_url: Some("canvas.example.com".into()),
            database_url: "postgres://canvas_cloud@localhost/canvas_cloud".into(),
            listen: "127.0.0.1:18480".into(),
            tls_mode: TlsMode::Off,
            domain: None,
            acme_email: None,
            master_key: None,
            state_dir: dir.clone(),
        };
        gen_config(&args(false), None).unwrap();
        let cfg = Config::load(&ConfigSource {
            path: path.clone(),
            explicit: true,
        })
        .unwrap();
        assert_eq!(cfg.public_url, "http://canvas.example.com");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(gen_config(&args(false), None).is_err(), "never clobbers");
        gen_config(&args(true), None).unwrap();
        let again = Config::load(&ConfigSource {
            path: path.clone(),
            explicit: true,
        })
        .unwrap();
        assert_ne!(again.master_key, cfg.master_key, "--force makes a new key");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn password_files() {
        let path = std::env::temp_dir().join(format!("cvc-pw-{}", Uuid::now_v7()));
        std::fs::write(&path, "s3cret pass\nignored\n").unwrap();
        assert_eq!(read_secret_line(&path).unwrap(), "s3cret pass");
        std::fs::write(&path, "\n").unwrap();
        assert!(read_secret_line(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(read_secret_line(&path).is_err());
    }
}
