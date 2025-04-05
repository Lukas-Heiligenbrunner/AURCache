use crate::Mirror;
use crate::mirror::Mirrors;
use anyhow::anyhow;
use chrono::Utc;
use log::info;
use reqwest::Client;
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
    async fn measure_duration(&mut self, target_db: TargetDb) -> anyhow::Result<f64>;
}

pub trait Bench {
    /// Rank the mirrors based on the score.
    fn rank(&mut self) -> impl Future<Output = anyhow::Result<Vec<Mirror>>> + Send;

    fn gen_mirrorlist(&self, mirrors: Vec<Mirror>) -> anyhow::Result<String>;
}

impl Bench for Mirrors {
    async fn rank(&mut self) -> anyhow::Result<Vec<Mirror>> {
        let mut durations = Vec::new();
        for mirror in self.0.iter_mut() {
            // Skip mirrors that are not active
            if !mirror.active {
                continue;
            }

            // only use http(s) mirrors
            if mirror.protocol != crate::Protocol::Http && mirror.protocol != crate::Protocol::Https
            {
                continue;
            }

            info!("Benchmarking {}", mirror.url);
            let duration = mirror.measure_duration(TargetDb::Core).await;
            match duration {
                Ok(duration) => {
                    durations.push((mirror, duration));
                }
                Err(err) => {
                    info!("Failed to measure duration for {}: {}", mirror.url, err);
                    continue;
                }
            }
        }

        // Sort by duration (ascending order)
        durations.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());

        // Extract only the sorted Mirror references
        Ok(durations
            .into_iter()
            .map(|(mirror, _)| mirror.clone())
            .collect())
    }

    fn gen_mirrorlist(&self, mirrors: Vec<Mirror>) -> anyhow::Result<String> {
        let mut body = format!(
            r#"##
## Arch Linux repository mirrorlist
## Created by aurcache
## Generated on {}
##
"#,
            Utc::now().date_naive()
        );

        for mirror in &mirrors[..10] {
            body.push_str(&format!("## {}\n", mirror.country.kind));
            body.push_str(&format!("Server = {}$repo/os/$arch\n", mirror.url));
            body.push('\n');
        }

        Ok(body)
    }
}

impl Benchmark for Mirror {
    async fn measure_duration(&mut self, target_db: TargetDb) -> anyhow::Result<f64> {
        let url = &self.url;
        let url: Url = match target_db {
            TargetDb::Core => url.join("core/os/x86_64/core.db")?,
            TargetDb::Extra => url.join("extra/os/x86_64/extra.db")?,
        };

        let start = Instant::now();

        match Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?
            .get(url.as_str())
            .send()
            .await
        {
            Ok(response) => {
                let transfer_time: f64 = start.elapsed().as_secs_f64();

                let file_size = response.bytes().await?.len();
                let transfer_rate = (file_size as f64) / (transfer_time * 1024.0);
                info!("Transfer Rate: {url} => {transfer_rate:.2} kb/s");
                Ok(transfer_rate)
            }
            Err(err) => Err(anyhow!(err)),
        }
    }
}
