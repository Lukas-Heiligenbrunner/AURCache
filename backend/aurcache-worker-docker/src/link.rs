//! The SSH link into a build container: `ssh-link` exec'd once, and the
//! build's ssh-agent answering over it until the build agent closes it.
//!
//! It goes over the engine's exec API, so it needs no path the engine and
//! the worker share, and works the same against a remote daemon, the hybrid
//! image's own Podman, or the host's Docker.

use crate::container;
use aurcache_build_agent::layout::Scratch;
use aurcache_worker_container::ssh::agent::Agent;
use aurcache_worker_container::ssh::link::{self, Served};
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::AsyncWrite;
use tokio_util::io::StreamReader;

/// Exec `ssh-link` into the container and serve the link to its end.
///
/// Called once per build: the build agent takes one link, and has no use
/// for a second. `agent` is the build's ssh-agent, kept for as long as the
/// link is served.
pub async fn serve(docker: Docker, container_id: String, agent: Arc<Agent>) -> Served {
    match open(&docker, &container_id).await {
        Ok(Opened { output, input }) => link::serve(agent.socket(), output, input).await,
        Err(message) => Served::lost(message),
    }
}

/// The exec's two ends: what `ssh-link` writes, and its stdin.
struct Opened {
    output: StreamReader<BoxStream<'static, std::io::Result<Bytes>>, Bytes>,
    input: Pin<Box<dyn AsyncWrite + Send>>,
}

async fn open(docker: &Docker, container_id: &str) -> Result<Opened, String> {
    let agent = container::agent().display().to_string();
    let socket = Scratch::new(container::SCRATCH)
        .ssh_link()
        .display()
        .to_string();
    let created = docker
        .create_exec(
            container_id,
            CreateExecOptions {
                attach_stdin: Some(true),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                cmd: Some(vec![agent, "ssh-link".into(), "--socket".into(), socket]),
                ..CreateExecOptions::default()
            },
        )
        .await
        .map_err(|e| format!("the exec of ssh-link failed: {e}"))?;
    let started = docker
        .start_exec(
            &created.id,
            Some(StartExecOptions {
                detach: false,
                tty: false,
                output_capacity: None,
            }),
        )
        .await
        .map_err(|e| format!("the exec of ssh-link failed: {e}"))?;
    let StartExecResults::Attached { output, input } = started else {
        return Err("the exec of ssh-link started detached".to_string());
    };
    // stdout carries the frames; what ssh-link says on stderr goes to the
    // worker's own log.
    let stdout = output
        .filter_map(|chunk| async move {
            match chunk {
                Ok(LogOutput::StdOut { message }) => Some(Ok(message)),
                Ok(LogOutput::StdErr { message }) => {
                    tracing::debug!("ssh-link: {}", String::from_utf8_lossy(&message).trim());
                    None
                }
                Ok(_) => None,
                Err(e) => Some(Err(std::io::Error::other(e))),
            }
        })
        .boxed();
    Ok(Opened {
        output: StreamReader::new(stdout),
        input,
    })
}
