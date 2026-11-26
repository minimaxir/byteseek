use std::hash::BuildHasher;
use std::ops::Range;
use std::sync::OnceLock;

use hashbrown::{DefaultHashBuilder, HashTable};

use crate::chunks::map_chunks;
use crate::profiles;

/// Returns a clone of one process-wide hasher, so tables from different threads can merge
/// using each other's stored hashes.
fn shared_hasher() -> DefaultHashBuilder {
    static HASHER: OnceLock<DefaultHashBuilder> = OnceLock::new();
    HASHER.get_or_init(DefaultHashBuilder::default).clone()
}

/// A clause's first satisfying scalar span within one object.
type ClauseSpan = Option<(usize, usize)>;

/// Per-object clause states for one predicate pattern, keyed by object path.
///
/// Object paths live in one byte arena and are indexed by a hash table of object ids, so
/// recording a hit allocates nothing per object. Objects are kept in first-hit order.
pub(super) struct PredicateObjects {
    clause_count: usize,
    hasher: DefaultHashBuilder,
    index: HashTable<u32>,
    /// Concatenated object paths; object `i` spans `path_ends[i - 1]..path_ends[i]`.
    paths: Vec<u8>,
    path_ends: Vec<usize>,
    /// Hash of each object path, kept so merging tables and growing the index never rehash.
    path_hashes: Vec<u64>,
    /// `clause_count` spans per object.
    spans: Vec<ClauseSpan>,
}

impl PredicateObjects {
    pub(super) fn new(clause_count: usize) -> Self {
        Self {
            clause_count,
            hasher: shared_hasher(),
            index: HashTable::new(),
            paths: Vec::new(),
            path_ends: Vec::new(),
            path_hashes: Vec::new(),
            spans: Vec::new(),
        }
    }

    /// Whether no object has a recorded hit.
    pub(super) fn is_empty(&self) -> bool {
        self.path_ends.is_empty()
    }

    /// Forgets every object, keeping the storage for the next document's.
    pub(super) fn clear(&mut self) {
        self.index.clear();
        self.paths.clear();
        self.path_ends.clear();
        self.path_hashes.clear();
        self.spans.clear();
    }

