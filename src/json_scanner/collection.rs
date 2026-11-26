use std::ops::Range;

/// How a hit stores its path: `String` keeps it, `()` skips building it for span-only searches.
pub(crate) trait HitPath: Send + Sized {
    /// Whether scans need to track paths at all.
    const KEEPS_PATH: bool;

    /// Calls `make` only when the path is kept.
    fn build(make: impl FnOnce() -> String) -> Self;

    /// Returns the stored path, which is empty when it was not kept.
    fn into_string(self) -> String;
}

impl HitPath for String {
    const KEEPS_PATH: bool = true;

    #[inline]
    fn build(make: impl FnOnce() -> String) -> Self {
        make()
    }

    #[inline]
    fn into_string(self) -> String {
        self
    }
}

impl HitPath for () {
    const KEEPS_PATH: bool = false;

    #[inline]
    fn build(_make: impl FnOnce() -> String) -> Self {}

    #[inline]
    fn into_string(self) -> String {
        String::new()
    }
}

/// A format-specific scalar byte span and its accepted-match path.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ScalarHit<P = String> {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) path: P,
}

/// Records a hit's span, and its path when `P` keeps one.
#[inline]
pub(crate) fn push_hit<P: HitPath>(
    hits: &mut Vec<ScalarHit<P>>,
    path: &[u8],
    start: usize,
    end: usize,
) {
    hits.push(ScalarHit {
        start,
        end,
        path: P::build(|| String::from_utf8(path.to_vec()).expect("path is always valid UTF-8")),
    });
}

/// Collects `scan` hits sequentially when `limit` is finite, otherwise from ordered parallel
/// `chunks`. Returns `None` when the document uses unsupported syntax.
#[inline]
pub(crate) fn collect_hits<H, S, I, C, M>(
    (length, limit): (usize, usize),
    init: I,
    scan: C,
    chunks: M,
) -> Option<Vec<H>>
where
    I: FnOnce() -> S,
    C: Fn(Range<usize>, &mut S) -> (Vec<H>, Option<usize>) + Sync,
    M: FnOnce(C) -> Option<Vec<Vec<H>>>,
{
    if limit != usize::MAX {
        let (hits, reached) = scan(0..length, &mut init());
        return reached.map(|_| hits);
    }
    chunks(scan).map(|chunks| chunks.into_iter().flatten().collect())
}
