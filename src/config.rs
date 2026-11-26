/// Controls how text case is handled during matching.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum CaseMode {
    /// Match bytes exactly.
    #[default]
    Sensitive,
    /// Fold ASCII letters to lowercase before matching.
    ///
    /// Non-ASCII bytes are compared exactly.
    AsciiInsensitive,
}

/// Controls the ordering of collected file-search results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PathOrder {
    /// Preserve traversal discovery order for minimum collection overhead.
    #[default]
    Discovery,
    /// Sort matched files lexicographically by path before returning them.
    Path,
}

/// Selects an AST-aware parsing mode for structured input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StructuredMode {
    /// Parse as JSON, exposing jq-like paths such as `$.user.name` and `$.items[0]`.
    Json,
    /// Parse as YAML, exposing jq-like paths such as `$.user.name` and `$.items[0]`.
    Yaml,
    /// Parse as TOML, exposing jq-like paths such as `$.user.name` and `$.items[0]`.
    Toml,
}

/// Controls how structured predicate evaluation handles nulls and type mismatches in data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum StructuredPredicateHandling {
    /// Skip candidate records whose predicate inputs are null or have the wrong type.
    #[default]
    Ignore,
    /// Surface null and type mismatches as structured search errors.
    Error,
}

// Keep configuration fields, getters, and builder setters in one declaration.
macro_rules! search_config {
    ($( [$(#[$getter_doc:meta])*] [$(#[$setter_doc:meta])*]
        $field:ident: $field_type:ty; )*) => {
        /// Runtime configuration for building a [`crate::Searcher`].
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
        pub struct SearchConfig {
            $(pub(crate) $field: $field_type,)*
        }


        impl SearchConfig {
            /// Creates a fluent builder for search configuration.
            ///
            /// # Returns
            ///
            /// A builder initialized with the default configuration.
            #[must_use]
            pub fn builder() -> SearchConfigBuilder {
                SearchConfigBuilder::new()
            }

            $(
                $(#[$getter_doc])*
                #[must_use]
                pub fn $field(self) -> $field_type {
                    self.$field
                }
            )*
        }

        impl SearchConfigBuilder {
            $(
                $(#[$setter_doc])*
                #[must_use]
                pub fn $field(mut self, $field: $field_type) -> Self {
                    self.config.$field = $field;
                    self
                }
            )*
        }
    };
}

search_config! {
    [
        /// Returns the active case mode.
    ]
    [
        /// Sets case handling for all matches.
        ///
        /// # Arguments
        ///
        /// * `case_mode` - Matching mode for case-sensitive or ASCII-insensitive search.
    ]
    case_mode: CaseMode;
    [
        /// Returns true when non-structured patterns are parsed as boolean line queries.
    ]
    [
        /// Enables boolean-query parsing for non-structured exact searches.
        ///
        /// When enabled, patterns are parsed as line-scoped boolean expressions using
        /// `AND`, `OR`, `NOT`, and parentheses. Quoted literals may contain whitespace. On each
        /// line a query holds on, its matches are the occurrences of its terms that are not
        /// negated, or the whole line if it holds through `NOT` alone.
        ///
        /// # Arguments
        ///
        /// * `boolean_queries` - If true, exact text patterns use boolean query syntax.
    ]
    boolean_queries: bool;
    [
        /// Returns true when matches must satisfy ASCII word boundaries.
    ]
    [
        /// Restricts matches to ASCII word boundaries.
        ///
        /// # Arguments
        ///
        /// * `whole_word` - If true, match starts and ends must be adjacent to non-word bytes.
    ]
    whole_word: bool;
    [
        /// Returns true when overlapping matches are allowed.
    ]
    [
        /// Controls whether matches may overlap.
        ///
        /// # Arguments
        ///
        /// * `overlapping` - If false, results are leftmost-longest and non-overlapping.
    ]
    overlapping: bool;
    [
        /// Returns true when directory traversal should ignore `.gitignore` files.
    ]
    [
        /// Controls whether directory traversal ignores `.gitignore`.
        ///
        /// # Arguments
        ///
        /// * `ignore_gitignore` - If true, recursively searched directories do not apply
        ///   `.gitignore` filtering.
    ]
    ignore_gitignore: bool;
    [
        /// Returns the configured ordering for collected path-search results.
    ]
    [
        /// Selects the ordering of collected file-search results.
        ///
        /// # Arguments
        ///
        /// * `path_order` - Discovery order for minimum overhead or deterministic path order.
    ]
    path_order: PathOrder;
    [
        /// Returns an upper bound on emitted matches.
    ]
    [
        /// Sets an optional upper bound on emitted matches.
        ///
        /// # Arguments
        ///
        /// * `max_matches` - Maximum number of matches to emit, or `None` for no limit.
    ]
    max_matches: Option<usize>;
    [
        /// Returns the maximum Levenshtein edit distance for approximate matching.
    ]
    [
        /// Enables approximate matching with a maximum Levenshtein edit distance.
        ///
        /// # Arguments
        ///
        /// * `max_edit_distance` - Maximum edit distance per emitted match, or `None` for exact
        ///   literal matching.
    ]
    max_edit_distance: Option<usize>;
    [
        /// Returns the configured structured parsing mode, if any.
    ]
    [
        /// Enables AST-aware search for a supported structured format.
        ///
        /// # Arguments
        ///
        /// * `structured_mode` - Structured parsing mode to apply before matching.
    ]
    structured_mode: Option<StructuredMode>;
    [
        /// Returns how structured predicates handle nulls and type mismatches.
    ]
    [
        /// Configures null and type-mismatch handling for structured predicates.
        ///
        /// # Arguments
        ///
        /// * `structured_predicate_handling` - Whether predicate type mismatches are ignored
        ///   or returned as errors.
    ]
    structured_predicate_handling: StructuredPredicateHandling;
}

/// Builder for [`SearchConfig`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SearchConfigBuilder {
    config: SearchConfig,
}

impl SearchConfigBuilder {
    /// Creates a builder with default values.
    ///
    /// # Returns
    ///
    /// A builder initialized with the default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Finalizes and returns the immutable configuration.
    ///
    /// # Returns
    ///
    /// The configuration with every value set on this builder.
    #[must_use]
    pub fn build(self) -> SearchConfig {
        self.config
    }
}
