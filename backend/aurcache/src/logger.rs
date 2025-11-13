use tracing::level_filters::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

pub fn init_logger() {
    let env_name = "LOG_LEVEL";
    let default_level = LevelFilter::INFO;

    let env_filter = EnvFilter::builder()
        .with_default_directive(default_level.into())
        .with_env_var(env_name)
        .from_env_lossy();

    let env_filter = env_filter
        .add_directive("rocket=warn".parse().unwrap())
        .add_directive("hyper::proto=warn".parse().unwrap());

    let use_color = std::env::var("LOG_STYLE")
        .map(|s| s != "never")
        .unwrap_or(true);

    #[cfg(debug_assertions)]
    let formatter = fmt::layer().with_target(true).with_ansi(use_color).pretty();

    // print the compact version when in release mode
    #[cfg(not(debug_assertions))]
    let formatter = fmt::layer()
        .with_target(true)
        .with_ansi(use_color)
        .compact();

    tracing_subscriber::registry()
        .with(env_filter)
        .with(formatter)
        .init();
}
