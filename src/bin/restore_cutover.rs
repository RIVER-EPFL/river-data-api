//! Carry a dumped production database's curated state into the rebuilt one.
//!
//! `cargo run --bin restore_cutover -- <source-url> <target-url>`, where the source is a scratch
//! database holding the production dump and the target is the rebuilt database. The cutover order
//! is:
//!
//! 1. `pg_dump` production.
//! 2. `pg_restore` that dump into a scratch database on the new server.
//! 3. Build the new database from the baseline migration and let the pairing plans and the sync
//!    services mint its sites, parameters, streams and instruments.
//! 4. Run this. It moves the samples, the readings, the curation ledger and the public API setups,
//!    and prints every natural key the source holds that the rebuilt database does not.
//! 5. Verify: `scripts/dbdiff.sh <scratch> <target>` compares the two per table.
//!
//! Nothing is carried by id. A reference the rebuilt database cannot match by natural key is
//! reported, and the run writes nothing at all if it fails partway.

use sea_orm::Database;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (Some(source_url), Some(target_url)) = (args.next(), args.next()) else {
        eprintln!("usage: restore_cutover <source-url> <target-url>");
        std::process::exit(2);
    };
    let source = Database::connect(source_url).await?;
    let target = Database::connect(target_url).await?;
    let report = river_db::restore::restore(&source, &target).await?;
    for (table, rows) in &report.carried {
        println!("{rows} {table}");
    }
    println!(
        "{} projects configured, {} slots exposed",
        report.projects_configured, report.slots_exposed
    );
    for line in &report.refused {
        println!("refused: {line}");
    }
    for line in &report.unmatched {
        println!("unmatched: {line}");
    }
    Ok(())
}
