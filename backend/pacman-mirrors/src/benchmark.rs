use crate::Mirror;
use crate::mirror::Mirrors;
use anyhow::bail;
use log::debug;
use std::time::{Duration, Instant};
use url::Url;

#[derive(PartialEq, Eq, Debug, Clone, Copy)]
pub enum TargetDb {
    Core,
    Extra,
}

trait Benchmark {
    /// Measure time (in seconds) it took to connect (from user's geography)
    /// and retrive the '[core,extra]/os/x86_64/[core,extra].db' file from the given URL.
    fn measure_duration(&mut self, target_db: TargetDb) -> anyhow::Result<f64>;
}

pub trait Rank {
    /// Rank the mirrors based on the score.
    fn rank(&mut self) -> anyhow::Result<Vec<Mirror>>;
}

impl Rank for Mirrors {
    fn rank(&mut self) -> anyhow::Result<Vec<Mirror>> {
        let mut val = self
            .0
            .iter_mut()
            .map(|mirror| {
                let duration = mirror.measure_duration(TargetDb::Extra);
                (mirror, duration)
            })
            .filter_map(|(mirror, duration)| duration.map(|duration| (mirror, duration)).ok())
            .collect::<Vec<(&mut Mirror, f64)>>();

        // Sort by duration (ascending order)
        val.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // Extract only the sorted Mirror references
        Ok(val.into_iter().map(|(mirror, _)| mirror.clone()).collect())
    }
}

impl Benchmark for Mirror {
    fn measure_duration(&mut self, target_db: TargetDb) -> anyhow::Result<f64> {
        let url = &self.url;
        let url: Url = match target_db {
            TargetDb::Core => url.join("core/os/x86_64/core.db")?,
            TargetDb::Extra => url.join("extra/os/x86_64/extra.db")?,
        };

        let start = Instant::now();

        match reqwest::blocking::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?
            .get(url.as_str())
            .build()
        {
            Ok(response) => {
                let transfer_time: f64 = start.elapsed().as_secs_f64();

                let file_size = response.body().unwrap().as_bytes().unwrap().len();
                let transfer_rate = (file_size as f64) / transfer_time;
                debug!("Transfer Rate: {url} => {transfer_rate}");
                Ok(transfer_rate)
            }
            Err(err) => {
                bail!(format!("Failed to fetch `{url}, HTTP status code: {err}`"))
            }
        }
    }
}
