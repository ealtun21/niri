use std::ffi::OsString;
use std::path::PathBuf;

use clap::{Parser, Subcommand};
use clap_complete::Shell;
use niri_ipc::{Action, AgentButton, AgentMod, OutputAction};

use crate::utils::version;

#[derive(Parser)]
#[command(author, version = version(), about, long_about = None)]
#[command(args_conflicts_with_subcommands = true)]
#[command(subcommand_value_name = "SUBCOMMAND")]
#[command(subcommand_help_heading = "Subcommands")]
pub struct Cli {
    /// Path to config file (default: `$XDG_CONFIG_HOME/niri/config.kdl`).
    ///
    /// This can also be set with the `NIRI_CONFIG` environment variable. If both are set, the
    /// command line argument takes precedence.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Import environment globally to systemd and D-Bus, run D-Bus services.
    ///
    /// Set this flag in a systemd service started by your display manager, or when running
    /// manually as your main compositor instance. Do not set when running as a nested window, or
    /// on a TTY as your non-main compositor instance, to avoid messing up the global environment.
    #[arg(long)]
    pub session: bool,
    /// Command to run upon compositor startup.
    #[arg(last = true)]
    pub command: Vec<OsString>,

    #[command(subcommand)]
    pub subcommand: Option<Sub>,
}

#[derive(Subcommand)]
pub enum Sub {
    /// Communicate with the running niri instance.
    Msg {
        #[command(subcommand)]
        msg: Msg,
        /// Format output as JSON.
        #[arg(short, long)]
        json: bool,
        /// Print the IPC request as JSON instead of sending it.
        #[arg(long)]
        print_request: bool,
    },
    /// Serve MCP on stdio for AI agents driving windows.
    #[cfg(feature = "dbus")]
    Mcp,
    /// Validate the config file.
    Validate {
        /// Path to config file (default: `$XDG_CONFIG_HOME/niri/config.kdl`).
        ///
        /// This can also be set with the `NIRI_CONFIG` environment variable. If both are set, the
        /// command line argument takes precedence.
        #[arg(short, long)]
        config: Option<PathBuf>,
    },
    /// Cause a panic to check if the backtraces are good.
    Panic,
    /// Generate shell completions.
    Completions { shell: CompletionShell },
}

#[derive(Subcommand)]
pub enum Msg {
    /// List connected outputs.
    Outputs,
    /// List workspaces.
    Workspaces,
    /// List open windows.
    Windows,
    /// List open layer-shell surfaces.
    Layers,
    /// Get the configured keyboard layouts.
    KeyboardLayouts,
    /// Print information about the focused output.
    FocusedOutput,
    /// Print information about the focused window.
    FocusedWindow,
    /// Pick a window with the mouse and print information about it.
    PickWindow,
    /// Pick a color from the screen with the mouse.
    PickColor,
    /// Perform an action.
    Action {
        #[command(subcommand)]
        action: Action,
    },
    /// Change output configuration temporarily.
    ///
    /// The configuration is changed temporarily and not saved into the config file. If the output
    /// configuration subsequently changes in the config file, these temporary changes will be
    /// forgotten.
    Output {
        /// Output name.
        ///
        /// Run `niri msg outputs` to see the output names.
        #[arg()]
        output: String,
        /// Configuration to apply.
        #[command(subcommand)]
        action: OutputAction,
    },
    /// Start continuously receiving events from the compositor.
    EventStream,
    /// Print the version of the running niri instance.
    Version,
    /// Request an error from the running niri instance.
    RequestError,
    /// Print the overview state.
    OverviewState,
    /// List screencasts.
    Casts,
    /// Capture a window to a PNG file.
    CaptureWindow {
        /// Id of the window.
        #[arg(long)]
        id: u64,
        /// Path of the PNG file to write.
        #[arg(long)]
        path: String,
        /// Longest edge of the image in pixels.
        #[arg(long)]
        max_edge: Option<u32>,
        /// Milliseconds to wait for the window to settle before capturing.
        #[arg(long)]
        settle_ms: Option<u32>,
    },
    /// Print the agent status.
    AgentState {
        /// Keep printing the status as it changes.
        #[arg(long)]
        watch: bool,
    },
    /// Pause agent input. Only the owner can unpause, by keybind.
    AgentPause,
    /// Perform an agent input operation on a window.
    Agent {
        /// Id of the window.
        #[arg(long)]
        id: u64,
        #[command(subcommand)]
        op: AgentCmd,
    },
    /// Send a raw JSON request to the compositor, reading from stdin.
    RawRequest,
}

