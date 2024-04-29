use config::{self, Config, ConfigError, File, FileFormat};
use mostro_core::message::Action;
use serde::{Deserialize, Serialize};
use std::default;
use std::io::Write;
use std::path::Path;

#[derive(Clone, Default, Deserialize, Serialize)]
pub struct MostroStats {
    pub received: u64,
    pub new_order: u64,
    pub new_dispute: u64,
    pub release: u64,
    pub bytes_recv: u64,
    pub max_msg_bytes: u64,
    pub min_msg_bytes: u64,
    pub last_msg_bytes: u64,
}



#[derive(Clone, Default, Deserialize, Serialize)]
pub struct MostroMessageStats {
    pub overall_stats: MostroStats,
    pub monthly_stats: MostroStats,
}

impl<'a> IntoIterator for &'a MostroMessageStats {
    type Item = MostroStats;
    type IntoIter = MostroStatsIterator<'a>;

    fn into_iter(self) -> Self::IntoIter {
        MostroStatsIterator {
            stats: self,
            index: 0,
        }
    }
}


pub struct MostroStatsIterator<'a> {
    stats: &'a MostroMessageStats,
    index: usize,
}

impl<'a> Iterator for MostroStatsIterator<'a> {
    type Item = MostroStats;

    fn next(&mut self) -> Option<Self::Item> {
        let res = match self.index {
            0 => self.stats.overall_stats,
            1 => self.stats.monthly_stats,
            _ => return None,
        };
        self.index += 1;
        Some(res)
    }
}


impl MostroMessageStats {
    pub fn reset_counters(&mut self) -> Self {
        Self::default()
    }

    pub fn reset_monthly_counters(&mut self) {
        self.monthly_stats = MostroStats::default()
    }

    pub fn reset_overall_counters(&mut self) {
        self.overall_stats = MostroStats::default()
    }

    pub fn message_inc_counter(&mut self, kind: &Action) {

     for mut stat in self.into_iter() {

        match kind {
            Action::NewOrder => stat.new_order += 1,
            Action::Dispute => stat.new_dispute += 1,
            Action::Release => stat.release += 1,
            _ => stat.received += 1,
        }
    }
    }



    pub fn data_recv(&mut self, data: usize) {


        for stat in iter.iter_mut(){
            // Total byte receviced
            stat.bytes_recv += data as u64;
            // Last message bytes
            stat.last_msg_bytes = data as u64;
            // Max bytes
            if stat.max_msg_bytes < data as u64 {
                stat.max_msg_bytes = data as u64
            }
            // Min bytes
            if stat.min_msg_bytes > data as u64 {
                stat.min_msg_bytes = data as u64
            }
        }
    }
}

impl core::fmt::Debug for MostroStats {
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
}




