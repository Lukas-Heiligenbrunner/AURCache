//! Find out what a container can reach over TCP.

use anyhow::{Context, Result};
use aurcache_build_agent::report::Reach;
use std::io::Write;
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// One target's answer.
struct Answer {
    index: usize,
    reachable: bool,
}

/// Try every target at once and print the `[Reach]` list, in the order given,
/// as one line of JSON.
///
/// No target takes more than `timeout` in all: a connection that has not
/// completed by then, and a name that has not resolved by then, count as
/// unreachable.
pub fn run(targets: &[String], timeout: Duration) -> Result<()> {
    let reached = reach_all(targets, timeout);
    let list: Vec<Reach> = targets
        .iter()
        .zip(reached)
        .map(|(target, reachable)| Reach {
            target: target.clone(),
            reachable,
        })
        .collect();
    let mut line = serde_json::to_vec(&list).context("cannot encode the answer")?;
    line.push(b'\n');
    let mut out = std::io::stdout().lock();
    out.write_all(&line)
        .and_then(|()| out.flush())
        .context("cannot write the answer")
}

/// Whether each target answered, in the order of `targets`.
fn reach_all(targets: &[String], timeout: Duration) -> Vec<bool> {
    let deadline = Instant::now() + timeout;
    let (send, receive) = mpsc::channel();
    for (index, target) in targets.iter().enumerate() {
        let send = send.clone();
        let target = target.clone();
        std::thread::spawn(move || {
            let reachable = connects(&target, deadline);
            let _ = send.send(Answer { index, reachable });
        });
    }
    drop(send);
    let mut reached = vec![false; targets.len()];
    for _ in targets {
        let left = deadline.saturating_duration_since(Instant::now());
        match receive.recv_timeout(left) {
            Ok(answer) => reached[answer.index] = answer.reachable,
            Err(_) => break,
        }
    }
    reached
}

/// Whether a TCP connection to `target` completes before `deadline`.
fn connects(target: &str, deadline: Instant) -> bool {
    let Ok(addresses) = target.to_socket_addrs() else {
        return false;
    };
    for address in addresses {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return false;
        }
        if TcpStream::connect_timeout(&address, left).is_ok() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::reach_all;
    use std::net::TcpListener;
    use std::time::Duration;

    #[test]
    fn an_open_port_answers_and_a_closed_one_does_not() {
        let open = TcpListener::bind("127.0.0.1:0").unwrap();
        let closed = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap()
        };
        let targets = [
            open.local_addr().unwrap().to_string(),
            closed.to_string(),
            "not a target".to_owned(),
        ];
        assert_eq!(
            reach_all(&targets, Duration::from_secs(5)),
            [true, false, false]
        );
    }
}
