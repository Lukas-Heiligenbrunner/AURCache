//! The SSH link over the engine's exec API, which needs no path shared with
//! the engine. See "The SSH link" in `design/in-progress/container-workers.md`.

use crate::container;
use anyhow::Context;
use aurcache_build_agent::layout::Scratch;
use aurcache_worker_container::ssh::agent::Agent;
use aurcache_worker_container::ssh::link;
use bollard::Docker;
use bollard::container::LogOutput;
use bollard::exec::{CreateExecOptions, StartExecOptions, StartExecResults};
use bytes::Bytes;
use futures::StreamExt;
use futures::stream::BoxStream;
use std::pin::Pin;
use tokio::io::AsyncWrite;
use tokio_util::io::StreamReader;

/// Exec `ssh-link` into the container and serve the link to its end.
pub async fn serve(docker: &Docker, container_id: &str, agent: &Agent) -> anyhow::Result<()> {
    let Opened { output, input } = open(docker, container_id).await?;
    Ok(link::serve(agent.socket(), output, input).await?)
}

/// The exec's two ends: what `ssh-link` writes, and its stdin.
struct Opened {
    output: StreamReader<BoxStream<'static, std::io::Result<Bytes>>, Bytes>,
    input: Pin<Box<dyn AsyncWrite + Send>>,
}

async fn open(docker: &Docker, container_id: &str) -> anyhow::Result<Opened> {
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
        .context("the exec of ssh-link")?;
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
        .context("the exec of ssh-link")?;
    let StartExecResults::Attached { output, input } = started else {
        anyhow::bail!("the exec of ssh-link started detached");
    };
    // stdout carries the link; what ssh-link says on stderr goes to the
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