    fn path<'a>(paths: &'a [u8], path_ends: &[usize], object: usize) -> &'a [u8] {
        let start = object
            .checked_sub(1)
            .map_or(0, |previous| path_ends[previous]);
        &paths[start..path_ends[object]]
    }

    /// Records `span` for `clause_index` of the object at `object_path` unless that clause
    /// already holds an earlier span.
    pub(super) fn record(&mut self, object_path: &[u8], clause_index: usize, span: (usize, usize)) {
        let object = self.object_id(object_path);
        self.spans[object * self.clause_count + clause_index].get_or_insert(span);
    }

    fn object_id(&mut self, object_path: &[u8]) -> usize {
        // Clauses of one object are usually visited consecutively; skip hashing for them.
        if let Some(last) = self.path_ends.len().checked_sub(1)
            && Self::path(&self.paths, &self.path_ends, last) == object_path
        {
            return last;
        }
        self.object_id_with_hash(object_path, self.hasher.hash_one(object_path))
    }

    fn object_id_with_hash(&mut self, object_path: &[u8], hash: u64) -> usize {
        let Self {
            index,
            paths,
            path_ends,
            path_hashes,
            spans,
            clause_count,
            ..
        } = self;
        if let Some(&object) = index.find(hash, |&object| {
            Self::path(paths, path_ends, object as usize) == object_path
        }) {
            return object as usize;
        }

        let object = path_ends.len();
        paths.extend_from_slice(object_path);
        path_ends.push(paths.len());
        path_hashes.push(hash);
        spans.resize(spans.len() + *clause_count, None);
        let object_id = u32::try_from(object).expect("object count fits in u32");
        index.insert_unique(hash, object_id, |&existing| path_hashes[existing as usize]);
        object
    }

    /// Reserves room for absorbing every table of `later` without growing storage mid-merge.
    pub(super) fn reserve_for<'a>(&mut self, later: impl Iterator<Item = &'a Self>) {
        let (objects, path_bytes) = later.fold((0, 0), |(objects, path_bytes), table| {
            (
                objects + table.path_ends.len(),
                path_bytes + table.paths.len(),
            )
        });
        let path_hashes = &self.path_hashes;
        self.index
            .reserve(objects, |&object| path_hashes[object as usize]);
        self.paths.reserve(path_bytes);
        self.path_ends.reserve(objects);
        self.path_hashes.reserve(objects);
        self.spans.reserve(objects * self.clause_count);
    }

    /// Merges `later`, filled from the input following this table's, as if its hits had been
    /// recorded here.
    pub(super) fn absorb(&mut self, later: Self) {
        for (object, clause_spans) in later.spans.chunks_exact(later.clause_count).enumerate() {
            let object_path = Self::path(&later.paths, &later.path_ends, object);
            let merged = self.object_id_with_hash(object_path, later.path_hashes[object]);
            let merged_spans = &mut self.spans[merged * self.clause_count..][..self.clause_count];
            for (merged_span, span) in merged_spans.iter_mut().zip(clause_spans) {
                if let Some(span) = *span {
                    merged_span.get_or_insert(span);
                }
            }
        }
    }

    /// Maps `(object_path, first_clause_span)` of every object whose clauses all hold a span,
    /// in first-hit order, in parallel for large tables.
    pub(super) fn map_complete<T: Send>(
        &self,
        map: impl Fn(&[u8], (usize, usize)) -> T + Sync,
    ) -> Vec<T> {
        /// Fewest objects worth a parallel task.
        const MIN_OBJECTS_PER_TASK: usize = 512;

        let object_count = self.spans.len() / self.clause_count;
        let map_objects = |objects: Range<usize>| {
            self.spans[objects.start * self.clause_count..objects.end * self.clause_count]
                .chunks_exact(self.clause_count)
                .zip(objects)
                .filter_map(|(clause_spans, object)| match clause_spans {
                    [Some(first), rest @ ..] if rest.iter().all(Option::is_some) => Some(map(
                        Self::path(&self.paths, &self.path_ends, object),
                        *first,
                    )),
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        // Several tasks per thread, so work stealing can balance them.
        let task_count =
            (object_count / MIN_OBJECTS_PER_TASK).min(4 * rayon::current_num_threads());
        if task_count < 2 || !profiles::has_parallel_workers() {
            return map_objects(0..object_count);
        }
        let mapped = map_chunks(task_count, |task| {
            map_objects(object_count * task / task_count..object_count * (task + 1) / task_count)
        });
        let mut complete = Vec::with_capacity(mapped.iter().map(Vec::len).sum());
        complete.extend(mapped.into_iter().flatten());
        complete
    }
}

#[cfg(test)]
mod tests {
    use super::PredicateObjects;

    #[test]
    fn keeps_first_span_per_clause_and_reports_only_complete_objects() {
        let mut objects = PredicateObjects::new(2);
        objects.record(b"$.a", 0, (1, 2));
        objects.record(b"$.b", 0, (5, 6));
        objects.record(b"$.a", 0, (3, 4));
        objects.record(b"$.a", 1, (7, 8));
        objects.record(b"$.c", 1, (9, 10));

        let complete = objects.map_complete(|path, span| (path.to_vec(), span));

        assert_eq!(complete, vec![(b"$.a".to_vec(), (1, 2))]);
    }

    #[test]
    fn a_cleared_table_records_the_next_document_from_scratch() {
        let mut objects = PredicateObjects::new(2);
        objects.record(b"$.a", 0, (1, 2));
        objects.record(b"$.b", 0, (3, 4));
        objects.record(b"$.b", 1, (5, 6));

        objects.clear();
        let emptied = objects.is_empty();
        objects.record(b"$.a", 1, (7, 8));
        objects.record(b"$.a", 0, (9, 10));

        assert!(emptied && !objects.is_empty());
        assert_eq!(
            objects.map_complete(|path, span| (path.to_vec(), span)),
            vec![(b"$.a".to_vec(), (9, 10))]
        );
    }

    #[test]
    fn absorbing_a_later_table_matches_recording_in_order() {
        let hits = [
            (&b"$.a"[..], 0, (1, 2)),
            (b"$.b", 1, (3, 4)),
            (b"$.a", 1, (5, 6)),
            (b"$.c", 0, (7, 8)),
            (b"$.b", 0, (9, 10)),
            (b"$.a", 0, (11, 12)),
            (b"$.c", 1, (13, 14)),
        ];
        let complete =
            |objects: &PredicateObjects| objects.map_complete(|path, span| (path.to_vec(), span));
        let mut sequential = PredicateObjects::new(2);
        for &(path, clause, span) in &hits {
            sequential.record(path, clause, span);
        }

        for split in 0..=hits.len() {
            let mut earlier = PredicateObjects::new(2);
            let mut later = PredicateObjects::new(2);
            for (index, &(path, clause, span)) in hits.iter().enumerate() {
                let table = if index < split {
                    &mut earlier
                } else {
                    &mut later
                };
                table.record(path, clause, span);
            }

            earlier.reserve_for(std::iter::once(&later));
            earlier.absorb(later);

            assert_eq!(
                complete(&earlier),
                complete(&sequential),
                "split at {split}"
            );
        }
    }

    #[test]
    fn distinguishes_many_objects() {
        let mut objects = PredicateObjects::new(1);
        for index in 0..1_000 {
            objects.record(format!("$[{index}]").as_bytes(), 0, (index, index + 1));
            objects.record(format!("$[{}]", index / 2).as_bytes(), 0, (0, 0));
        }

        let complete = objects.map_complete(|path, span| (path.to_vec(), span.0));

        assert_eq!(complete.len(), 1_000);
        for (object, (path, start)) in complete.into_iter().enumerate() {
            assert_eq!(start, object);
            assert_eq!(path, format!("$[{object}]").into_bytes());
        }
    }
}
