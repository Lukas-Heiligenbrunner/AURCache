mod builds;
mod cli;
mod compose;
mod config;
mod doctor;
mod dump;
mod output;
mod packages;
mod pacman;
mod repo;
mod setup;
mod setup_cmd;
mod url;
mod workers;

use crate::builds::run_builds_command;
use crate::cli::{
    Cli, Command, ConfigCommand, OutputFormat, RawArgs, RepoCommand, RepoConfigArgs, TokenCommand,
};
use crate::dump::{dump_command, restore_command};
use crate::output::{
    print_graph, print_health, print_json, print_raw_response, print_search_results, print_stats,
    print_user_info, render,
};
use crate::packages::run_packages_command;
use crate::setup_cmd::run_setup_command;
use crate::workers::run_worker_command;
use anyhow::{Context, Result};
use aurcache_client::{AurCacheClient, Method};
use clap::{CommandFactory, Parser};
use config::{
    load_config, resolve_runtime_config, save_config, set_token, set_url, summarize_config,
};
use serde_json::Value;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let format = cli.format;

    match cli.command {
        Command::Config { command } => run_config_command(format, command),
        // Offline, like `config`: the repo address is arithmetic on the
        // configured URL, and a new user reaching for this may not have a token
        // yet. Prompting for one to print three lines of text would be absurd.
        Command::Repo {
            command: RepoCommand::Config(args),
        } => run_repo_config(format, cli.url, cli.token, args).await,
        Command::Completions { shell } => {
            run_completions(shell);
            Ok(())
        }
        // Also offline: the whole point is standing up an instance that does
        // not exist yet, so there is nothing to authenticate against.
        Command::Setup { command } => run_setup_command(format, *command),
        command => {
            let runtime = resolve_runtime_config(cli.url, cli.token)?;
            let used_token = runtime.token.clone();
            let client = AurCacheClient::new(runtime.url.clone(), runtime.token)?;
            match run(&client, format, command.clone()).await {
                Err(err) if aurcache_client::is_unauthorized(&err) && config::is_interactive() => {
                    eprintln!("{err}");
                    warn_if_token_from_env(used_token.as_deref());
                    eprintln!("Please re-enter your AURCache API token to continue.");
                    let token = config::prompt_and_save_token(load_config()?)?;
                    let client = AurCacheClient::new(runtime.url, Some(token))?;
                    run(&client, format, command).await
                }
                result => result,
            }
        }
    }
}

/// Warns the user if the token that was just rejected came from the
/// `AURCACHE_TOKEN` environment variable. Env vars take precedence over the
/// config file (see `resolve_runtime_config`), so saving a freshly prompted
/// token there won't actually fix anything next run unless the stale
/// environment variable is also updated or unset.
fn warn_if_token_from_env(used_token: Option<&str>) {
    if let (Some(used), Ok(env_token)) = (used_token, std::env::var("AURCACHE_TOKEN"))
        && used == env_token
    {
        eprintln!(
            "Warning: that token came from the AURCACHE_TOKEN environment variable. \
             The new token will be saved to the config file, but AURCACHE_TOKEN still \
             takes precedence over it, so this will keep failing until you update or \
             unset that environment variable."
        );
    }
}

async fn run(client: &AurCacheClient, format: OutputFormat, command: Command) -> Result<()> {
    match command {
        Command::Health => run_health(client, format).await,
        Command::Doctor => doctor::run_doctor(client, format).await,
        Command::Repo { .. } | Command::Completions { .. } | Command::Setup { .. } => {
            unreachable!("offline commands are handled before client setup")
        }
        Command::UserInfo => render_user_info(client, format).await,
        Command::Stats => render_stats(client, format).await,
        Command::Graph => render_graph(client, format).await,
        Command::Search { query } => render_search_results(client, format, &query).await,
        Command::Token { command } => run_token_command(client, format, command).await,
        Command::Config { .. } => unreachable!("config commands are handled before client setup"),
        Command::Pkg { command } => run_packages_command(client, format, command).await,
        Command::Dump {
            output,
            include_secrets,
        } => dump_command(client, format, output, include_secrets).await,
        Command::Restore {
            archive,
            dry_run,
            on_existing,
            clear,
            secrets,
        } => {
            restore_command(
                client,
                format,
                &archive,
                dry_run,
                on_existing,
                clear,
                secrets,
            )
            .await
        }
        Command::Builds { command } => run_builds_command(client, format, command).await,
        Command::Worker { command } => run_worker_command(client, format, command).await,
        Command::Raw(args) => run_raw_command(client, args).await,
    }
}

/// Ask the server how it publishes its repository, if we are in a position to.
///
/// The server is the only party that knows whether the repository sits on a
/// non-default port, under a path, or behind a reverse proxy — deriving it from
/// the API URL is a guess that happens to be right for a default deployment.
/// So ask when a token makes that possible, and fall back to the guess when it
/// does not: a first-time user with no token still gets a usable answer, which
/// is the whole reason this command works offline.
async fn published_repo_url(
    api_url: &str,
    cli_token: Option<String>,
    args: &RepoConfigArgs,
) -> Option<String> {
    // An explicit --port is the user overriding us; asking would be pointless.
    if args.offline || args.port.is_some() {
        return None;
    }

    let token = config::stored_token(cli_token).ok().flatten()?;
    let client = AurCacheClient::new(api_url.to_string(), Some(token)).ok()?;
    match client.repo_info().await {
        Ok(info) => Some(info.public_url),
        Err(e) => {
            eprintln!(
                "Warning: could not ask {api_url} how it publishes the repository \
                 ({e:#}); falling back to the default port. Pass --offline to skip \
                 this check."
            );
            None
        }
    }
}

