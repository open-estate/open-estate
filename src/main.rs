// Open Estate Pure-Rust Server & Database Migration CLI (`open-estate-server`).

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use open_estate::db::{migrate_database_urls, DbStore};
use open_estate::modules::school_metrics::{ingest as ingest_school_metrics, IngestOptions};
use open_estate::shared::{EnvConfig, HttpClient};
use open_estate::web::server::serve_http;
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "open-estate-server",
    version,
    about = "Open Estate US Residential & Commercial Property Intelligence, AI Home Finder & Due-Diligence Server"
)]
struct Cli {
    #[command(flatten)]
    serve: ServeArgs,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Args, Debug, Clone)]
struct ServeArgs {
    /// Bind host address
    #[arg(long, env = "HOST", default_value = "0.0.0.0", global = true)]
    host: String,

    /// Bind TCP port
    #[arg(long, env = "PORT", default_value_t = 8080, global = true)]
    port: u16,

    /// Override SQLite WAL database file path (`SQLITE_DB_PATH`)
    #[arg(long = "db-file", env = "SQLITE_DB_PATH", global = true)]
    db_file: Option<String>,

    /// Background area ZIP sync interval in minutes (`0` disables background polling)
    #[arg(long = "sync-interval-min", env = "SYNC_INTERVAL_MIN", global = true)]
    sync_interval_min: Option<u64>,

    /// Local reports directory for pre-seeded reports
    #[arg(long = "reports-dir", env = "REPORTS_DIR", default_value = "reports", global = true)]
    reports_dir: String,

    /// Local Zillow / public datasets cache directory
    #[arg(long = "data-dir", env = "DATA_DIR", default_value = ".cache/datasets", global = true)]
    data_dir: String,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start the Open Estate Axum HTTP server
    Serve,

    /// Live idempotent database migration (`SQLite WAL <-> OCI MySQL HeatWave`)
    Migrate {
        /// Source database URL or SQLite file path
        #[arg(long)]
        from: String,

        /// Destination database URL or SQLite file path
        #[arg(long)]
        to: String,

        /// Streaming batch size per table chunk
        #[arg(long = "batch-size", default_value_t = 200)]
        batch_size: usize,
    },

    /// Public school data utilities (CAASPP / CDE Directory ingestion)
    Schools {
        #[command(subcommand)]
        command: SchoolsCommands,
    },
}

#[derive(Subcommand, Debug)]
enum SchoolsCommands {
    /// Download (or read local) official CAASPP & CDE files and populate `school_metrics`
    Ingest {
        /// State two-letter code (currently only `CA` is supported)
        #[arg(long, default_value = "CA")]
        state: String,

        /// Spring test year (e.g. `2026` for the 2025-26 academic year)
        #[arg(long, default_value_t = 2026)]
        year: i64,

        /// Local CAASPP ZIP or caret-delimited file path (skips network download)
        #[arg(long)]
        file: Option<PathBuf>,

        /// Local CDE public schools directory (`pubschls.txt`) path
        #[arg(long = "directory-file")]
        directory_file: Option<PathBuf>,

        /// Skip CDE School Dashboard CDS/NCES crosswalk query fallback
        #[arg(long = "no-dashboard", default_value_t = false)]
        no_dashboard: bool,
    },
}

fn build_runtime_env(args: &ServeArgs) -> EnvConfig {
    let cwd = std::env::current_dir().unwrap_or_else(|_| Path::new(".").to_path_buf());
    let base = EnvConfig::from_process_and_dotenv(Some(&cwd));
    let mut overrides: Vec<(String, String)> = vec![
        ("HOST".to_string(), args.host.clone()),
        ("PORT".to_string(), args.port.to_string()),
        ("REPORTS_DIR".to_string(), args.reports_dir.clone()),
        ("DATA_DIR".to_string(), args.data_dir.clone()),
    ];
    if let Some(ref db_file) = args.db_file {
        if !db_file.trim().is_empty() {
            overrides.push(("SQLITE_DB_PATH".to_string(), db_file.clone()));
            overrides.push(("SQLITE_PATH".to_string(), db_file.clone()));
        }
    }
    if let Some(sync_min) = args.sync_interval_min {
        overrides.push(("SYNC_INTERVAL_MIN".to_string(), sync_min.to_string()));
    }
    base.with_overrides(overrides)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();

    let cli = Cli::parse();

    match cli.command {
        Some(Commands::Migrate {
            from,
            to,
            batch_size,
        }) => {
            let summary = migrate_database_urls(&from, &to, batch_size).await?;
            println!("{}", serde_json::to_string_pretty(&summary)?);
            Ok(())
        }
        Some(Commands::Schools {
            command:
                SchoolsCommands::Ingest {
                    state,
                    year,
                    file,
                    directory_file,
                    no_dashboard,
                },
        }) => {
            let env = open_estate::proxy_pool::apply_runtime_defaults(&build_runtime_env(&cli.serve));
            let db = DbStore::connect(&env).await?;
            db.ensure_schema().await?;
            let http = HttpClient::from_env(&env, None);
            let opts = IngestOptions {
                state,
                year,
                file,
                directory_file,
                use_dashboard: !no_dashboard,
            };
            let stats = ingest_school_metrics(&db, &http, &opts).await?;
            println!("{}", serde_json::to_string_pretty(&stats)?);
            Ok(())
        }
        Some(Commands::Serve) | None => {
            // The server routes public-data scraping through the self-updating free proxy pool
            // by default (strict: never direct) unless OUTBOUND_PROXY_URLS or
            // FREE_PROXY_POOL_ENABLED=false says otherwise.
            let env = open_estate::proxy_pool::apply_runtime_defaults(&build_runtime_env(&cli.serve));
            serve_http(&cli.serve.host, cli.serve.port, env).await
        }
    }
}