/// Agent input operations; coordinates are pixels of the last capture.
#[derive(Subcommand)]
pub enum AgentCmd {
    /// Click at a point.
    #[command(allow_negative_numbers = true)]
    Click {
        x: f64,
        y: f64,
        #[arg(long, value_enum, default_value = "left")]
        button: AgentButton,
        #[arg(long, default_value_t = 1)]
        count: u8,
        /// Comma-separated modifiers, e.g. `ctrl,shift`.
        #[arg(long, value_enum, value_delimiter = ',')]
        mods: Vec<AgentMod>,
    },
    /// Move the pointer.
    #[command(allow_negative_numbers = true)]
    Move { x: f64, y: f64 },
    /// Scroll at a point.
    #[command(allow_negative_numbers = true)]
    Scroll {
        x: f64,
        y: f64,
        #[arg(long, default_value_t = 0.)]
        dx: f64,
        #[arg(long, default_value_t = 0.)]
        dy: f64,
    },
    /// Drag from one point to another.
    #[command(allow_negative_numbers = true)]
    Drag {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        #[arg(long, default_value_t = 10)]
        steps: u16,
    },
    /// Press a key chord such as `ctrl+l`.
    Key {
        chord: String,
        #[arg(long, default_value_t = 1)]
        repeat: u16,
    },
    /// Type text.
    Type {
        #[arg(allow_hyphen_values = true)]
        text: String,
    },
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum CompletionShell {
    Bash,
    Elvish,
    Fish,
    PowerShell,
    Zsh,
    Nushell,
}

impl TryFrom<CompletionShell> for Shell {
    type Error = &'static str;

    fn try_from(shell: CompletionShell) -> Result<Self, Self::Error> {
        match shell {
            CompletionShell::Bash => Ok(Shell::Bash),
            CompletionShell::Elvish => Ok(Shell::Elvish),
            CompletionShell::Fish => Ok(Shell::Fish),
            CompletionShell::PowerShell => Ok(Shell::PowerShell),
            CompletionShell::Zsh => Ok(Shell::Zsh),
            CompletionShell::Nushell => Err("Nushell should be handled separately"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agent_state_watch() {
        let Some(Sub::Msg {
            msg: Msg::AgentState { watch },
            ..
        }) = Cli::try_parse_from(["niri", "msg", "agent-state", "--watch"])
            .unwrap()
            .subcommand
        else {
            panic!("expected Msg::AgentState");
        };
        assert!(watch);
    }

    #[test]
    fn parses_agent_pause() {
        assert!(matches!(
            Cli::try_parse_from(["niri", "msg", "agent-pause"])
                .unwrap()
                .subcommand,
            Some(Sub::Msg {
                msg: Msg::AgentPause,
                ..
            })
        ));
    }

    #[test]
    fn parse_agent_click() {
        let cli = Cli::try_parse_from([
            "niri",
            "msg",
            "agent",
            "--id",
            "7",
            "click",
            "10",
            "20",
            "--button",
            "right",
            "--mods",
            "ctrl,shift",
        ])
        .unwrap();
        let Some(Sub::Msg {
            msg: Msg::Agent { id, op },
            ..
        }) = cli.subcommand
        else {
            panic!("expected Msg::Agent");
        };
        assert_eq!(id, 7);
        let AgentCmd::Click {
            x,
            y,
            button,
            count,
            mods,
        } = op
        else {
            panic!("expected click");
        };
        assert_eq!((x, y, count), (10., 20., 1));
        assert_eq!(button, AgentButton::Right);
        assert_eq!(mods, [AgentMod::Ctrl, AgentMod::Shift]);
    }

    #[test]
    fn parse_agent_negative_numbers() {
        let parse = |args: &[&str]| {
            let mut v = vec!["niri", "msg", "agent", "--id", "7"];
            v.extend_from_slice(args);
            Cli::try_parse_from(v)
        };
        let Some(Sub::Msg {
            msg:
                Msg::Agent {
                    op: AgentCmd::Scroll { x, y, dx, dy },
                    ..
                },
            ..
        }) = parse(&["scroll", "1", "2", "--dy", "-3"])
            .unwrap()
            .subcommand
        else {
            panic!("expected scroll");
        };
        assert_eq!((x, y, dx, dy), (1., 2., 0., -3.));

        let Some(Sub::Msg {
            msg:
                Msg::Agent {
                    op: AgentCmd::Move { x, y },
                    ..
                },
            ..
        }) = parse(&["move", "-5", "20"]).unwrap().subcommand
        else {
            panic!("expected move");
        };
        assert_eq!((x, y), (-5., 20.));

        assert!(parse(&["click", "-5", "20"]).is_ok());
        assert!(parse(&["drag", "-1", "-2", "-3", "-4"]).is_ok());
    }

    #[test]
    fn parse_agent_type_hyphen_text() {
        for args in [&["type", "--", "-x"][..], &["type", "-x"]] {
            let mut v = vec!["niri", "msg", "agent", "--id", "7"];
            v.extend_from_slice(args);
            let Some(Sub::Msg {
                msg:
                    Msg::Agent {
                        op: AgentCmd::Type { text },
                        ..
                    },
                ..
            }) = Cli::try_parse_from(v).unwrap().subcommand
            else {
                panic!("expected type");
            };
            assert_eq!(text, "-x", "{args:?}");
        }
    }
}