/// Print the `pacman.conf` stanza, resolving the URL without touching the token.
async fn run_repo_config(
    format: OutputFormat,
    cli_url: Option<String>,
    cli_token: Option<String>,
    args: RepoConfigArgs,
) -> Result<()> {
    let api_url = config::resolve_url_only(cli_url)?;
    let published = published_repo_url(&api_url, cli_token, &args).await;
    let config = repo::repo_config(
        &api_url,
        args.port,
        args.name,
        args.siglevel,
        published.as_deref(),
    )?;

    if args.install {
        let installed = repo::install_stanza(&args.pacman_conf, &config)?;
        return match format {
            OutputFormat::Json => print_json(&installed),
            OutputFormat::Text => {
                if installed.changed {
                    println!("added [{}] to {}", config.name, installed.path);
                    println!("  sudo pacman -Sy");
                } else {
                    println!(
                        "[{}] is already configured in {}",
                        config.name, installed.path
                    );
                }
                Ok(())
            }
        };
    }

    match format {
        OutputFormat::Json => print_json(&config),
        OutputFormat::Text => {
            repo::print_repo_config(&config);
            Ok(())
        }
    }
}

fn run_completions(shell: clap_complete::Shell) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
}

fn run_config_command(format: OutputFormat, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => show_config(format),
        ConfigCommand::SetUrl { url } => save_url_config(format, url),
        ConfigCommand::SetToken { token } => save_token_config(format, token),
    }
}

async fn run_health(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    client.health().await?;
    let info = client.server_info().await?;
    render(format, &info, print_health)
}

async fn render_user_info(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let user = client.user_info().await?;
    render(format, &user, print_user_info)
}

async fn render_stats(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let stats = client.stats().await?;
    render(format, &stats, print_stats)
}

async fn render_graph(client: &AurCacheClient, format: OutputFormat) -> Result<()> {
    let points = client.graph().await?;
    render(format, &points, |points| print_graph(points))
}

async fn render_search_results(
    client: &AurCacheClient,
    format: OutputFormat,
    query: &str,
) -> Result<()> {
    let results = client.search(query).await?;
    render(format, &results, |results| print_search_results(results))
}

async fn run_token_command(
    client: &AurCacheClient,
    format: OutputFormat,
    command: TokenCommand,
) -> Result<()> {
    match command {
        TokenCommand::Regenerate => {
            let response = client.regenerate_api_token().await?;

            // Persist the new token to the config file so subsequent CLI
            // invocations keep working without requiring the user to run
            // `config set-token` themselves. If AURCACHE_TOKEN is set, it
            // still takes precedence at runtime, so warn about that too.
            let config = set_token(load_config()?, Some(response.token.clone()))?;
            save_config(&config)?;
            if std::env::var("AURCACHE_TOKEN").is_ok() {
                eprintln!(
                    "Note: the new token was saved to the config file, but the \
                     AURCACHE_TOKEN environment variable is set and will keep \
                     taking precedence over it. Update or unset that environment \
                     variable to actually use the new token."
                );
            }

            match format {
                OutputFormat::Json => print_json(&response),
                OutputFormat::Text => {
                    println!("{}", response.token);
                    Ok(())
                }
            }
        }
    }
}

async fn run_raw_command(client: &AurCacheClient, args: RawArgs) -> Result<()> {
    let method = Method::from_bytes(args.method.as_bytes())
        .with_context(|| format!("unsupported HTTP method: {}", args.method))?;
    let body = args.body.as_deref().map(parse_json_body).transpose()?;
    let text = client
        .request_text(method, &args.path, &args.query, body.as_ref())
        .await?;
    if text.trim().is_empty() {
        return Ok(());
    }
    print_raw_response(&text)
}

fn show_config(format: OutputFormat) -> Result<()> {
    let summary = summarize_config(&load_config()?)?;
    match format {
        OutputFormat::Json => print_json(&summary),
        OutputFormat::Text => {
            println!("path: {}", summary.path);
            println!("url: {}", summary.url.unwrap_or_else(|| "-".to_string()));
            println!("token: {}", summary.token_state);
            Ok(())
        }
    }
}

fn save_url_config(format: OutputFormat, url: Option<String>) -> Result<()> {
    let config = set_url(load_config()?, url)?;
    save_config(&config)?;
    match format {
        OutputFormat::Json => print_json(&summarize_config(&config)?),
        OutputFormat::Text => {
            println!("saved url to config");
            Ok(())
        }
    }
}

fn save_token_config(format: OutputFormat, token: Option<String>) -> Result<()> {
    let config = set_token(load_config()?, token)?;
    save_config(&config)?;
    match format {
        OutputFormat::Json => print_json(&summarize_config(&config)?),
        OutputFormat::Text => {
            println!("saved token to config");
            Ok(())
        }
    }
}

fn parse_json_body(body: &str) -> Result<Value> {
    serde_json::from_str(body).context("invalid JSON passed to --body")
}
