// Mostro module for configurataion settings
pub mod settings;
/// This module provides functionality to manage and initialize settings for the Mostro application.
/// It includes structures for database, lightning, Nostr, and Mostro settings, as well as functions to initialize and access these settings.
pub mod types;
pub mod util;

// Re-export for convenience
pub use settings::{init_global_settings, Settings};
pub use types::{Database, Lightning, Mostro, Nostr};
pub use util::init_default_dir;
