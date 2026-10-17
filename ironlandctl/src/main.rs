//! `ironlandctl`: read and edit ironland-compositor's `config.toml` from the
//! command line, covering every setting the `gui-settings` Fyne app exposes.
//! See `README.md` (in this directory) for the full command reference.

mod full;
mod outputs;
mod paths;
mod session;
mod settings;
mod shortcuts;

use std::{
    io::{Read, Write},
    path::PathBuf,
};

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use ironland_config::{Config, FullConfig};
use outputs::OutputsCommand;
use session::SessionCommand;
use settings::SettingKey;
use shortcuts::ShortcutsCommand;

#[derive(Parser)]
#[command(
    name = "ironlandctl",
    version,
    about = "Read and edit ironland-compositor's config.toml"
)]
struct Cli {
    /// Use this file instead of the normal search path, for both reads and
    /// writes (mainly for testing).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Print the effective config: the config file merged over built-in
    /// defaults, exactly as the compositor would see it at startup.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Print the built-in default config.
    Defaults {
        #[arg(long)]
        json: bool,
    },
    /// Print which config file is active, and which one edits write to.
    Path {
        #[arg(long)]
        json: bool,
    },
    /// Print one setting's current value.
    Get { key: SettingKey },
    /// Change one setting.
    Set {
        key: SettingKey,
        value: String,
        /// Don't also push a dark_mode change out to the desktop's color
        /// scheme (gsettings); only save it to config.toml.
        #[arg(long)]
        no_apply_live: bool,
    },
    /// Remove an override, reverting a setting to its default.
    Unset { key: SettingKey },
    /// Delete the config file, reverting every setting to its default.
    Reset {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Keybindings (the `[shortcuts]` table).
    Shortcuts {
        #[command(subcommand)]
        command: ShortcutsCommand,
    },
    /// Monitors (the `[outputs]` table).
    Outputs {
        #[command(subcommand)]
        command: OutputsCommand,
    },
    /// Session bring-up (the `[session]` table): which unit is the shell.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Read a full config as JSON from stdin and write it out, replacing
    /// the current file. Intended for the settings GUI's Save button, and
    /// for scripted bulk imports.
    Apply {
        #[arg(long)]
        no_apply_live: bool,
    },
    /// Print a shell completion script (e.g. `ironlandctl completions zsh
    /// > ~/.zfunc/_ironlandctl`).
    Completions { shell: Shell },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    if let Some(path) = &cli.config {
        paths::apply_read_override(path);
    }

    match &cli.command {
        Command::Show { json } => show(*json),
        Command::Defaults { json } => defaults(*json),
        Command::Path { json } => print_paths(&cli.config, *json),
        Command::Get { key } => {
            let full = FullConfig::load();
            println!("{}", settings::get(&full, *key));
            Ok(())
        }
        Command::Set {
            key,
            value,
            no_apply_live,
        } => set(&cli.config, *key, value, *no_apply_live),
        Command::Unset { key } => unset(&cli.config, *key),
        Command::Reset { yes } => reset(&cli.config, *yes),
        Command::Shortcuts { command } => shortcuts_command(&cli.config, command),
        Command::Outputs { command } => outputs_command(&cli.config, command),
        Command::Session { command } => session::run(command),
        Command::Apply { no_apply_live } => apply(&cli.config, *no_apply_live),
        Command::Completions { shell } => {
            clap_complete::generate(
                *shell,
                &mut Cli::command(),
                "ironlandctl",
                &mut std::io::stdout(),
            );
            Ok(())
        }
    }
}

fn show(json: bool) -> Result<()> {
    let full = FullConfig::load();
    if json {
        println!("{}", serde_json::to_string_pretty(&full)?);
        return Ok(());
    }
    print_full(&full);
    Ok(())
}

fn defaults(json: bool) -> Result<()> {
    let full = FullConfig::default();
    if json {
        println!("{}", serde_json::to_string_pretty(&full)?);
        return Ok(());
    }
    print_full(&full);
    Ok(())
}

fn print_full(full: &FullConfig) {
    for key in SettingKey::all() {
        println!("{:<32} {}", key.dotted(), settings::get(full, *key));
    }
}

fn print_paths(explicit: &Option<PathBuf>, json: bool) -> Result<()> {
    let (_, loaded_from) = Config::try_load().map_err(|err| anyhow::anyhow!("{err}"))?;
    let write_path = paths::write_path(explicit)?;
    let search_path = ironland_config::config_search_path();

    if json {
        println!(
            "{}",
            serde_json::json!({
                "active": loaded_from,
                "writes_to": write_path,
                "search_path": search_path,
            })
        );
        return Ok(());
    }

    match &loaded_from {
        Some(path) => println!("Active config:  {}", path.display()),
        None => println!("Active config:  (none found; using built-in defaults)"),
    }
    println!("Writes go to:   {}", write_path.display());
    println!("\nSearch path (in priority order):");
    for path in search_path {
        let marker = if path.is_file() { "*" } else { " " };
        println!("  {marker} {}", path.display());
    }
    Ok(())
}

fn set(explicit: &Option<PathBuf>, key: SettingKey, raw: &str, no_apply_live: bool) -> Result<()> {
    let path = paths::write_path(explicit)?;
    let mut doc = paths::load_doc(&path)?;
    settings::set(&mut doc, key, raw)?;
    paths::write_atomic(&path, &doc.to_string())?;
    println!(
        "Set {} = {} (saved to {})",
        key.dotted(),
        raw,
        path.display()
    );

    if key == SettingKey::AppearanceDarkMode && !no_apply_live {
        let dark = matches!(
            raw.to_ascii_lowercase().as_str(),
            "true" | "on" | "yes" | "1"
        );
        apply_live_color_scheme(dark);
    }
    Ok(())
}

fn unset(explicit: &Option<PathBuf>, key: SettingKey) -> Result<()> {
    let path = paths::write_path(explicit)?;
    let mut doc = paths::load_doc(&path)?;
    settings::unset(&mut doc, key);
    paths::write_atomic(&path, &doc.to_string())?;
    println!("Unset {} (saved to {})", key.dotted(), path.display());
    Ok(())
}

fn reset(explicit: &Option<PathBuf>, yes: bool) -> Result<()> {
    let path = paths::write_path(explicit)?;
    if !path.is_file() {
        println!("{} doesn't exist; nothing to reset.", path.display());
        return Ok(());
    }
    if !yes {
        print!(
            "This deletes {}, reverting every setting to its default. Continue? [y/N] ",
            path.display()
        );
        std::io::stdout().flush().ok();
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if !matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Aborted.");
            return Ok(());
        }
    }
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    println!(
        "Removed {}; every setting is back to its default.",
        path.display()
    );
    Ok(())
}

