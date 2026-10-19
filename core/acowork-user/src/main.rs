//! acowork-user — standalone account / user-domain service (ADR-084).
//!
//! Lifecycle is managed by the Gateway supervisor (spawn / health-poll /
//! restart). Binds loopback only: it trusts the `X-Auth-*` headers the
//! Gateway's `user_proxy` injects (ADR-084 §决策 7).
//!
//! Public contract: the Desktop still calls `{gateway}/api/auth/*`,
//! `{gateway}/api/users/*` and `{gateway}/api/user/avatar-*` — byte-identical
//! to before the extraction, so the Desktop needed no changes.

use std::io::{BufRead, Read};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;

use acowork_user::cli::{Cli, Command};
use acowork_user::config::UserServiceConfig;
use acowork_user::server::UserService;
use acowork_user::AuthService;

fn main() {
    let cli = Cli::parse();

    acowork_core::logging::init_subprocess_logging(&cli.log_level);
    acowork_core::logging::install_panic_hook();

    // Config: defaults <- TOML <- CLI.
    let mut config = match &cli.config {
        Some(path) => match UserServiceConfig::load_file(path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!(error = %e, "failed to load acowork-user config");
                std::process::exit(1);
            }
        },
        None => UserServiceConfig::default(),
    };
    if let Some(dir) = cli.data_dir.clone() {
        config.data_dir = dir;
    }
    if let Some(port) = cli.port {
        config.port = port;
    }
    match acowork_user::cli::resolve_auth_mode(&cli, config.auth_mode) {
        Ok(mode) => config.auth_mode = mode,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    }
    if let Err(e) = config.validate() {
        tracing::error!(error = %e, "invalid acowork-user config");
        std::process::exit(1);
    }

    if !config.enabled {
        tracing::warn!("acowork-user disabled via config (enabled=false) — exiting");
        std::process::exit(0);
    }

    // `admin-setup` is a one-shot: seed the first admin, then exit. It runs
    // before any server work so it never races the serving path.
    if let Some(Command::AdminSetup {
        password_file,
        password_stdin,
        check,
    }) = &cli.command
    {
        let code = run_admin_setup(
            &config,
            password_file.as_deref(),
            *password_stdin,
            *check,
        );
        std::process::exit(code);
    }

    // `first-admin` is a read-only one-shot (ADR-087 default-owner lookup).
    if let Some(Command::FirstAdmin) = &cli.command {
        std::process::exit(run_first_admin(&config));
    }

    let auth = if config.is_multi_user() {
        match AuthService::new(
            &config.data_dir,
            config.password_policy.clone(),
            config.bootstrap_admin.clone(),
        ) {
            Ok(svc) => {
                if let Err(e) = svc.ensure_bootstrap_admin() {
                    tracing::error!(error = %e, "failed to ensure bootstrap admin");
                    std::process::exit(1);
                }
                Some(Arc::new(svc))
            }
            Err(e) => {
                tracing::error!(error = %e, "failed to initialize the account service");
                std::process::exit(1);
            }
        }
    } else {
        tracing::info!("AUTH_MODE=local — serving profiles and avatars only");
        None
    };

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|e| {
            tracing::error!(error = %e, "failed to build the tokio runtime");
            std::process::exit(1);
        });

    runtime.block_on(async move {
        // Wave 6.2: fold the legacy `users/{lo}/chats/{hi}/` layout into the
        // chat_sessions store once, before anything can serve. Idempotent
        // (per-row deterministic message ids + staging directories), and
        // fail-soft: a pair that cannot convert keeps its staging directory
        // for the next boot, and the adapter 404s that chat rather than
        // serve a half-migrated one.
        if let Some(svc) = auth.as_ref() {
            match acowork_user::chat_sessions::migrate_legacy(svc.data_dir()) {
                Ok(r) if r.pairs > 0 => {
                    tracing::info!(pairs = r.pairs, messages = r.messages, "legacy chat layout migrated (Wave 6.2)");
                }
                Ok(_) => {}
                Err(e) => tracing::error!(error = %e, "legacy chat migration aborted"),
            }
        }

        // Profile-change signal (ADR-084 §决策 4b). Best-effort: a broker that
        // is not up yet is fine (the client reconnects), and a total failure
        // only delays the Gateway's view until its next pull.
        acowork_user::mqtt_publisher::init(&cli.mqtt_host, cli.mqtt_port, cli.mqtt_password.clone())
            .await;

        if let Some(ref url) = cli.gateway_health_url {
            spawn_gateway_health_watchdog(
                url.clone(),
                Duration::from_millis(cli.gateway_health_interval_ms),
                Duration::from_millis(cli.gateway_health_timeout_ms),
            );
            tracing::info!(url = %url, "Gateway health watchdog started (ADR-018)");
        }

        let service = Arc::new(UserService::new(config.clone(), auth));

        let bind = format!("{}:{}", cli.host, config.port);
        let bind_addr: std::net::SocketAddr = match bind.parse() {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(bind = %bind, error = %e, "invalid bind address");
                std::process::exit(1);
            }
        };

        let addr = match service.clone().serve(bind_addr).await {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(error = %e, "failed to start acowork-user");
                std::process::exit(1);
            }
        };
        tracing::info!(
            addr = %addr,
            data_dir = %config.data_dir.display(),
            auth_mode = %config.auth_mode,
            "acowork-user service listening (standalone)"
        );

        // The Gateway supervisor reads the *actual* port from here (the
        // configured port may have been occupied).
        if let Some(path) = &cli.port_file {
            if let Err(e) = std::fs::write(path, addr.port().to_string()) {
                tracing::error!(path = %path.display(), error = %e, "failed to write port file");
                std::process::exit(1);
            }
            tracing::info!(path = %path.display(), port = addr.port(), "port file written");
        }

        let shutdown = acowork_core::shutdown::Shutdown::new();
        acowork_core::shutdown::install_signal_handlers(shutdown.clone());
        while !shutdown.is_shutting_down() {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        tracing::info!("acowork-user shut down");
    });
}

