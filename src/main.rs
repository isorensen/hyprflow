use clap::{Parser, Subcommand};
use hyprflow::capture::capture_session;
use hyprflow::config::{config_path, load_config, sessions_dir};
use hyprflow::hyprctl::RealHyprctl;
use hyprflow::process::RealProcessInfo;
use hyprflow::restore::restore_session;
use hyprflow::session::{
    delete_session, list_sessions, load_session, save_session, session_exists,
};

#[derive(Parser)]
#[command(
    name = "hyprflow",
    version,
    about = "Save and restore Hyprland sessions"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Show detailed output
    #[arg(short, long, global = true)]
    verbose: bool,
}

#[derive(Subcommand)]
enum Commands {
    /// Save current session
    Save {
        /// Session name (default: "latest")
        name: Option<String>,
        /// Overwrite without prompt
        #[arg(short, long)]
        force: bool,
    },
    /// Restore a saved session
    Restore {
        /// Session name (default: "latest"). Pass "autosave" to always
        /// restore the newest autosave-* session, regardless of
        /// whether "latest" also exists.
        name: Option<String>,
        /// Preview without executing
        #[arg(short, long)]
        dry_run: bool,
        /// Skip restore if session is older than duration (e.g., 24h, 7d, 30m)
        #[arg(long)]
        max_age: Option<String>,
        /// Print exec-once line for Hyprland config
        #[arg(long)]
        on_login: bool,
    },
    /// List saved sessions
    List,
    /// Delete a saved session
    Delete {
        /// Session name to delete
        name: String,
    },
    /// Show config info
    Config,
    /// Manage autosave (status, run, install/uninstall timer)
    Autosave {
        /// Run autosave now (capture + rotate)
        #[arg(long)]
        now: bool,
        /// Install systemd timer
        #[arg(long)]
        install: bool,
        /// Uninstall systemd timer
        #[arg(long)]
        uninstall: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    let config = load_config();
    let sessions_dir = sessions_dir();

    match cli.command {
        Commands::Save { name, force } => {
            let name = name.unwrap_or_else(|| config.general.default_session.clone());

            if !force && session_exists(&name, &sessions_dir) && name != "latest" {
                eprintln!(
                    "Session '{}' already exists. Use --force to overwrite.",
                    name
                );
                std::process::exit(1);
            }

            let hyprctl = RealHyprctl;
            let process_info = RealProcessInfo;

            match capture_session(&name, &hyprctl, &process_info, &config) {
                Ok(session) => {
                    let client_count = session.clients.len();
                    match save_session(&session, &sessions_dir) {
                        Ok(()) => {
                            println!("Saved session '{}' ({} windows)", name, client_count);
                            if cli.verbose {
                                for c in &session.clients {
                                    println!("  ws={} {} — {}", c.workspace, c.class, c.title);
                                }
                            }
                        }
                        Err(e) => {
                            eprintln!("Error saving session: {}", e);
                            std::process::exit(1);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("Error capturing session: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Commands::Restore {
            name,
            dry_run,
            max_age,
            on_login,
        } => {
            if on_login {
                println!("Add this line to ~/.config/hypr/hyprland.conf:");
                println!();
                println!("  exec-once = hyprflow restore --max-age 24h");
                println!();
                println!("This will restore your last saved session on login.");
                println!("Sessions older than 24h will be skipped.");
                return;
            }

            let name = name.unwrap_or_else(|| config.general.default_session.clone());

            // "autosave" is a reserved name, not a session on disk: it
            // always means "the newest autosave-* session", regardless
            // of whether a manually-saved "latest" also exists. Without
            // this, `restore` (or `restore latest`) only ever falls
            // back to the newest autosave when "latest" is missing
            // entirely -- once you've saved "latest" manually even
            // once, it silently shadows every later autosave forever.
            let session = if name == "autosave" {
                match hyprflow::session::list_autosave_sessions(&sessions_dir) {
                    Ok(autosaves) if !autosaves.is_empty() => {
                        let newest = &autosaves[0].name;
                        match load_session(newest, &sessions_dir) {
                            Ok(s) => s,
                            Err(e) => {
                                eprintln!("Error loading session '{}': {}", newest, e);
                                std::process::exit(1);
                            }
                        }
                    }
                    _ => {
                        eprintln!("No autosave sessions available.");
                        std::process::exit(1);
                    }
                }
            } else {
                match load_session(&name, &sessions_dir) {
                Ok(s) => s,
                Err(hyprflow::session::SessionError::NotFound(_)) => {
                    // If requesting the default session and it doesn't exist,
                    // fall back to the most recent autosave
                    if name == config.general.default_session {
                        match hyprflow::session::list_autosave_sessions(&sessions_dir) {
                            Ok(autosaves) if !autosaves.is_empty() => {
                                let fallback_name = &autosaves[0].name;
                                println!(
                                    "Session '{}' not found. Falling back to '{}'.",
                                    name, fallback_name
                                );
                                match load_session(fallback_name, &sessions_dir) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        eprintln!("Error loading fallback session: {}", e);
                                        std::process::exit(1);
                                    }
                                }
                            }
                            _ => {
                                eprintln!(
                                    "Session '{}' not found and no autosave sessions available.",
                                    name
                                );
                                std::process::exit(1);
                            }
                        }
                    } else {
                        eprintln!("Error: session '{}' not found", name);
                        std::process::exit(1);
                    }
                }
                Err(e) => {
                    eprintln!("Error: {}", e);
                    std::process::exit(1);
                }
            }
            };

            if let Some(ref age_str) = max_age {
                match hyprflow::session::parse_max_age(age_str) {
                    Ok(max_duration) => {
                        let age = chrono::Utc::now() - session.created_at;
                        if age > max_duration {
                            println!(
                                "Session '{}' is too old (created {}).",
                                session.name,
                                session.created_at.format("%Y-%m-%d %H:%M")
                            );
                            println!("Skipping restore (max age: {}).", age_str);
                            return;
                        }
                    }
                    Err(e) => {
                        eprintln!("Error: {}", e);
                        std::process::exit(1);
                    }
                }
            }

            let hyprctl = RealHyprctl;
            match restore_session(&session, &hyprctl, &config, dry_run, cli.verbose) {
                Ok(report) => {
                    if dry_run {
                        println!("Dry run for session '{}':", session.name);
                    } else {
                        println!("Restored session '{}':", session.name);
                    }
                    println!(
                        "  {} restored, {} skipped, {} failed",
                        report.restored, report.skipped, report.failed
                    );
                    for detail in &report.details {
                        println!("  {}", detail);
                    }
                }
                Err(e) => {
                    eprintln!("Error restoring session: {}", e);
                    std::process::exit(1);
                }
            }
        }

        Commands::List => match list_sessions(&sessions_dir) {
            Ok(sessions) => {
                if sessions.is_empty() {
                    println!("No saved sessions.");
                } else {
                    println!("Saved sessions:");
                    for s in sessions {
                        let tag = if s.name.starts_with("autosave-") {
                            " [auto]"
                        } else {
                            ""
                        };
                        println!(
                            "  {} — {} windows ({}){}",
                            s.name,
                            s.client_count,
                            s.created_at.format("%Y-%m-%d %H:%M"),
                            tag
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!("Error listing sessions: {}", e);
                std::process::exit(1);
            }
        },

        Commands::Delete { name } => match delete_session(&name, &sessions_dir) {
            Ok(()) => println!("Deleted session '{}'", name),
            Err(e) => {
                eprintln!("Error: {}", e);
                std::process::exit(1);
            }
        },

        Commands::Config => {
            println!("Config path: {}", config_path().display());
            println!("Sessions dir: {}", sessions_dir.display());
            println!("Default session: {}", config.general.default_session);
            println!("Restore delay: {}ms", config.general.restore_delay_ms);
            println!(
                "Window detect timeout: {}ms",
                config.general.window_detect_timeout_ms
            );
            println!("Ignored classes: {:?}", config.filters.ignore_classes);
            if !config.apps.is_empty() {
                println!("App configs:");
                for (name, app) in &config.apps {
                    println!(
                        "  {}: binary={:?} capture_cwd={:?}",
                        name, app.binary, app.capture_cwd
                    );
                }
            }

            // Show Brave profile mappings
            println!();
            match hyprflow::brave::read_profiles() {
                Ok(profiles) if !profiles.is_empty() => {
                    println!("Brave profiles detected:");
                    let profile_ws = config
                        .apps
                        .get("brave-browser")
                        .and_then(|c| c.profile_workspaces.as_ref());
                    for profile in &profiles {
                        if let Some(ws) = profile_ws.and_then(|m| m.get(&profile.directory)) {
                            println!("  ✓ {} ({}) → ws={}", profile.directory, profile.name, ws);
                        } else {
                            println!(
                                "  · {} ({}) — not mapped, will be skipped",
                                profile.directory, profile.name
                            );
                        }
                    }
                    if profile_ws.is_none() {
                        println!(
                            "  (no profile_workspaces configured — all profiles will be captured)"
                        );
                    }
                }
                Ok(_) => println!("No Brave profiles detected."),
                Err(e) => println!("Could not read Brave profiles: {e}"),
            }
        }

        Commands::Autosave {
            now,
            install,
            uninstall,
        } => {
            let flag_count = [now, install, uninstall].iter().filter(|&&f| f).count();
            if flag_count > 1 {
                eprintln!(
                    "Error: only one of --now, --install, --uninstall may be specified at a time."
                );
                std::process::exit(1);
            }

            let systemd_dir = hyprflow::autosave::systemd_user_dir();

            if install {
                match hyprflow::autosave::install(&systemd_dir) {
                    Ok((service_path, timer_path)) => {
                        println!("Created:");
                        println!("  {}", service_path.display());
                        println!("  {}", timer_path.display());
                        println!();
                        println!("To enable and start:");
                        println!("  systemctl --user enable --now hyprflow-autosave.timer");
                        println!();
                        println!("To check status:");
                        println!("  systemctl --user status hyprflow-autosave.timer");
                        println!("  journalctl --user -u hyprflow-autosave.service");
                    }
                    Err(e) => {
                        eprintln!("Error installing autosave timer: {}", e);
                        std::process::exit(1);
                    }
                }
            } else if uninstall {
                match hyprflow::autosave::uninstall(&systemd_dir) {
                    Ok(()) => println!("Autosave timer removed."),
                    Err(e) => {
                        eprintln!("Error uninstalling autosave timer: {}", e);
                        std::process::exit(1);
                    }
                }
            } else if now {
                let hyprctl = RealHyprctl;
                let process_info = RealProcessInfo;
                let name = hyprflow::session::autosave_name_now();

                match capture_session(&name, &hyprctl, &process_info, &config) {
                    Ok(session) => {
                        let client_count = session.clients.len();
                        if let Err(e) = save_session(&session, &sessions_dir) {
                            eprintln!("Error saving autosave session: {}", e);
                            std::process::exit(1);
                        }

                        let retain = config.general.autosave_retain;
                        let total_before = hyprflow::session::list_autosave_sessions(&sessions_dir)
                            .map(|s| s.len())
                            .unwrap_or(0);
                        let pruned =
                            hyprflow::session::rotate_autosaves(&sessions_dir, retain).unwrap_or(0);
                        let retained = total_before.saturating_sub(pruned);

                        println!(
                            "Autosaved '{}' ({} windows). Retained {}, pruned {}.",
                            name, client_count, retained, pruned
                        );
                    }
                    Err(e) => {
                        eprintln!("Error capturing session: {}", e);
                        std::process::exit(1);
                    }
                }
            } else {
                // Status mode (no flags)
                let installed = hyprflow::autosave::is_installed(&systemd_dir);
                let active = hyprflow::autosave::is_active();

                if !installed {
                    println!("Autosave is not configured.");
                    println!("Run 'hyprflow autosave --install' to set up the systemd timer.");
                } else if !active {
                    println!("Autosave timer is installed but not active.");
                    println!("  systemctl --user enable --now hyprflow-autosave.timer");
                } else {
                    println!("Autosave is active (every 10min).");
                    match hyprflow::session::list_autosave_sessions(&sessions_dir) {
                        Ok(sessions) if !sessions.is_empty() => {
                            let latest = &sessions[0];
                            println!(
                                "Last: {} — {} windows",
                                latest.created_at.format("%Y-%m-%d %H:%M:%S"),
                                latest.client_count
                            );
                            println!(
                                "Retained: {} sessions (oldest: {})",
                                sessions.len(),
                                sessions.last().unwrap().name
                            );
                        }
                        Ok(_) => println!("No autosave sessions yet."),
                        Err(e) => println!("Could not list sessions: {e}"),
                    }
                    println!("To disable: hyprflow autosave --uninstall");
                }
            }
        }
    }
}
