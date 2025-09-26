// / CLI for Mostro
// / Initialize the default directory for the settings file
//! CLI

use crate::config::util::init_configuration_file;
#[cfg(feature = "startos")]
use crate::config::MOSTRO_DB_PASSWORD;
use clap::Parser;
#[cfg(feature = "startos")]
use secrecy::SecretString;

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
    /// Set password for db encryption
    #[cfg(feature = "startos")]
    #[arg(short, long)]
    password: Option<String>,
    /// Set cleartext password for db encryption (no password required)
    #[cfg(feature = "startos")]
    #[arg(short, long, action = clap::ArgAction::SetTrue)]
    pub cleartext: bool,
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

    #[cfg(feature = "startos")]
    if let Some(password) = cli.password.as_deref() {
        let _ = MOSTRO_DB_PASSWORD.set(SecretString::from(password.to_string()));
    }

    // Mostro settings are initialized
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_cli_parser_creation() {
        // Test that CLI struct can be created
        #[cfg(feature = "startos")]
        let cli = Cli {
            dirsettings: None,
            password: None,
            cleartext: false,
        };
        #[cfg(not(feature = "startos"))]
        let cli = Cli { dirsettings: None };
        assert!(cli.dirsettings.is_none());

        #[cfg(feature = "startos")]
        let cli_with_path = Cli {
            dirsettings: Some("/custom/path".to_string()),
            password: None,
            cleartext: false,
        };
        #[cfg(not(feature = "startos"))]
        let cli_with_path = Cli {
            dirsettings: Some("/custom/path".to_string()),
        };
        assert_eq!(cli_with_path.dirsettings.unwrap(), "/custom/path");
    }

    #[test]
    fn test_cli_parsing_help() {
        // Test that help can be parsed without panicking
        let result = Cli::try_parse_from(["mostro", "--help"]);
        assert!(result.is_err()); // Help exits with error code
    }

    #[test]
    fn test_cli_parsing_version() {
        // Test that version can be parsed without panicking
        let result = Cli::try_parse_from(["mostro", "--version"]);
        assert!(result.is_err()); // Version exits with error code
    }

    #[test]
    fn test_cli_parsing_no_args() {
        // Test parsing with no arguments (should succeed)
        let result = Cli::try_parse_from(["mostro"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(cli.dirsettings.is_none());
    }

    #[test]
    fn test_cli_parsing_with_dirsettings_short() {
        // Test parsing with short flag -d
        let result = Cli::try_parse_from(["mostro", "-d", "/test/path"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert_eq!(cli.dirsettings.unwrap(), "/test/path");
    }

    #[test]
    fn test_cli_parsing_with_dirsettings_long() {
        // Test parsing with long flag --dirsettings
        let result = Cli::try_parse_from(["mostro", "--dirsettings", "/test/path"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert_eq!(cli.dirsettings.unwrap(), "/test/path");
    }

    #[test]
    fn test_cli_parsing_invalid_args() {
        // Test parsing with invalid arguments
        let result = Cli::try_parse_from(["mostro", "--invalid"]);
        assert!(result.is_err());
    }

    #[test]
    #[cfg(feature = "startos")]
    fn test_cli_parsing_cleartext_flag() {
        // Test parsing with cleartext flag (no value required)
        let result = Cli::try_parse_from(["mostro", "-c"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(cli.cleartext);

        // Test parsing with long cleartext flag
        let result = Cli::try_parse_from(["mostro", "--cleartext"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(cli.cleartext);

        // Test parsing without cleartext flag (should default to false)
        let result = Cli::try_parse_from(["mostro"]);
        assert!(result.is_ok());
        let cli = result.unwrap();
        assert!(!cli.cleartext);
    }

    mod settings_init_tests {
        use super::*;

        #[test]
        fn test_settings_init_structure() {
            // This is a structural test since we can't easily mock the CLI parsing
            // In a real implementation, we would need dependency injection for testing

            // Test that the function signature is correct
            let _: fn() -> Result<(), Box<dyn std::error::Error>> = settings_init;

            // Verify function exists and has correct return type
        }

        #[test]
        fn test_custom_path_handling() {
            // Test the logical flow of custom path handling
            let custom_path = Some("/custom/path".to_string());
            #[cfg(feature = "startos")]
            let cli = Cli {
                dirsettings: custom_path.clone(),
                password: None,
                cleartext: false,
            };
            #[cfg(not(feature = "startos"))]
            let cli = Cli {
                dirsettings: custom_path.clone(),
            };

            if let Some(path) = cli.dirsettings.as_deref() {
                assert_eq!(path, "/custom/path");
            } else {
                panic!("Custom path should be present");
            }
        }

        #[test]
        fn test_default_path_handling() {
            // Test the logical flow of default path handling
            #[cfg(feature = "startos")]
            let cli = Cli {
                dirsettings: None,
                password: None,
                cleartext: false,
            };
            #[cfg(not(feature = "startos"))]
            let cli = Cli { dirsettings: None };

            if cli.dirsettings.is_none() {
                // This is the expected path for default settings
            } else {
                panic!("Default path should be None");
            }
        }
    }
}
