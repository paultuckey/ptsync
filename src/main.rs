mod album;
#[cfg(test)]
mod boundary_tests;
mod classify;
mod db_cmd;
mod dedup;
mod exif_util;
mod file_type;
mod fs;
mod info_cmd;
mod inspect;
mod markdown;
mod media;
mod progress;
mod supplemental_info;
mod sync_cmd;
mod test_util;
mod track_util;
mod util;

use clap::{Parser, Subcommand};
use tracing::{Level, debug, error, info};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The binary / command name, used wherever the tool labels its own output
/// (generated-file markers, docs, etc.).
pub(crate) const COMMAND_NAME: &str = "ptsync";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show info for an individual photo or video
    Info {
        /// Turn debugging information on
        #[arg(short, long)]
        debug: bool,

        /// The takeout or iCloud zip/directory
        #[arg(short, long)]
        root: String,

        /// Photo, video or album to view info for
        #[arg(short, long)]
        input: String,
    },
    /// Scan files in an archive or directory and collect meta info into a sqlite database
    Db {
        /// Turn debugging information on
        #[arg(short, long)]
        debug: bool,

        /// The takeout or iCloud zip/directory
        #[arg(short, long)]
        input: String,

        /// Path to the sqlite database file to write
        #[arg(short, long, default_value = "db.sqlite")]
        output: String,

        /// Clear existing rows before scanning; also rebuilds the database if
        /// its schema is out of date
        #[arg(long, action = clap::ArgAction::Set, default_value_t = false)]
        clear: bool,
    },
    /// Sync files in an archive or directory into a standardised directory structure
    Sync {
        /// Turn debugging information on
        #[arg(short, long)]
        debug: bool,

        /// If set, don't do anything, just print what would be done.
        #[arg(short = 'n', long)]
        dry_run: bool,

        /// Google Takeout or iCloud input directory or zip file
        #[arg(long)]
        input: String,

        /// Directory to sync photos and videos into
        #[arg(short, long)]
        output: Option<String>,

        /// Skip generating markdown files
        #[arg(long)]
        skip_markdown: bool,

        /// Skip inspecting and copying photo and video files
        #[arg(long)]
        skip_media: bool,

        /// Skip inspecting and copying albums
        #[arg(long)]
        skip_albums: bool,
    },
}

fn main() {
    match go() {
        Ok(_) => {}
        Err(e) => {
            error!("Error: {e}");
            std::process::exit(1);
        }
    }
}

fn go() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Info { debug, root, input } => {
            enable_debug(debug);
            info_cmd::main(&input, &root)?
        }
        Commands::Db {
            debug,
            input,
            output,
            clear,
        } => {
            enable_debug(debug);
            db_cmd::main(&input, &output, clear)?
        }
        Commands::Sync {
            debug,
            dry_run,
            skip_markdown,
            input,
            output,
            skip_media,
            skip_albums,
        } => {
            enable_debug(debug);
            enable_dry_run(dry_run);
            sync_cmd::main(
                dry_run,
                &input,
                &output,
                skip_markdown,
                skip_media,
                skip_albums,
            )?;
        }
    }
    Ok(())
}

fn enable_debug(debug: bool) {
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(if debug { Level::DEBUG } else { Level::INFO })
        .with_target("nom_exif", Level::ERROR)
        .with_target("turso_core", Level::WARN)
        .with_target("turso_sdk_kit", Level::WARN)
        .with_target("turso_sync_engine", Level::WARN);
    let registry_layer = tracing_subscriber::fmt::layer()
        .with_writer(progress::IndicatifWriter)
        .with_target(false);
    tracing_subscriber::registry()
        .with(registry_layer)
        .with(filter)
        .init();

    if debug {
        debug!("Debug mode is on");
    }
}

fn enable_dry_run(dry_run: bool) {
    if dry_run {
        info!("Dry run mode is on, no changes will be made to disk");
    }
}

/// Generates `docs/cli.md` from the CLI's own `--help` output and verifies the
/// committed copy is current.
///
/// Run `UPDATE_DOCS=1 cargo test` to regenerate the doc after changing any
/// argument; a plain `cargo test` (locally and in CI) fails if it is stale.
#[cfg(test)]
mod cli_docs {
    use super::Cli;
    use clap::CommandFactory;

    const DOC_PATH: &str = "docs/cli.md";
    const BIN: &str = super::COMMAND_NAME;
    /// Fixed width for the doc so it renders identically regardless of the
    /// terminal `cargo test` happens to run in.
    const DOC_WIDTH: usize = 100;

    /// Render the exact text `<args>` would print, by driving clap the same way
    /// the real binary does and capturing the resulting help "error".
    fn render_help(args: &[&str]) -> anyhow::Result<String> {
        match Cli::command()
            .color(clap::ColorChoice::Never)
            .term_width(DOC_WIDTH)
            .try_get_matches_from(args.iter().copied())
        {
            Ok(_) => anyhow::bail!("--help should produce a DisplayHelp error"),
            Err(err) => Ok(err.render().to_string()),
        }
    }

    fn generate() -> anyhow::Result<String> {
        let mut out = String::new();
        out.push_str("# CLI reference\n\n");
        out.push_str(&format!(
            "<!-- Generated by the `cli_docs` test from `{BIN} --help`. -->\n"
        ));
        out.push_str(
            "<!-- Do not edit by hand. Run `UPDATE_DOCS=1 cargo test` to regenerate. -->\n\n",
        );

        out.push_str(&format!("## {BIN}\n\n```\n"));
        out.push_str(render_help(&[BIN, "--help"])?.trim_end());
        out.push_str("\n```\n");

        // Discover subcommands from the command model so new commands are
        // documented automatically. Skip clap's built-in `help` command.
        let subcommands: Vec<String> = Cli::command()
            .get_subcommands()
            .map(|s| s.get_name().to_string())
            .filter(|name| name != "help")
            .collect();

        for sub in subcommands {
            out.push_str(&format!("\n## {BIN} {sub}\n\n```\n"));
            out.push_str(render_help(&[BIN, &sub, "--help"])?.trim_end());
            out.push_str("\n```\n");
        }
        Ok(out)
    }

    #[test]
    fn cli_docs_up_to_date() -> anyhow::Result<()> {
        let generated = generate()?;

        if std::env::var_os("UPDATE_DOCS").is_some() {
            if let Some(dir) = std::path::Path::new(DOC_PATH).parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(DOC_PATH, &generated)?;
            return Ok(());
        }

        let existing = std::fs::read_to_string(DOC_PATH).unwrap_or_default();
        assert_eq!(
            existing, generated,
            "{DOC_PATH} is out of date. Regenerate with:\n\n    UPDATE_DOCS=1 cargo test\n"
        );
        Ok(())
    }
}
