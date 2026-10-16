//! The `outputs` subcommand: the `[outputs.*]` table, keyed by connector
//! name, plus `detect` (shells out to `wayland-info`, same as the settings
//! GUI, to list currently connected monitors).

use anyhow::{Result, bail};
use clap::{Args, Subcommand};
use ironland_config::{Config, OutputPosition, OutputSettings};
use toml_edit::{DocumentMut, Item, Table, value};

#[derive(Debug, Subcommand)]
pub enum OutputsCommand {
    /// List every configured monitor.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Print one monitor's settings.
    Get { name: String },
    /// Change a monitor's settings.
    Set {
        /// Connector name, e.g. "eDP-1" or "HDMI-A-1".
        name: String,
        #[command(flatten)]
        args: SetArgs,
    },
    /// Forget a monitor's settings, reverting it to auto-placed/extended.
    Remove { name: String },
    /// List currently connected monitors (via `wayland-info`).
    Detect {
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
#[command(next_help_heading = "Monitor settings")]
pub struct SetArgs {
    /// Mark this the primary monitor.
    #[arg(long, conflicts_with = "no_primary")]
    pub primary: bool,
    /// Clear the primary flag.
    #[arg(long)]
    pub no_primary: bool,

    /// Requested refresh rate in Hz, e.g. 144 or 59.94.
    #[arg(long, value_name = "HZ", conflicts_with_all = ["refresh_mhz", "auto_refresh"])]
    pub refresh_hz: Option<f64>,
    /// Requested refresh rate in millihertz (matches the config file's own
    /// unit), e.g. 144000.
    #[arg(long, value_name = "MHZ", conflicts_with_all = ["refresh_hz", "auto_refresh"])]
    pub refresh_mhz: Option<i32>,
    /// Let the compositor pick the refresh rate automatically.
    #[arg(long)]
    pub auto_refresh: bool,

    /// Duplicate ("mirror") another output instead of extending the desktop.
    #[arg(long, value_name = "CONNECTOR", conflicts_with_all = ["right_of", "left_of", "above", "below", "x", "auto_position"])]
    pub mirror_of: Option<String>,
    /// Place this output to the right of another one.
    #[arg(long, value_name = "CONNECTOR", conflicts_with_all = ["left_of", "above", "below", "mirror_of", "x", "auto_position"])]
    pub right_of: Option<String>,
    /// Place this output to the left of another one.
    #[arg(long, value_name = "CONNECTOR", conflicts_with_all = ["right_of", "above", "below", "mirror_of", "x", "auto_position"])]
    pub left_of: Option<String>,
    /// Place this output above another one.
    #[arg(long, value_name = "CONNECTOR", conflicts_with_all = ["right_of", "left_of", "below", "mirror_of", "x", "auto_position"])]
    pub above: Option<String>,
    /// Place this output below another one.
    #[arg(long, value_name = "CONNECTOR", conflicts_with_all = ["right_of", "left_of", "above", "mirror_of", "x", "auto_position"])]
    pub below: Option<String>,
    /// Place this output at an exact logical position (needs --y too).
    #[arg(long, requires = "y", conflicts_with_all = ["right_of", "left_of", "above", "below", "mirror_of", "auto_position"])]
    pub x: Option<i32>,
    /// Paired with --x.
    #[arg(long, requires = "x")]
    pub y: Option<i32>,
    /// Clear any position/mirror override, reverting to auto-placement.
    #[arg(long, conflicts_with_all = ["right_of", "left_of", "above", "below", "mirror_of", "x"])]
    pub auto_position: bool,
}

fn outputs_table(doc: &mut DocumentMut) -> &mut Table {
    if !doc.contains_key("outputs") {
        doc.insert("outputs", Item::Table(Table::new()));
    }
    doc["outputs"]
        .as_table_mut()
        .expect("config.toml has a non-table [outputs]")
}

pub fn list(config: &Config, json: bool) {
    let mut names: Vec<&String> = config.outputs.keys().collect();
    names.sort();

    if json {
        let map: serde_json::Map<_, _> = names
            .iter()
            .map(|n| ((*n).clone(), settings_json(&config.outputs[*n])))
            .collect();
        println!("{}", serde_json::Value::Object(map));
        return;
    }

    if names.is_empty() {
        println!("No monitors configured; all are auto-placed and extended.");
        return;
    }
    for name in names {
        println!("{name}: {}", describe(&config.outputs[name]));
    }
}

pub fn get(config: &Config, name: &str) -> String {
    match config.outputs.get(name) {
        Some(settings) => describe(settings),
        None => "auto-placed, extended, not primary (no override configured)".to_string(),
    }
}

fn describe(settings: &OutputSettings) -> String {
    let mut parts = Vec::new();
    if settings.primary {
        parts.push("primary".to_string());
    }
    match settings.refresh_rate {
        Some(mhz) => parts.push(format!("refresh={:.3}Hz", f64::from(mhz) / 1000.0)),
        None => parts.push("refresh=auto".to_string()),
    }
    if let Some(target) = &settings.mirror_of {
        parts.push(format!("mirrors {target}"));
    } else {
        match &settings.position {
            None => parts.push("position=auto".to_string()),
            Some(OutputPosition::RightOf { right_of }) => {
                parts.push(format!("right of {right_of}"))
            }
            Some(OutputPosition::LeftOf { left_of }) => parts.push(format!("left of {left_of}")),
            Some(OutputPosition::Above { above }) => parts.push(format!("above {above}")),
            Some(OutputPosition::Below { below }) => parts.push(format!("below {below}")),
            Some(OutputPosition::Absolute { x, y }) => parts.push(format!("at ({x}, {y})")),
        }
    }
    parts.join(", ")
}

fn settings_json(settings: &OutputSettings) -> serde_json::Value {
    serde_json::to_value(settings).unwrap_or(serde_json::Value::Null)
}

pub fn set(doc: &mut DocumentMut, name: &str, args: &SetArgs) -> Result<()> {
    let table = outputs_table(doc);
    if !table.contains_key(name) {
        table.insert(name, Item::Table(Table::new()));
    }
    let out = table[name]
        .as_table_mut()
        .expect("config.toml has a non-table [outputs.<name>]");

    if args.primary {
        out["primary"] = value(true);
    } else if args.no_primary {
        out.remove("primary");
    }

    if args.auto_refresh {
        out.remove("refresh_rate");
    } else if let Some(mhz) = args.refresh_mhz {
        out["refresh_rate"] = value(i64::from(mhz));
    } else if let Some(hz) = args.refresh_hz {
        if hz <= 0.0 {
            bail!("refresh rate must be positive, got {hz}");
        }
        out["refresh_rate"] = value((hz * 1000.0).round() as i64);
    }

    if args.auto_position {
        out.remove("mirror_of");
        out.remove("position");
    } else if let Some(target) = &args.mirror_of {
        out.remove("position");
        out["mirror_of"] = value(target.as_str());
    } else if let Some(target) = &args.right_of {
        out.remove("mirror_of");
        out["position"] = position_table("right_of", target);
    } else if let Some(target) = &args.left_of {
        out.remove("mirror_of");
        out["position"] = position_table("left_of", target);
    } else if let Some(target) = &args.above {
        out.remove("mirror_of");
        out["position"] = position_table("above", target);
    } else if let Some(target) = &args.below {
        out.remove("mirror_of");
        out["position"] = position_table("below", target);
    } else if let (Some(x), Some(y)) = (args.x, args.y) {
        out.remove("mirror_of");
        let mut pos = Table::new();
        pos.insert("x", value(i64::from(x)));
        pos.insert("y", value(i64::from(y)));
        pos.set_implicit(false);
        out["position"] = Item::Table(pos);
    }

    Ok(())
}

fn position_table(key: &str, target: &str) -> Item {
    let mut pos = Table::new();
    pos.insert(key, value(target));
    pos.set_implicit(false);
    Item::Table(pos)
}

pub fn remove(doc: &mut DocumentMut, name: &str) {
    outputs_table(doc).remove(name);
}

/// A monitor detected live via `wayland-info`. Refresh rates are in
/// millihertz, matching the Wayland protocol and the config file.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DetectedOutput {
    pub name: String,
    pub make: String,
    pub model: String,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    pub scale: i32,
    pub current_refresh: i32,
    pub refresh_rates: Vec<i32>,
}

pub fn detect() -> Result<Vec<DetectedOutput>> {
    let output = std::process::Command::new("wayland-info")
        .output()
        .map_err(|err| anyhow::anyhow!("running wayland-info: {err}"))?;
    if !output.status.success() {
        bail!(
            "wayland-info exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let outputs = parse_wayland_info(&String::from_utf8_lossy(&output.stdout));
    if outputs.is_empty() {
        bail!("wayland-info reported no monitors");
    }
    Ok(outputs)
}

struct DetectedMode {
    width: i32,
    height: i32,
    refresh: i32,
    current: bool,
    preferred: bool,
}

fn parse_wayland_info(data: &str) -> Vec<DetectedOutput> {
    let mut outputs = Vec::new();
    let mut current: Option<DetectedOutput> = None;
    let mut modes: Vec<DetectedMode> = Vec::new();

    let finish = |current: &mut Option<DetectedOutput>,
                  modes: &mut Vec<DetectedMode>,
                  outputs: &mut Vec<DetectedOutput>| {
        let Some(mut out) = current.take() else {
            modes.clear();
            return;
        };
        if out.name.is_empty() {
            modes.clear();
            return;
        }
        if out.scale < 1 {
            out.scale = 1;
        }
        let chosen = modes
            .iter()
            .position(|m| m.current)
            .or_else(|| modes.iter().position(|m| m.preferred))
            .or(if modes.is_empty() { None } else { Some(0) });
        if let Some(i) = chosen {
            let mode = &modes[i];
            out.width = mode.width / out.scale;
            out.height = mode.height / out.scale;
            out.current_refresh = mode.refresh;
            let mut rates: Vec<i32> = modes
                .iter()
                .filter(|m| m.width == mode.width && m.height == mode.height)
                .map(|m| m.refresh)
                .collect();
            rates.sort_unstable();
            rates.dedup();
            out.refresh_rates = rates;
        }
        if out.width <= 0 {
            out.width = 1920;
            out.height = 1080;
        }
        outputs.push(out);
        modes.clear();
    };

    for line in data.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("interface: '") {
            finish(&mut current, &mut modes, &mut outputs);
            if let Some(end) = rest.find('\'')
                && rest[..end].starts_with("wl_output")
            {
                current = Some(DetectedOutput {
                    name: String::new(),
                    make: String::new(),
                    model: String::new(),
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 0,
                    scale: 1,
                    current_refresh: 0,
                    refresh_rates: Vec::new(),
                });
            }
            continue;
        }
        let Some(out) = current.as_mut() else {
            continue;
        };

        if let Some(rest) = trimmed.strip_prefix("x: ") {
            if let Some((x, rest)) = rest.split_once(", y: ")
                && let Some((y, rest)) = rest.split_once(", scale: ")
            {
                out.x = x.trim().parse().unwrap_or(0);
                out.y = y.trim().parse().unwrap_or(0);
                out.scale = rest.trim().trim_end_matches(',').parse().unwrap_or(1);
            }
        } else if let Some(rest) = trimmed.strip_prefix("make: '") {
            if let Some((make, rest)) = rest.split_once("', model: '") {
                out.make = make.to_string();
                out.model = match rest.find('\'') {
                    Some(end) => rest[..end].to_string(),
                    None => rest.trim_end_matches(',').to_string(),
                };
            }
        } else if line.starts_with(char::is_whitespace)
            && !line.starts_with("\t\t")
            && let Some(rest) = trimmed.strip_prefix("name: ")
        {
            out.name = rest.trim().trim_matches('\'').to_string();
        }
        if let Some(rest) = trimmed.strip_prefix("width: ") {
            if let Some((width, rest)) = rest.split_once(" px, height: ")
                && let Some((height, rest)) = rest.split_once(" px, refresh: ")
                && let Some(hz) = rest.split_whitespace().next()
            {
                let width: i32 = width.trim().parse().unwrap_or(0);
                let height: i32 = height.trim().parse().unwrap_or(0);
                let hz: f64 = hz.parse().unwrap_or(0.0);
                modes.push(DetectedMode {
                    width,
                    height,
                    refresh: (hz * 1000.0).round() as i32,
                    current: false,
                    preferred: false,
                });
            }
        } else if trimmed.contains("flags:")
            && let Some(last) = modes.last_mut()
        {
            last.current = trimmed.contains("current");
            last.preferred = trimmed.contains("preferred");
        }
    }
    finish(&mut current, &mut modes, &mut outputs);
    outputs.sort_by(|a, b| a.name.cmp(&b.name));
    outputs
}

pub fn format_refresh(rate_mhz: i32) -> String {
    if rate_mhz % 1000 == 0 {
        format!("{} Hz", rate_mhz / 1000)
    } else {
        format!("{:.3} Hz", f64::from(rate_mhz) / 1000.0)
    }
}
