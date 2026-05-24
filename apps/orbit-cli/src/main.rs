//! orbit-cli — talks to orbit-server via gRPC.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use anyhow::Result;
use clap::{Parser, Subcommand};
use futures::StreamExt;
use indicatif::{ProgressBar, ProgressStyle};
use orbit_proto::etl::v1 as pb;
use orbit_proto::etl::v1::etl_service_client::EtlServiceClient;
use std::path::PathBuf;
use std::time::Duration;
use tracing_subscriber::{prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(name = "orbit", version, about = "orbit CLI")]
struct Args {
    /// gRPC server endpoint.
    #[arg(long, env = "ORBIT_ENDPOINT", default_value = "http://127.0.0.1:9876", global = true)]
    endpoint: String,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// ETL operations.
    #[command(subcommand)]
    Etl(EtlCmd),
    /// Geospatial / raster operations (offline; no gRPC).
    #[command(subcommand)]
    Geo(GeoCmd),
}

#[derive(Subcommand, Debug)]
enum GeoCmd {
    /// Rasterize a vector file into a GeoTIFF.
    Rasterize {
        #[arg(long)]
        vector: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        width: usize,
        #[arg(long)]
        height: usize,
        /// "min_x min_y max_x max_y"
        #[arg(long, num_args = 4, value_names = ["MIN_X", "MIN_Y", "MAX_X", "MAX_Y"])]
        bbox: Vec<f64>,
        #[arg(long, default_value_t = 1.0)]
        burn_value: f64,
        #[arg(long, default_value_t = 0.0)]
        no_data: f64,
    },
    /// Mosaic multiple GeoTIFFs into one.
    Mosaic {
        #[arg(long, required = true, num_args = 1..)]
        inputs: Vec<PathBuf>,
        #[arg(long)]
        output: PathBuf,
    },
    /// Sample one pixel value at given geo coordinates.
    Sample {
        /// Input GeoTIFF.
        #[arg(long)]
        raster: PathBuf,
        /// Geographic X coordinate (e.g. longitude).
        #[arg(long)]
        x: f64,
        /// Geographic Y coordinate (e.g. latitude).
        #[arg(long)]
        y: f64,
    },
    /// Reproject a raster to a target EPSG via gdalwarp.
    Warp {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        output: PathBuf,
        #[arg(long)]
        target_epsg: u32,
    },
    /// Print VSI paths for asset hrefs (e.g. piped from a STAC search).
    /// Each line of stdin is one asset URL; output is the VSI-rewritten path.
    GetImagery {
        /// Optionally take URLs from a file instead of stdin.
        #[arg(long)]
        urls: Option<PathBuf>,
    },
}

