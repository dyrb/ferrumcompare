//! Read-only folder comparison and bounded, disk-backed text diffs.
mod compare;
pub mod export;
pub mod filter;
mod scan;
pub mod text;
mod verify;

pub use compare::{Comparison, Difference, State, Summary};
pub use scan::ordinal as name_order;
pub use scan::{Backend, Case, NONE, Node, Options, Progress, Tree};

use std::io;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Instant;

pub fn compare(left: &std::path::Path, right: &std::path::Path, options: &Options, progress: &Progress) -> io::Result<Comparison> {
    let started = Instant::now();
    progress.phase.store(1, Relaxed);
    let (left, right) = scan::pair(left, right, options, progress)?;
    progress.check()?;
    progress.phase.store(2, Relaxed);
    let mut result = Comparison::build(left, right, options.case, progress)?;
    result.scope = options.scope.clone();
    verify::run(&mut result, options, progress)?;
    result.aggregate();
    result.elapsed = started.elapsed();
    progress.phase.store(4, Relaxed);
    Ok(result)
}
