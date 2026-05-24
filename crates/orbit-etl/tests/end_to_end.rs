//! End-to-end integration test: file -> Polars -> SQLite via the engine directly.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use orbit_etl::{Engine, FileFormat, FileSource, PipelineSpec};
use sqlx::Row;
use std::path::PathBuf;
use tokio_stream::StreamExt;

#[tokio::test]
async fn csv_to_sqlite_with_sql_and_dedupe() {
    let tmp = tempfile::Builder::new()
        .suffix(".db")
        .tempfile()
        .expect("tempfile");
    let db_url = format!("sqlite://{}?mode=rwc", tmp.path().display());

    let engine = Engine::open(&db_url).await.expect("engine open");

    let sample_csv = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("examples")
        .join("sample.csv");

    let spec = PipelineSpec {
        source: FileSource {
            path: sample_csv,
            format: FileFormat::Csv,
            has_header: true,
            delimiter: ",".into(),
        },
        destination_table: "events_us".into(),
        sql_transform: Some(
            "SELECT user_id, event_id, country, amount, timestamp FROM input WHERE country = 'US'"
                .into(),
        ),
        dedupe_column: Some("event_id".into()),
        batch_size: 4,
    };

    let (_id, mut events) = engine.run(spec).await.expect("run");
    let mut saw_completed = false;
    let mut last_event: Option<orbit_etl::Event> = None;
    while let Some(ev) = events.next().await {
        eprintln!("EVENT: {ev:?}");
        if matches!(ev, orbit_etl::Event::Completed { .. }) {
            saw_completed = true;
        }
        last_event = Some(ev);
    }
    assert!(saw_completed, "pipeline did not complete; last event = {last_event:?}");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&db_url)
        .await
        .expect("connect");
    let row = sqlx::query("SELECT COUNT(*) as n FROM events_us")
        .fetch_one(&pool)
        .await
        .expect("count");
    let n: i64 = row.get("n");
    // sample has 5 US rows including one duplicate -> 4 unique by event_id
    assert_eq!(n, 4, "expected 4 unique US rows, got {n}");
}