fn shortcuts_command(explicit: &Option<PathBuf>, command: &ShortcutsCommand) -> Result<()> {
    match command {
        ShortcutsCommand::List { json } => {
            shortcuts::list(&Config::load(), *json);
            Ok(())
        }
        ShortcutsCommand::Get { action } => {
            println!("{}", shortcuts::get(&Config::load(), action)?);
            Ok(())
        }
        ShortcutsCommand::Set { action, combos } => {
            let path = paths::write_path(explicit)?;
            let mut doc = paths::load_doc(&path)?;
            shortcuts::set(&mut doc, action, combos)?;
            paths::write_atomic(&path, &doc.to_string())?;
            println!("Bound {action} to {combos} (saved to {})", path.display());
            Ok(())
        }
        ShortcutsCommand::Unset { action } => {
            let path = paths::write_path(explicit)?;
            let mut doc = paths::load_doc(&path)?;
            shortcuts::unset(&mut doc, action)?;
            paths::write_atomic(&path, &doc.to_string())?;
            println!("Unbound {action} (saved to {})", path.display());
            Ok(())
        }
        ShortcutsCommand::Events(events) => match events {
            shortcuts::EventsCommand::List { json } => {
                shortcuts::events_list(&Config::load(), *json);
                Ok(())
            }
            shortcuts::EventsCommand::Add { name, combos } => {
                let path = paths::write_path(explicit)?;
                let mut doc = paths::load_doc(&path)?;
                shortcuts::events_add(&mut doc, name, combos.as_deref());
                paths::write_atomic(&path, &doc.to_string())?;
                println!("Added shell event {name} (saved to {})", path.display());
                Ok(())
            }
            shortcuts::EventsCommand::Remove { name } => {
                let path = paths::write_path(explicit)?;
                let mut doc = paths::load_doc(&path)?;
                shortcuts::events_remove(&mut doc, name);
                paths::write_atomic(&path, &doc.to_string())?;
                println!("Removed shell event {name} (saved to {})", path.display());
                Ok(())
            }
        },
    }
}

