use bollard::Docker;

pub async fn healthy() -> anyhow::Result<()> {
    // check docker socket connection
    let docker = Docker::connect_with_unix_defaults()?;
    docker.ping().await.map(|_| ()).map_err(Into::into)
}
