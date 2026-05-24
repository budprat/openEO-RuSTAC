//! CLI entry — diff two Rust source trees by public surface.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;

use anyhow::Result;
use api_diff::{collect_tree, diff, write_csv};
use clap::Parser;

#[derive(Parser, Debug)]
#[command(
    name = "api-diff",
    version,
    about = "Diff two Rust source trees by public-symbol surface; emits CSV."
)]
struct Args {
    /// Path to the left-side source root (e.g. upstream raster engine libs).
    #[arg(long)]
    left: PathBuf,

    /// Path to the right-side source root (e.g. mvp/orbit-etl/crates).
    #[arg(long)]
    right: PathBuf,

    /// Output CSV path. Use `-` for stdout.
    #[arg(long, default_value = "-")]
    out: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let left = collect_tree(&args.left)?;
    let right = collect_tree(&args.right)?;
    let rows = diff(&left, &right);

    eprintln!(
        "left={left_syms}, right={right_syms}, rows={rows}, only_left={only_left}, only_right={only_right}, sig_mismatch={mismatch}",
        left_syms = left.len(),
        right_syms = right.len(),
        rows = rows.len(),
        only_left = rows.iter().filter(|r| r.in_left && !r.in_right).count(),
        only_right = rows.iter().filter(|r| !r.in_left && r.in_right).count(),
        mismatch = rows
            .iter()
            .filter(|r| r.signature_match == Some(false))
            .count(),
    );

    if args.out == "-" {
        write_csv(std::io::stdout(), &rows)?;
    } else {
        let f = std::fs::File::create(&args.out)?;
        write_csv(f, &rows)?;
    }
    Ok(())
}