fn outputs_command(explicit: &Option<PathBuf>, command: &OutputsCommand) -> Result<()> {
    match command {
        OutputsCommand::List { json } => {
            outputs::list(&Config::load(), *json);
            Ok(())
        }
        OutputsCommand::Get { name } => {
            println!("{}", outputs::get(&Config::load(), name));
            Ok(())
        }
        OutputsCommand::Set { name, args } => {
            let path = paths::write_path(explicit)?;
            let mut doc = paths::load_doc(&path)?;
            outputs::set(&mut doc, name, args)?;
            paths::write_atomic(&path, &doc.to_string())?;
            println!("Updated monitor {name} (saved to {})", path.display());
            Ok(())
        }
        OutputsCommand::Remove { name } => {
            let path = paths::write_path(explicit)?;
            let mut doc = paths::load_doc(&path)?;
            outputs::remove(&mut doc, name);
            paths::write_atomic(&path, &doc.to_string())?;
            println!("Removed monitor {name} (saved to {})", path.display());
            Ok(())
        }
        OutputsCommand::Detect { json } => {
            let detected = outputs::detect()?;
            if *json {
                println!("{}", serde_json::to_string_pretty(&detected)?);
                return Ok(());
            }
            for output in detected {
                let label = if output.make.is_empty() && output.model.is_empty() {
                    format!("{} — {}×{}", output.name, output.width, output.height)
                } else {
                    format!(
                        "{} — {} {} — {}×{}",
                        output.name, output.make, output.model, output.width, output.height
                    )
                };
                println!("{label}");
                if !output.refresh_rates.is_empty() {
                    let rates: Vec<String> = output
                        .refresh_rates
                        .iter()
                        .map(|r| outputs::format_refresh(*r))
                        .collect();
                    println!("  refresh rates: {}", rates.join(", "));
                }
            }
            Ok(())
        }
    }
}

fn apply(explicit: &Option<PathBuf>, no_apply_live: bool) -> Result<()> {
    let mut input = String::new();
    std::io::stdin()
        .read_to_string(&mut input)
        .context("reading config JSON from stdin")?;
    let full: FullConfig = serde_json::from_str(&input).context("parsing config JSON")?;

    let path = paths::write_path(explicit)?;
    let mut doc = paths::load_doc(&path)?;
    full::write(&mut doc, &full);
    paths::write_atomic(&path, &doc.to_string())?;

    if !no_apply_live {
        apply_live_color_scheme(full.appearance.dark_mode);
    }

    // Deliberately the only thing on stdout: callers (the settings GUI)
    // that want the resulting path parse this line, not a longer message.
    println!("{}", path.display());
    Ok(())
}

/// Pushes dark/light mode out to the rest of the desktop, not just
/// config.toml: writes the same GNOME setting
/// (`org.gnome.desktop.interface color-scheme`) that xdg-desktop-portal-gtk
/// reads from when answering an app's `org.freedesktop.portal.Settings`
/// (`org.freedesktop.appearance`, `color-scheme`) query - the closest thing
/// to a standard XDG mechanism for this without running a full portal
/// backend. Best-effort: a system without gsettings/GNOME's schema just
/// leaves this a no-op beyond config.toml's own copy of the toggle.
fn apply_live_color_scheme(dark: bool) {
    let value = if dark { "prefer-dark" } else { "default" };
    if let Err(err) = std::process::Command::new("gsettings")
        .args(["set", "org.gnome.desktop.interface", "color-scheme", value])
        .status()
    {
        eprintln!("warning: couldn't apply color scheme live via gsettings: {err}");
    }
}
