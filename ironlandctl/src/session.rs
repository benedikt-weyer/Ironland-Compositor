//! `ironlandctl session`: helpers for the `[session]` table, i.e. picking
//! which `systemd --user` service the compositor should start as the desktop
//! shell. Shared by the settings GUI and the shell's Nexus page so neither
//! has to know how to enumerate units.

use std::process::Command;

use anyhow::{Context, Result, bail};
use clap::Subcommand;

#[derive(Subcommand)]
pub enum SessionCommand {
    /// List the `systemd --user` services a shell could be started from:
    /// installed unit files plus anything currently loaded (transient units),
    /// sorted, without template units like `foo@.service`.
    Units {
        /// Print a JSON array of unit names instead of one name per line.
        #[arg(long)]
        json: bool,
    },
}

pub fn run(command: &SessionCommand) -> Result<()> {
    match command {
        SessionCommand::Units { json } => {
            let units = list_units()?;
            if *json {
                println!("{}", serde_json::to_string(&units)?);
            } else {
                for unit in units {
                    println!("{unit}");
                }
            }
            Ok(())
        }
    }
}

fn list_units() -> Result<Vec<String>> {
    let mut names = Vec::new();
    // `list-unit-files` knows every installed service (including ones not
    // loaded right now, like a shell that crashed or hasn't started yet);
    // `list-units` additionally covers transient units that have no file.
    for subcommand in ["list-unit-files", "list-units"] {
        let output = Command::new("systemctl")
            .args(["--user", subcommand, "--type=service", "--all"])
            .args(["--no-legend", "--no-pager", "--plain"])
            .output()
            .with_context(|| format!("failed to run `systemctl --user {subcommand}`"))?;
        if !output.status.success() {
            bail!(
                "`systemctl --user {subcommand}` failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        names.extend(parse_unit_names(&String::from_utf8_lossy(&output.stdout)));
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Extracts service names from `systemctl ... --no-legend --plain` output:
/// the first column of each line, minus template units (which can't be
/// started without an instance name).
fn parse_unit_names(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| name.ends_with(".service") && !name.ends_with("@.service"))
        .map(str::to_string)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_first_column_and_skips_templates() {
        let output = "\
desktop-shell.service            enabled enabled
dbus-broker.service              static  -
getty@.service                   enabled enabled
pipewire.socket                  enabled enabled
iron-polkit.service loaded active running Polkit authentication agent
";
        assert_eq!(
            parse_unit_names(output),
            vec![
                "desktop-shell.service",
                "dbus-broker.service",
                "iron-polkit.service"
            ]
        );
    }

    #[test]
    fn blank_output_has_no_units() {
        assert!(parse_unit_names("").is_empty());
        assert!(parse_unit_names("\n  \n").is_empty());
    }
}