#[derive(Subcommand, Debug)]
enum EtlCmd {
    /// Run a pipeline.
    Run {
        /// Input file path.
        #[arg(long)]
        input: PathBuf,
        /// Destination SQLite table.
        #[arg(long)]
        table: String,
        /// File format. Inferred from extension if omitted.
        #[arg(long, value_enum)]
        format: Option<FmtArg>,
        /// Optional Polars SQL transform; use `input` as the source table name.
        #[arg(long)]
        sql: Option<String>,
        /// Dedupe by column (UNIQUE + ON CONFLICT DO NOTHING).
        #[arg(long)]
        dedupe: Option<String>,
        /// Batch size (default 1024).
        #[arg(long, default_value_t = 1024)]
        batch: u32,
        /// Disable CSV header row.
        #[arg(long)]
        no_header: bool,
    },
    /// Show the status of a single job.
    Status { id: String },
    /// List recent jobs.
    List {
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
    /// Cancel a running job.
    Cancel { id: String },
}

#[derive(Copy, Clone, Debug, clap::ValueEnum)]
enum FmtArg { Csv, Parquet, Json }

impl FmtArg {
    fn to_pb(self) -> pb::FileFormat {
        match self {
            Self::Csv     => pb::FileFormat::Csv,
            Self::Parquet => pb::FileFormat::Parquet,
            Self::Json    => pb::FileFormat::Json,
        }
    }
    fn from_path(p: &std::path::Path) -> Option<Self> {
        match p.extension().and_then(|e| e.to_str())?.to_ascii_lowercase().as_str() {
            "csv" => Some(Self::Csv),
            "parquet" | "pq" => Some(Self::Parquet),
            "json" | "ndjson" => Some(Self::Json),
            _ => None,
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing();
    let args = Args::parse();

    // Geo subcommand is fully offline — no gRPC needed.
    if let Cmd::Geo(geo) = args.cmd {
        return handle_geo(geo);
    }

    let mut client = EtlServiceClient::connect(args.endpoint.clone()).await?;

    match args.cmd {
        Cmd::Etl(EtlCmd::Run { input, table, format, sql, dedupe, batch, no_header }) => {
            let fmt = format.or_else(|| FmtArg::from_path(&input))
                .ok_or_else(|| anyhow::anyhow!("cannot infer format from extension; pass --format"))?;
            let spec = pb::PipelineSpec {
                source: Some(pb::pipeline_spec::Source::File(pb::FileSource {
                    path: input.display().to_string(),
                    format: fmt.to_pb() as i32,
                    has_header: !no_header,
                    delimiter: ",".into(),
                })),
                destination_table: table.clone(),
                sql_transform: sql.unwrap_or_default(),
                dedupe_column: dedupe.unwrap_or_default(),
                batch_size: batch,
            };

            let pb_w = ProgressBar::new_spinner();
            pb_w.set_style(ProgressStyle::with_template("{spinner:.green} {msg}")?);
            pb_w.enable_steady_tick(Duration::from_millis(100));

            let mut stream = client.run_pipeline(spec).await?.into_inner();
            while let Some(ev) = stream.next().await {
                let ev = ev?;
                if let Some(inner) = ev.event {
                    match inner {
                        pb::pipeline_event::Event::Started(s) => {
                            pb_w.set_message(format!("job {} started", s.job_id));
                        }
                        pb::pipeline_event::Event::Progress(p) => {
                            pb_w.set_message(format!("read={} written={}", p.rows_read, p.rows_written));
                        }
                        pb::pipeline_event::Event::Completed(c) => {
                            pb_w.finish_with_message(format!(
                                "✓ completed read={} written={} ({})",
                                c.rows_read, c.rows_written, c.job_id
                            ));
                        }
                        pb::pipeline_event::Event::Failed(f) => {
                            pb_w.abandon_with_message(format!("✗ failed: {}", f.error));
                            anyhow::bail!("pipeline failed: {}", f.error);
                        }
                    }
                }
            }
        }
        Cmd::Etl(EtlCmd::Status { id }) => {
            let s = client.get_status(pb::JobId { id: id.clone() }).await?.into_inner();
            print_status(&s);
        }
        Cmd::Etl(EtlCmd::List { limit }) => {
            let list = client.list_jobs(pb::ListJobsRequest { limit }).await?.into_inner();
            if list.jobs.is_empty() {
                println!("no jobs.");
            } else {
                for s in &list.jobs { print_status(s); println!(); }
            }
        }
        Cmd::Etl(EtlCmd::Cancel { id }) => {
            let r = client.cancel_job(pb::JobId { id: id.clone() }).await?.into_inner();
            println!("cancelled: {}", r.cancelled);
        }
        Cmd::Geo(_) => unreachable!("geo handled above"),
    }
    Ok(())
}

fn handle_geo(cmd: GeoCmd) -> Result<()> {
    match cmd {
        GeoCmd::Rasterize {
            vector, output, width, height, bbox, burn_value, no_data,
        } => {
            anyhow::ensure!(bbox.len() == 4, "--bbox requires 4 values");
            let bbox_t = (bbox[0], bbox[1], bbox[2], bbox[3]);
            let out = orbit_geo::rasterization::rasterize(
                &vector, &output, width, height, bbox_t, burn_value, no_data,
            )?;
            println!("rasterized -> {}", out.display());
        }
        GeoCmd::Mosaic { inputs, output } => {
            let out = orbit_geo::gdal_utils::mosaic(&inputs, &output)?;
            println!("mosaic -> {}", out.display());
        }
        GeoCmd::Sample { raster, x, y } => {
            let rds: orbit_geo::dataset::RasterDataset<i16> =
                orbit_geo::builder::RasterDatasetBuilder::from_files(&[raster.clone()])?
                    .build()?;
            let v = orbit_geo::sampling::sample_at_point(&rds, x, y)?;
            println!("{x} {y} {v}");
        }
        GeoCmd::Warp { input, output, target_epsg } => {
            let out = orbit_geo::gdal_utils::warp(&input, &output, target_epsg)?;
            println!("warped -> {} (EPSG:{target_epsg})", out.display());
        }
        GeoCmd::GetImagery { urls } => {
            use std::io::BufRead;
            let lines: Vec<String> = if let Some(p) = urls {
                std::fs::read_to_string(p)?
                    .lines()
                    .filter(|s| !s.trim().is_empty())
                    .map(String::from)
                    .collect()
            } else {
                std::io::stdin()
                    .lock()
                    .lines()
                    .filter_map(|r| r.ok())
                    .filter(|s| !s.trim().is_empty())
                    .collect()
            };
            for href in &lines {
                println!("{}", orbit_geo::providers::vsi_rewrite(href));
            }
        }
    }
    Ok(())
}

fn print_status(s: &pb::JobStatus) {
    let state = pb::JobState::try_from(s.state).map(|v| format!("{v:?}")).unwrap_or_else(|_| "?".into());
    println!("id          : {}", s.id);
    println!("state       : {state}");
    println!("rows_read   : {}", s.rows_read);
    println!("rows_written: {}", s.rows_written);
    println!("started_at  : {}", s.started_at);
    if !s.finished_at.is_empty() { println!("finished_at : {}", s.finished_at); }
    if !s.error.is_empty()      { println!("error       : {}", s.error); }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().compact())
        .init();
}
