pub mod settings;

use crate::cli::settings::init_default_dir;
use clap::Parser;
use mostro_core::error::MostroError;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "mostro p2p",
    about = "A P2P lightning exchange over Nostr",
    author,
    help_template = "\
{before-help}{name}

{about-with-newline}
{author-with-newline}
{usage-heading} {usage}

{all-args}{after-help}
",
    version
)]
#[command(propagate_version = true)]
#[command(arg_required_else_help(false))]
pub struct Cli {
    /// Set folder for Mostro settings file - default is HOME/.mostro
    #[arg(short, long)]
    dirsettings: Option<String>,
}

pub fn settings_init() -> Result<PathBuf, MostroError> {
    let cli = Cli::parse();

    if let Some(path) = cli.dirsettings.as_deref() {
        Ok(init_default_dir(Some(path.to_string()))?)
    } else {
        Ok(init_default_dir(None)?)
    }
}
