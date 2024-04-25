use config::{self, Config, ConfigError, File, FileFormat};
use mostro_core::message::Action;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::Path;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct MostroMessageStats {
    pub received: u64,
    pub new_order: u64,
    pub new_dispute: u64,
    pub release: u64,
    pub bytes_recv: u64,
    pub max_msg_bytes: u64,
    pub min_msg_bytes: u64,
    pub last_msg_bytes: u64,
}

impl core::fmt::Debug for MostroMessageStats {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        let mut ds = f.debug_struct("MostroMessageStats");
        ds.field("Total messages received", &self.received);
        ds.field("Orders created", &self.new_order);
        ds.field("Disputes created", &self.new_dispute);
        ds.field("Orders released", &self.release);
        ds.field("Max message size (bytes)", &self.max_msg_bytes);
        ds.field("Min message size (bytes)", &self.min_msg_bytes);
        ds.field("Last message size (bytes)", &self.last_msg_bytes);
        ds.field("Total bytes received", &self.bytes_recv);
        ds.finish()
    }
}

impl MostroMessageStats {
    pub fn new() -> Result<Self, ConfigError> {
        if !Path::new("mostro_stats.json").exists() {
            let mut f = std::fs::File::create("mostro_stats.json").map_err(|_| {
                ConfigError::NotFound("File mostro_stats.json not found".to_string())
            })?;
            let s = serde_json::to_string(&MostroMessageStats::default());
            println!("{:?}", s);
            let _ = f.write(s.unwrap().as_bytes());
        }

        let stats = Config::builder()
            .add_source(File::new("mostro_stats.json", FileFormat::Json))
            .build()?;
        stats.try_deserialize()
    }

    pub fn message_inc_counter(&mut self, kind: &Action) {
        match kind {
            Action::NewOrder => self.new_order += 1,
            Action::Dispute => self.new_dispute += 1,
            Action::Release => self.release += 1,
            _ => self.received += 1,
        }
    }

    pub fn data_recv(&mut self, data: usize) {
        // Total byte receviced
        self.bytes_recv += data as u64;
        // Last message bytes
        self.last_msg_bytes = data as u64;
        // Max bytes
        if self.max_msg_bytes < data as u64 {
            self.max_msg_bytes = data as u64
        }
        // Min bytes
        if self.min_msg_bytes > data as u64 {
            self.min_msg_bytes = data as u64
        }
    }
}