/// Seed the first administrator, then exit.
///
/// Returns the process exit code.
fn run_admin_setup(
    config: &UserServiceConfig,
    password_file: Option<&std::path::Path>,
    password_stdin: bool,
    check_only: bool,
) -> i32 {
    if !config.is_multi_user() {
        eprintln!(
            "admin-setup requires --auth-mode multi_user (resolved: {}).",
            config.auth_mode
        );
        return 1;
    }
    if check_only {
        // Read-only probe: report the same predicate `/health` publishes as
        // `requires_setup`, so the Gateway's first-boot prompt cannot
        // disagree with the restricted-mode gate it later applies.
        //
        // Deliberately not `AuthService::new(...)`: that *creates* `auth/` and
        // generates the signing key, and seeding here (`ensure_bootstrap_admin`
        // is create-once) would plant a passwordless `admin` before the
        // service ever starts — so an operator's configured `bootstrap_admin`
        // would silently never be applied (ADR-076 §决策 12).
        return if acowork_user::auth::service::store_requires_setup(&config.data_dir) {
            println!("setup required");
            0
        } else {
            println!("already configured");
            1
        };
    }
    let svc = match AuthService::new(&config.data_dir, config.password_policy.clone(), None) {
        Ok(svc) => svc,
        Err(e) => {
            eprintln!("failed to initialize the account service: {e}");
            return 1;
        }
    };
    // Never seed through the bootstrap path — the bootstrap credential is
    // first-boot only and must not become a standing second admin.
    if let Err(e) = svc.ensure_bootstrap_admin() {
        eprintln!("failed to prepare the admin account: {e}");
        return 1;
    }
    let mut password = match read_admin_password(password_file, password_stdin) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let result = svc.set_admin_password(&password);
    // The Argon2id hash is the only durable artifact; drop the plaintext.
    zeroize::Zeroize::zeroize(&mut password);
    match result {
        Ok(()) => {
            println!("Admin password set.");
            0
        }
        Err(e) => {
            eprintln!("failed to set the admin password: {e}");
            1
        }
    }
}

