//! Stopping a worker when its process is told to: SIGTERM from whatever runs
//! it, SIGINT from a terminal.

use crate::executor::Executor;
use crate::runner::Runner;
use anyhow::{Context, Result};
use std::sync::Arc;
use std::time::Duration;
use tokio::signal::unix::{Signal, SignalKind, signal};

/// The signals that ask a worker to stop.
///
/// From the moment this exists the process no longer dies of them by itself,
/// so whoever listens also has to act on [`received`](Self::received).
pub struct StopSignals {
    terminate: Signal,
    interrupt: Signal,
}

impl StopSignals {
    pub fn listen() -> Result<Self> {
        Ok(Self {
            terminate: signal(SignalKind::terminate()).context("listening for SIGTERM")?,
            interrupt: signal(SignalKind::interrupt()).context("listening for SIGINT")?,
        })
    }

    /// The name of the next of them to arrive.
    pub async fn received(&mut self) -> &'static str {
        tokio::select! {
            _ = self.terminate.recv() => "SIGTERM",
            _ = self.interrupt.recv() => "SIGINT",
        }
    }
}

/// How a worker that was told to stop wound down.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WoundDown {
    /// Every build it held was stopped and reported.
    Reported,
    /// Some build was not reported in the time allowed; the server ends it
    /// when it stops hearing of it.
    OutOfTime,
    /// A second signal arrived: whoever sent it does not want to wait.
    Hurried,
}

/// Run `runner` until a stop signal arrives, then wind it down: it claims
/// nothing more, stops the builds it holds and reports each as interrupted,
/// for at most `within`.
pub async fn run_until_told_to_stop<E: Executor>(
    runner: Arc<Runner<E>>,
    signals: &mut StopSignals,
    within: Duration,
) -> Result<WoundDown> {
    let running = Arc::clone(&runner).run();
    tokio::pin!(running);
    let signal = tokio::select! {
        ended = &mut running => return ended.map(|()| WoundDown::Reported),
        signal = signals.received() => signal,
    };
    tracing::info!(
        "Received {signal}: claiming no more builds and stopping the ones running; the \
         server queues a new build of each"
    );
    runner.stop();
    tokio::select! {
        ended = tokio::time::timeout(within, running) => match ended {
            Ok(ended) => ended.map(|()| WoundDown::Reported),
            Err(_) => {
                tracing::warn!(
                    "Not every build was stopped and reported within {}s; the server ends \
                     the rest when it stops hearing of them",
                    within.as_secs()
                );
                Ok(WoundDown::OutOfTime)
            }
        },
        signal = signals.received() => {
            tracing::warn!("Received {signal} again: stopping without waiting for the builds");
            Ok(WoundDown::Hurried)
        }
    }
}
