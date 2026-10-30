//! A binary-only member has no public API a host can compile against, so the
//! rules must not fire on it — even though every shape below is textbook R1.
//!
//! Real case: `crawlkit/crates/crawlkit` is the CLI. Its `pub enum Commands`
//! gates clap subcommands per feature across ~14 sites, which is the
//! idiomatic pattern for a binary and not a unification hazard. The member has
//! `src/main.rs` and no `src/lib.rs`, so it is skipped entirely.

/// Which subcommand to run.
#[derive(Debug, Clone)]
pub enum Commands {
    /// Crawl a site.
    Crawl,
    /// Only with the queue feature.
    #[cfg(feature = "queue-ops")]
    Queue,
    /// Only with the full feature set.
    #[cfg(feature = "full")]
    Warehouse,
}

fn main() {
    let _ = Commands::Crawl;
}
