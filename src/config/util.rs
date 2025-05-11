/// Utility functions for the config module
/// This module provides utility functions for the config module.
/// It includes functions to initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// It also includes functions to add a trailing slash to a path if it doesn't already have one.
use std::path::MAIN_SEPARATOR;
use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

use mostro_core::error::MostroError::{self, *};
use mostro_core::error::ServiceError;

#[cfg(windows)]
pub fn has_trailing_slash(p: &Path) -> bool {
    let last = p.as_os_str().encode_wide().last();
    last == Some(b'\\' as u16) || last == Some(b'/' as u16)
}
#[cfg(unix)]
pub fn has_trailing_slash(p: &Path) -> bool {
    p.as_os_str().as_bytes().last() == Some(&b'/')
}

pub fn add_trailing_slash(p: &mut PathBuf) {
    if !has_trailing_slash(p) {
        let mut s = p.as_os_str().to_os_string();
        s.push(format!("{MAIN_SEPARATOR}"));
        *p = PathBuf::from(s);
    }
}

/// Initialize the default settings directory and create a settings file from the template if it doesn't exist.
/// Checks if the directory already exists, and if not, creates it and writes the template file.
/// If a custom config path is provided, it uses that instead of the default `~/.mostro` directory.
pub fn init_default_dir(config_path: Option<String>) -> Result<PathBuf, MostroError> {
    let settings_dir = if let Some(path) = config_path {
        PathBuf::from(path)
    } else {
        let home = std::env::var("HOME")
            .map_err(|e| MostroInternalErr(ServiceError::EnvVarError(e.to_string())))?;
        PathBuf::from(home).join(".mostro")
    };

    if !settings_dir.exists() {
        std::fs::create_dir_all(&settings_dir)
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

        let config_path = settings_dir.join("settings.toml");
        std::fs::write(&config_path, include_bytes!("../../settings.tpl.toml"))
            .map_err(|e| MostroInternalErr(ServiceError::IOError(e.to_string())))?;

        println!(
            "Created settings file from template at {} for mostro - Edit it to configure your Mostro instance",
            config_path.display()
        );
        std::process::exit(0);
    }

    Ok(settings_dir)
}
