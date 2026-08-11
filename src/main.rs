mod album;
mod apple_maker_note;
mod classify;
mod db_cmd;
mod dedup;
#[cfg(test)]
mod docs_generator;
mod exif_util;
mod file_type;
mod fs;
mod info_cmd;
mod inspect;
mod live_photo;
mod markdown;
mod media;
mod progress;
mod quicktime_meta;
mod s3_fs;
mod s3_uri;
mod supplemental_info;
mod sync_cmd;
mod test_util;
mod track_util;
mod util;

use crate::util::OutputTZ;
use clap::{Args, Parser, Subcommand};
use std::io::IsTerminal;
use tracing::{Level, debug, error, info};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// Used wherever the tool labels its own output — generated-file markers, docs.
pub(crate) const COMMAND_NAME: &str = "ptsync";

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Turn debugging information on
    #[arg(short, long)]
    debug: bool,

    /// UTC offset to use when localizing UTC dates - `+12:00`, `-04:00`, `+0545` or `+00:00`
    /// (else `TZ` env var, else the machine's zone)
    // `allow_hyphen_values` because half the world's offsets start with one, and
    // clap would otherwise read `-z -04:00`'s value as another flag.
    #[arg(short = 'z', long, global = true, allow_hyphen_values = true)]
    timezone: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

/// Overrides for `s3://` paths. Each is optional; when omitted, the standard AWS
/// resolution applies (env vars, `~/.aws`, SSO, IMDS). Credentials are never
/// passed as flags - they come from that chain.
#[derive(Args, Clone)]
struct S3Opts {
    /// AWS region for `s3://` paths (else `AWS_REGION` / the profile's region)
    #[arg(long)]
    s3_region: Option<String>,

    /// Custom S3 endpoint URL for S3-compatible stores like MinIO; enables
    /// path-style addressing
    #[arg(long)]
    s3_endpoint_url: Option<String>,

    /// AWS profile name for `s3://` paths (else `AWS_PROFILE` / `default`)
    #[arg(long)]
    s3_profile: Option<String>,
}

impl S3Opts {
    fn to_config(&self) -> s3_fs::S3Config {
        s3_fs::S3Config {
            region: self.s3_region.clone(),
            endpoint_url: self.s3_endpoint_url.clone(),
            profile: self.s3_profile.clone(),
        }
    }
}

#[derive(Subcommand)]
enum Commands {
    /// Show info for an individual photo or video
    Info {
        /// The takeout or iCloud zip/directory
        #[arg(short, long)]
        root: String,

        /// Photo, video or album to view info for
        #[arg(short, long)]
        input: String,
    },
    /// Scan files in an archive or directory and collect meta info into a sqlite database
    Db {
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

        /// Skip inspecting photo and video files; leaves the media_item tables
        /// untouched, so only the classified tables are written
        #[arg(long)]
        skip_media: bool,

        /// Skip inspecting albums
        #[arg(long)]
        skip_albums: bool,

        #[command(flatten)]
        s3: S3Opts,
    },
    /// Sync files in an archive or directory into a standardised directory structure
    Sync {
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

        #[command(flatten)]
        s3: S3Opts,
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
    enable_debug(cli.debug);
    let tz = OutputTZ::resolve(cli.timezone.as_deref())?;
    match cli.command {
        Commands::Info { root, input } => info_cmd::main(&input, &root, tz)?,
        Commands::Db {
            input,
            output,
            clear,
            skip_media,
            skip_albums,
            s3,
        } => {
            s3_fs::set_s3_config(s3.to_config());
            db_cmd::main(
                &input,
                &output,
                db_cmd::DbScanOpts {
                    clear,
                    skip_media,
                    skip_albums,
                },
                tz,
            )?
        }
        Commands::Sync {
            dry_run,
            skip_markdown,
            input,
            output,
            skip_media,
            skip_albums,
            s3,
        } => {
            enable_dry_run(dry_run);
            s3_fs::set_s3_config(s3.to_config());
            sync_cmd::main(
                dry_run,
                &input,
                &output,
                skip_markdown,
                skip_media,
                skip_albums,
                tz,
            )?;
        }
    }
    Ok(())
}

fn enable_debug(debug: bool) {
    // `--debug` traces ptsync's own logic, so the noisy dependencies stay pinned
    // at ERROR rather than following it up.
    let filter = tracing_subscriber::filter::Targets::new()
        .with_default(Level::INFO)
        .with_target("ptsync", if debug { Level::DEBUG } else { Level::INFO })
        .with_target("nom_exif", Level::ERROR)
        .with_target("turso_core", Level::ERROR)
        .with_target("turso_sdk_kit", Level::ERROR)
        .with_target("turso_sync_engine", Level::ERROR)
        .with_target("aws_config", Level::ERROR)
        .with_target("aws_sdk_s3", Level::ERROR);
    let registry_layer = tracing_subscriber::fmt::layer()
        .with_writer(progress::IndicatifWriter)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false);

    // A normal run should read like rsync's output: just the message. A timestamp
    // per line only earns its place when reading back a `--debug` trace.
    let registry_layer = if debug {
        registry_layer.boxed()
    } else {
        registry_layer.without_time().boxed()
    };

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
