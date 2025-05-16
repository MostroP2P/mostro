// / CLI for Mostro
// / Initialize the default directory for the settings file
//! CLI

use crate::config::util::init_configuration_file;
use clap::Parser;
use mostro_core::prelude::*;

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

/// Initialize the settings file and create the global config variable for Mostro settings
/// Default folder is HOME but user can specify a custom folder with dirsettings (-d ) parameter from CLI
/// Example: mostro p2p -d /user_folder/mostro
pub fn settings_init() -> Result<(), Box<dyn std::error::Error>> {
    // Parse CLI arguments
    let cli = Cli::parse();

    // Select config file from CLI or default to HOME/.mostro
    // create config file if it doesn't exist
    if let Some(path) = cli.dirsettings.as_deref() {
        init_configuration_file(Some(path.to_string()))?
    } else {
        init_configuration_file(None)?
    };

    // Mostro settings are initialized
    Ok(())
}
