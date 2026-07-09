//! Benchmark for incremental resolution: indexes and resolves a directory, then re-indexes a
//! single (edited) file and resolves again, and finally runs a no-op resolve. Combine with
//! RUBYDEX_RESOLUTION_PROFILE=1 for per-phase breakdowns of each resolve.
//!
//! Usage: cargo run --release --example incremental -- <directory>

use std::collections::HashSet;
use std::time::Instant;

use rubydex::{
    indexing::{self, IndexerBackend},
    listing,
    model::graph::Graph,
    resolution::Resolver,
};

fn main() {
    let dir = std::env::args().nth(1).expect("directory argument");
    let (files, _errors) = listing::collect_file_paths(vec![dir], &HashSet::new());
    let mut graph = Graph::new();

    let started = Instant::now();
    indexing::index_files(&mut graph, files.clone(), IndexerBackend::RubyIndexer);
    eprintln!(
        "### initial index:       {:?} ({} files)",
        started.elapsed(),
        files.len()
    );

    let started = Instant::now();
    Resolver::new(&mut graph).resolve();
    eprintln!("### initial resolve:     {:?}", started.elapsed());

    // Simulate an edit: append a new module to one mid-sized file and re-index just that file
    let target = files[files.len() / 2].clone();
    let original = std::fs::read_to_string(&target).unwrap();
    std::fs::write(
        &target,
        format!("{original}\nmodule RdxIncrementalProbe\n  def probe_method; end\nend\n"),
    )
    .unwrap();

    let started = Instant::now();
    indexing::index_files(&mut graph, vec![target.clone()], IndexerBackend::RubyIndexer);
    eprintln!(
        "### reindex 1 file:      {:?} ({})",
        started.elapsed(),
        target.display()
    );

    let started = Instant::now();
    Resolver::new(&mut graph).resolve();
    eprintln!("### incremental resolve: {:?}", started.elapsed());

    let started = Instant::now();
    Resolver::new(&mut graph).resolve();
    eprintln!("### no-op resolve:       {:?}", started.elapsed());

    std::fs::write(&target, original).unwrap();
    std::mem::forget(graph);
}