/// Print the earliest-created active admin's `user_id` (ADR-087).
///
/// Deliberately read-only — like `admin-setup --check` it must not run
/// `AuthService::new` (which creates `auth/` + the signing key) or seed
/// the bootstrap admin. A passwordless bootstrap admin counts: the
/// account identity is established even before the password is set, and
/// the default owner is an identity, not a credential.
fn run_first_admin(config: &UserServiceConfig) -> i32 {
    if !config.is_multi_user() {
        eprintln!(
            "first-admin requires --auth-mode multi_user (resolved: {}).",
            config.auth_mode
        );
        return 1;
    }
    let list = match acowork_user::account::store::load_accounts(&config.data_dir) {
        Ok(list) => list,
        Err(e) => {
            eprintln!("failed to load the account store: {e}");
            return 1;
        }
    };
    let Some(admin) = list
        .accounts
        .iter()
        .filter(|a| a.is_admin() && a.disabled_at.is_none())
        .min_by_key(|a| a.created_at.as_str())
    else {
        return 1;
    };
    println!("{}", admin.user_id);
    0
}

/// Read the admin password from a file or stdin.
///
/// Note (ADR-084, decided in M4): the interactive TTY prompt stays in the
/// **Gateway's** `admin-setup`, which reads it and pipes the password to
/// `acowork-user admin-setup --password-stdin`. Only one process should ever
/// prompt, and for an operator the Gateway CLI is the entry point — so this
/// binary's contract is deliberately non-interactive, which is also the shape
/// the Gateway supervisor and scripts use.
fn read_admin_password(
    password_file: Option<&std::path::Path>,
    password_stdin: bool,
) -> Result<String, String> {
    if let Some(path) = password_file {
        let file = std::fs::File::open(path)
            .map_err(|e| format!("failed to open password file '{}': {e}", path.display()))?;
        // A password past 1 KiB is operator error, not a password.
        let mut buf = String::new();
        file.take(1024)
            .read_to_string(&mut buf)
            .map_err(|e| format!("failed to read password file '{}': {e}", path.display()))?;
        return Ok(buf.trim_end_matches(['\n', '\r']).to_string());
    }
    if password_stdin {
        let stdin = std::io::stdin();
        let mut line = String::new();
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| format!("failed to read password from stdin: {e}"))?;
        return Ok(line.trim_end_matches(['\n', '\r']).to_string());
    }
    Err("no password given. Pass --password-file <path> or --password-stdin.".to_string())
}

/// Self-exit when the Gateway stops answering (ADR-018).
///
/// A supervised subprocess that outlives its supervisor becomes an orphan
/// holding a port and a data directory, and the next boot has to fight it.
fn spawn_gateway_health_watchdog(health_url: String, interval: Duration, timeout: Duration) {
    tokio::spawn(async move {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("failed to build HTTP client for the Gateway health probe");

        // The Gateway must be unreachable for `timeout` *continuously* — a
        // restart or a long pause must not take this process with it.
        let mut last_success = std::time::Instant::now();
        loop {
            tokio::time::sleep(interval).await;

            let healthy = match client.get(&health_url).send().await {
                Ok(resp) => resp.status().is_success(),
                Err(_) => false,
            };

            if healthy {
                last_success = std::time::Instant::now();
            } else if last_success.elapsed() >= timeout {
                tracing::error!(
                    elapsed_secs = last_success.elapsed().as_secs(),
                    timeout_secs = timeout.as_secs(),
                    "Gateway unreachable — self-exiting (ADR-018)"
                );
                std::process::exit(0);
            }
        }
    });
}
