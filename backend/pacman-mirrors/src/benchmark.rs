use crate::Mirror;
use crate::mirror::Mirrors;
use reqwest::Client;
use std::fmt::Write as _;
use std::time::{Duration, Instant};
use tracing::info;
use url::Url;

pub trait Bench {
    /// Rank the mirrors fastest-first.
    fn rank(&self) -> impl Future<Output = anyhow::Result<Vec<Mirror>>> + Send;
}

/// Render a `pacman` mirrorlist from (at most) the first ten mirrors given.
#[must_use]
pub fn gen_mirrorlist(mirrors: &[Mirror]) -> String {
    let mut body = format!(
        r"##
## Arch Linux repository mirrorlist
## Created by aurcache
## Generated on {}
##
",
        jiff::Timestamp::now().strftime("%Y-%m-%d")
    );

    for mirror in mirrors.iter().take(10) {
        let _ = writeln!(body, "## {}", mirror.country.kind);
        let _ = writeln!(body, "Server = {}$repo/os/$arch", mirror.url);
        body.push('\n');
    }

    body
}

impl Bench for Mirrors {
    async fn rank(&self) -> anyhow::Result<Vec<Mirror>> {
        // One client for the whole rank: connection pooling and TLS session
        // resumption apply across probes, and setup is not timed per mirror.
        let client = Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .build()?;
        let mut durations = Vec::new();
        for mirror in &self.0 {
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
            match mirror.measure_duration(&client).await {
                Ok(duration) => durations.push((mirror, duration)),
                Err(err) => {
                    info!("Failed to measure duration for {}: {}", mirror.url, err);
                }
            }
        }

        // Fastest first: every mirror served the same file, so the elapsed
        // download time is directly comparable.
        durations.sort_by_key(|(_, duration)| *duration);

        Ok(durations
            .into_iter()
            .map(|(mirror, _)| mirror.clone())
            .collect())
    }
}

impl Mirror {
    /// Time a successful download of the benchmark database using the ranker's client.
    async fn measure_duration(&self, client: &Client) -> anyhow::Result<Duration> {
        let url: Url = self.url.join("core/os/x86_64/core.db")?;

        // The measurement spans connect + headers + the whole body: stopping
        // the clock at the response headers would time the handshake only and
        // ignore how fast the mirror actually serves data.
        let start = Instant::now();
        let response = client.get(url.as_str()).send().await?.error_for_status()?;
        let size = response.bytes().await?.len();
        let elapsed = start.elapsed();

        let rate = size as f64 / elapsed.as_secs_f64().max(f64::EPSILON) / 1024.0;
        info!(
            "{url} => {:.2}s for {size} bytes ({rate:.0} KiB/s)",
            elapsed.as_secs_f64()
        );
        Ok(elapsed)
    }
}

#[cfg(test)]
mod tests {
    use super::gen_mirrorlist;
    use crate::{Country, Mirror, Protocol};

    fn mirror(host: &str) -> Mirror {
        Mirror {
            url: format!("https://{host}/archlinux/").parse().unwrap(),
            protocol: Protocol::Https,
            last_sync: None,
            completion_pct: None,
            duration_avg: None,
            duration_stddev: None,
            score: None,
            active: true,
            country: Country::new("Austria", "AT"),
            isos: false,
            ipv4: true,
            ipv6: false,
            details: String::new(),
        }
    }

    #[tokio::test]
    async fn http_errors_are_not_ranked_as_fast_mirrors() {
        use super::Bench;
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/archlinux/core/os/x86_64/core.db"))
            .respond_with(ResponseTemplate::new(404).set_body_string("not found"))
            .expect(1)
            .mount(&server)
            .await;
        let mut mirror = mirror("example.com");
        mirror.url = format!("{}/archlinux/", server.uri()).parse().unwrap();
        mirror.protocol = Protocol::Http;

        let ranked = crate::mirror::Mirrors(vec![mirror]).rank().await.unwrap();
        assert!(ranked.is_empty());
    }

    /// Ranking only returns the mirrors that actually responded, which is
    /// routinely fewer than ten; slicing a fixed ten off the front used to
    /// panic here.
    #[test]
    fn gen_mirrorlist_handles_fewer_than_ten_mirrors() {
        let mirrors: Vec<Mirror> = (0..3).map(|i| mirror(&format!("m{i}.example"))).collect();
        let list = gen_mirrorlist(&mirrors);

        assert_eq!(list.matches("Server = ").count(), 3);
        assert!(list.contains("Server = https://m0.example/archlinux/$repo/os/$arch"));
        assert!(gen_mirrorlist(&[]).contains("Created by aurcache"));
    }

    /// The mirrorlist is capped, and keeps the ranking order it was given.
    #[test]
    fn gen_mirrorlist_keeps_the_first_ten_in_order() {
        let mirrors: Vec<Mirror> = (0..15)
            .map(|i| mirror(&format!("m{i:02}.example")))
            .collect();
        let list = gen_mirrorlist(&mirrors);

        assert_eq!(list.matches("Server = ").count(), 10);
        assert!(list.contains("m00.example"));
        assert!(list.contains("m09.example"));
        assert!(!list.contains("m10.example"));

        let first = list.find("m00.example").unwrap();
        let second = list.find("m01.example").unwrap();
        assert!(first < second, "ranking order must be preserved");
    }
}
