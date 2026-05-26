mod app;
mod refresh;
mod theme;
mod ui;

use std::io;
use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::Parser;

use crate::app::App;

#[derive(Parser, Debug)]
#[command(version, about = "TokenGauge TUI")]
struct Args {
    #[arg(long, env = "TOKENGAUGE_CONFIG")]
    config: Option<PathBuf>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    if !crossterm::tty::IsTty::is_tty(&io::stdout()) {
        return Err(anyhow!("tokengauge-tui must run in a TTY"));
    }

    let mut terminal = ratatui::init();
    let mut app = App::new(args.config);
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}
