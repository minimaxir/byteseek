use std::sync::OnceLock;
use std::thread::available_parallelism;

use aho_corasick::AhoCorasickKind;

use crate::{CaseMode, SearchConfig};

/// Minimum haystack size for parallel single-pattern search. High because `memmem` is already
/// fast enough that rayon dispatch dominates below it.
pub(crate) const PARALLEL_SINGLE_THRESHOLD: usize = 512 * 1024;

/// Minimum haystack size for parallel multi-pattern search, lower since Aho-Corasick costs
/// more per byte.
pub(crate) const PARALLEL_MULTI_THRESHOLD: usize = 64 * 1024;

/// Minimum parallel chunk size in bytes.
pub(crate) const MIN_CHUNK_SIZE: usize = 16 * 1024;

/// Smallest input whose linear pre-passes, such as an ASCII case fold, run in parallel. Below
/// this, waking the workers costs more than the pass.
pub(crate) const PARALLEL_PASS_LEN: usize = 1024 * 1024;

/// Chunk size, or minimum rayon split, of a parallel pre-pass.
pub(crate) const PARALLEL_PASS_CHUNK: usize = 64 * 1024;

/// Minimum line chunk for approximate line search, long enough to sample for planning.
pub(crate) const MIN_APPROX_LINE_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SizeDomain {
    Tiny,
    Small,
    Medium,
    Large,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SingleStrategy {
    SensitiveMemmem,
    SensitiveAnchored,
    AsciiAnchored,
    AsciiFoldedMemmem,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SingleProfile {
    pub strategy: SingleStrategy,
    pub initial_capacity: usize,
}

pub(crate) fn single_profile(
    config: SearchConfig,
    haystack_len: usize,
    pattern_len: usize,
    limit: usize,
) -> SingleProfile {
    let initial_capacity =
        single_initial_capacity(limit, haystack_len, pattern_len, config.overlapping());

    let strategy = match (config.case_mode(), config.overlapping()) {
        (CaseMode::Sensitive, true) => SingleStrategy::SensitiveAnchored,
        (CaseMode::Sensitive, false) => SingleStrategy::SensitiveMemmem,
        (CaseMode::AsciiInsensitive, false) if haystack_len >= 4_000_000 => {
            SingleStrategy::AsciiFoldedMemmem
        }
        (CaseMode::AsciiInsensitive, true | false) => SingleStrategy::AsciiAnchored,
    };

    SingleProfile {
        strategy,
        initial_capacity,
    }
}

pub(crate) fn multi_initial_capacity(
    haystack_len: usize,
    min_pattern_len: usize,
    limit: usize,
) -> usize {
    multi_capacity(limit, haystack_len, min_pattern_len.max(1))
}

pub(crate) fn aho_kind(pattern_count: usize, max_pattern_len: usize) -> Option<AhoCorasickKind> {
    if pattern_count <= 32 && max_pattern_len <= 64 {
        Some(AhoCorasickKind::DFA)
    } else {
        None
    }
}

pub(crate) fn use_lines_fast_path(
    config: SearchConfig,
    has_newline_pattern: bool,
    haystack_len: usize,
    _pattern_count: usize,
) -> bool {
    !has_newline_pattern && config.max_matches().is_none() && haystack_len >= 16 * 1024
}

/// Whether the current rayon pool has more than one thread, so splitting work can pay off.
#[inline]
pub(crate) fn has_parallel_workers() -> bool {
    rayon::current_num_threads() > 1
}

/// How many files and directories a walk may visit at once.
///
/// Walks are bound by file-system calls, which gain little from SMT siblings or efficiency
/// cores, so this is half the hardware threads, but at least two to keep I/O overlapping.
pub(crate) fn walk_parallelism() -> usize {
    // Cached because the count is a system call.
    static HALF_OF_HARDWARE_THREADS: OnceLock<usize> = OnceLock::new();
    HALF_OF_HARDWARE_THREADS
        .get_or_init(|| available_parallelism().map_or(1, |threads| threads.get().div_ceil(2)))
        .to_owned()
        .max(2)
        .min(rayon::current_num_threads())
}

pub(crate) fn use_parallel_lines(haystack_len: usize, line_count: usize) -> bool {
    haystack_len >= 512 * 1024 && line_count >= 10_000 && has_parallel_workers()
}

// Approximate matching profiles.

/// Shortest haystack an approximate search of one pattern splits across threads.
pub(crate) const PARALLEL_APPROX_THRESHOLD: usize = 256 * 1024;

/// Whether an approximate search split into `task_count` tasks should run them in parallel.
pub(crate) fn approx_use_parallel_tasks(haystack_len: usize, task_count: usize) -> bool {
    // Handing off a task costs tens of microseconds, more than searching a shorter haystack.
    task_count >= 2 && haystack_len >= 64 * 1024 && has_parallel_workers()
}

/// Most matches an overlapping single-pattern search reserves up front.
pub(crate) const OVERLAPPING_CAPACITY_CEILING: usize = 1_048_576;

/// Initial match capacity, sized for typical rather than worst-case density. Overlapping
/// search has a higher ceiling, since regrowing weighs more against its cheap scan.
fn single_initial_capacity(
    limit: usize,
    haystack_len: usize,
    pattern_len: usize,
    overlapping: bool,
) -> usize {
    let expected = haystack_len / pattern_len.max(1).saturating_mul(64).max(1);

    let floor = match (size_domain(haystack_len), overlapping) {
        (SizeDomain::Tiny, true | false) => 8,
        (SizeDomain::Small, true | false) => 16,
        (SizeDomain::Medium, false) => 32,
        (SizeDomain::Medium, true) | (SizeDomain::Large, false) => 64,
        (SizeDomain::Large, true) => 256,
    };
    let ceiling = if overlapping {
        OVERLAPPING_CAPACITY_CEILING
    } else {
        8_192
    };

    expected.clamp(floor, ceiling).min(limit)
}

/// Like [`single_initial_capacity`], for multi-pattern search.
fn multi_capacity(limit: usize, haystack_len: usize, pattern_len: usize) -> usize {
    let expected = haystack_len / pattern_len.max(1).saturating_mul(16).max(1);

    let floor = match size_domain(haystack_len) {
        SizeDomain::Tiny => 8,
        SizeDomain::Small => 16,
        SizeDomain::Medium => 64,
        SizeDomain::Large => 256,
    };
    let ceiling = 131_072;

    expected.clamp(floor, ceiling).min(limit)
}

fn size_domain(haystack_len: usize) -> SizeDomain {
    match haystack_len {
        0..=1023 => SizeDomain::Tiny,
        1024..=65_535 => SizeDomain::Small,
        65_536..=999_999 => SizeDomain::Medium,
        _ => SizeDomain::Large,
    }
}

// Structured value scanning profiles.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StructuredValueStrategy {
    /// Visit and fold every scalar.
    Naive,
    /// Fold the document once and skip non-matching regions with a `memmem` prefilter.
    Prefiltered,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StructuredValueProfile {
    pub strategy: StructuredValueStrategy,
    /// Minimum nesting depth for container prefiltering, since top-level containers span
    /// most of the document.
    pub prefilter_min_depth: usize,
}

/// Selects the scan strategy and prefilter depth for structured value queries.
pub(crate) fn structured_value_profile(
    haystack_len: usize,
    pattern_len: usize,
    _case_insensitive: bool,
) -> StructuredValueProfile {
    // Single-byte patterns are too frequent for a prefilter to prune anything.
    let strategy = if haystack_len >= 512 && pattern_len >= 2 {
        StructuredValueStrategy::Prefiltered
    } else {
        StructuredValueStrategy::Naive
    };

    // Outermost containers nearly always hold the pattern, and skipping one scans all of it.
    let prefilter_min_depth = match size_domain(haystack_len) {
        SizeDomain::Tiny | SizeDomain::Small => 0,
        SizeDomain::Medium | SizeDomain::Large => 2,
    };

    StructuredValueProfile {
        strategy,
        prefilter_min_depth,
    }
}
