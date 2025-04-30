// Mostro module for configurataion settings
/// This module provides functionality to manage and initialize settings for the Mostro application.
/// It includes structures for database, lightning, Nostr, and Mostro settings, as well as functions to initialize and access these settings.
pub mod types;
pub mod util;
pub mod settings;

// Re-export for convenience
pub use settings::{Settings, init_global_settings};
pub use types::{Database, Lightning, Mostro, Nostr};
pub use util::init_default_dir;
