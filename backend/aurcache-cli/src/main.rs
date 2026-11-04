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
    ApiCommand, Cli, Command, ConfigCommand, OutputFormat, RawArgs, RepoCommand, RepoConfigArgs,
    TokenCommand,
};
use crate::dump::{dump_command, restore_command};
use crate::output::{
    option_text, print_graph, print_health, print_raw_response, print_search_results, print_stats,
    print_user_info, render,
};
use crate::packages::run_packages_command;
use crate::setup_cmd::run_setup_command;
use crate::workers::run_worker_command;
use anyhow::{Context, Result};
use aurcache_client::{AurCacheClient, Method};
use clap::{CommandFactory, Parser};
use config::{
    ClientConfig, load_config, resolve_runtime_config, save_config, set_token, set_url,
    summarize_config,
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
        Command::Api(command) => {
            let runtime = resolve_runtime_config(cli.url, cli.token)?;
            let client = AurCacheClient::new(runtime.url.clone(), runtime.token)?;
            match run(&client, format, command.clone()).await {
                Err(err) if aurcache_client::is_unauthorized(&err) && config::is_interactive() => {
                    eprintln!("{err}");
                    eprintln!("Please re-enter your AURCache API token to continue.");
                    let token = config::prompt_and_save_token(load_config()?)?;
                    warn_if_env_token_wins();
                    let client = AurCacheClient::new(runtime.url, token)?;
                    run(&client, format, command).await
                }
                result => result,
            }
        }
    }
}

/// Warn, after a token was saved to the config file, that it will not be used
/// while `AURCACHE_TOKEN` is set: the environment takes precedence over the
/// file (see `resolve_runtime_config`).
fn warn_if_env_token_wins() {
    if std::env::var_os("AURCACHE_TOKEN").is_some() {
        eprintln!(
            "Note: the new token was saved to the config file, but the \
             AURCACHE_TOKEN environment variable is set and takes precedence \
             over it. Update or unset that environment variable to use the new \
             token."
        );
    }
}

async fn run(client: &AurCacheClient, format: OutputFormat, command: ApiCommand) -> Result<()> {
    match command {
        ApiCommand::Health => run_health(client, format).await,
        ApiCommand::Doctor => doctor::run_doctor(client, format).await,
        ApiCommand::UserInfo => render_user_info(client, format).await,
        ApiCommand::Stats => render_stats(client, format).await,
        ApiCommand::Graph => render_graph(client, format).await,
        ApiCommand::Search { query } => render_search_results(client, format, &query).await,
        ApiCommand::Token { command } => run_token_command(client, format, command).await,
        ApiCommand::Pkg { command } => run_packages_command(client, format, command).await,
        ApiCommand::Dump {
            output,
            include_secrets,
        } => dump_command(client, format, output, include_secrets).await,
        ApiCommand::Restore(args) => restore_command(client, format, args).await,
        ApiCommand::Builds { command } => run_builds_command(client, format, command).await,
        ApiCommand::Worker { command } => run_worker_command(client, format, command).await,
        ApiCommand::Raw(args) => run_raw_command(client, args).await,
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

    if !args.install {
        return render(format, &config, repo::print_repo_config);
    }
    let installed = repo::Installed {
        path: &args.pacman_conf,
        outcome: repo::install_stanza(&args.pacman_conf, &config)?,
    };
    render(format, &installed, |installed| {
        let path = installed.path.display();
        match installed.outcome {
            repo::Install::AlreadyPresent => {
                println!("[{}] is already configured in {path}", config.name);
            }
            repo::Install::Appended | repo::Install::AppendedWithSudo => {
                println!("added [{}] to {path}", config.name);
                println!("  sudo pacman -Sy");
            }
        }
    })
}

fn run_completions(shell: clap_complete::Shell) {
    let mut command = Cli::command();
    let name = command.get_name().to_string();
    clap_complete::generate(shell, &mut command, name, &mut std::io::stdout());
}

fn run_config_command(format: OutputFormat, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::Show => show_config(format),
        ConfigCommand::SetUrl { url } => save_setting(format, set_url(load_config()?, url)?, "url"),
        ConfigCommand::SetToken { token } => {
            save_setting(format, set_token(load_config()?, token)?, "token")
        }
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
            // `config set-token` themselves.
            let config = set_token(load_config()?, Some(response.token.clone()))?;
            save_config(&config)?;
            warn_if_env_token_wins();
            render(format, &response, |response| println!("{}", response.token))
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
    render(format, &summary, |summary| {
        println!("path: {}", summary.path);
        println!("url: {}", option_text(summary.url.as_deref()));
        println!("token: {}", summary.token_state);
    })
}

/// Save `config`, after `name` was changed in it.
fn save_setting(format: OutputFormat, config: ClientConfig, name: &str) -> Result<()> {
    save_config(&config)?;
    render(format, &summarize_config(&config)?, |_| {
        println!("saved {name} to config");
    })
}

fn parse_json_body(body: &str) -> Result<Value> {
    serde_json::from_str(body).context("invalid JSON passed to --body")
}
