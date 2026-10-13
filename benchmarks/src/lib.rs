//! Shared inputs and statistics for Javelin's benchmarks.
//!
//! Performance is a product requirement here, not a follow-up task, so the
//! harness exists before the features it will measure. Two things live in this
//! crate: synthetic Java at a few known sizes, and percentile maths, because
//! the interesting number for an interactive request is p95, not the mean.
//!
//! Fixtures are generated rather than committed. A checked-in 50k-line Java file
//! is a repository people have to clone; a generator is twenty lines and always
//! reproducible.

use std::fmt::Write as _;
use std::time::Duration;

/// Benchmark sizes, chosen to bracket the range of real Java files and
/// workspaces rather than to hit round numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// A typical hand-written class.
    Small,
    /// A large but still hand-maintained service class.
    Medium,
    /// A generated client or a legacy god-class.
    Large,
    /// Pathological generated source. Must degrade, never fall over.
    Huge,
}

impl Tier {
    pub const ALL: [Tier; 4] = [Tier::Small, Tier::Medium, Tier::Large, Tier::Huge];

    pub fn name(self) -> &'static str {
        match self {
            Tier::Small => "small",
            Tier::Medium => "medium",
            Tier::Large => "large",
            Tier::Huge => "huge",
        }
    }

    /// Number of methods in the generated class.
    pub fn methods(self) -> usize {
        match self {
            Tier::Small => 10,
            Tier::Medium => 200,
            Tier::Large => 2_000,
            Tier::Huge => 20_000,
        }
    }
}

/// Generate a syntactically valid Java class of the given size.
///
/// Grown for Task 9, and every substring anything outside this function
/// searched for is still there at the same offset relative to the method it
/// sits in — notably `completion.rs`'s `"int local = "` marker — so nothing
/// above this function had to change to keep working, only to be re-measured.
/// Mostly additive: a `var` local and an unqualified call to a sibling
/// method, per generated method, plus one overload set and its call sites,
/// once per tier. One line is rewritten rather than added, and stays inside
/// the pre-existing `if`: `return local * 2;` became `return doubled;`, so
/// the `var` local just added has a use. Phase 2b and 2c added exactly those
/// three shapes and the pre-Task-9 fixture exercised none of them.
pub fn java_source(tier: Tier) -> String {
    let methods = tier.methods();
    let mut source = String::with_capacity(methods * 220);
    source.push_str("package com.example.generated;\n\npublic class Generated {\n");
    for i in 0..methods {
        // Wraps rather than always forward: at `i == methods - 1` the only
        // sibling left is `method0`, so the call stays unqualified-to-a-real-
        // method at every tier down to `Tier::Small`'s ten.
        let next = (i + 1) % methods;
        let _ = write!(
            source,
            "    private int field{i} = {i};\n\
             \n\
                 public int method{i}(int argument) {{\n\
             \x20       int local = argument + field{i};\n\
             \x20       var doubled = local * 2;\n\
             \x20       method{next}(argument);\n\
             \x20       if (local > {i}) {{\n\
             \x20           return doubled;\n\
             \x20       }}\n\
             \x20       return local;\n\
             \x20   }}\n\n"
        );
    }
    // Once per tier, not per method: the elimination benchmark asks about one
    // call site, and `sole_candidate` walking a member list this size is the
    // point, not walking it once per generated method too.
    source.push_str(
        "    void over(int i) {}\n\
         \x20   void over(Generated g) {}\n\
         \x20   void over(String s) {}\n\
         \n\
         \x20   void overloadCallSites() {\n\
         \x20       this.over(1);\n\
         \x20       this.over(new Generated());\n\
         \x20       this.over(\"s\");\n\
         \x20   }\n\n",
    );
    source.push_str("}\n");
    source
}

/// Latency percentiles.
///
/// Criterion reports central tendency, which is the wrong summary for an
/// interactive request: users notice the slow ones. Anything measuring a
/// user-visible operation should report these alongside.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Percentiles {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub max: Duration,
}

/// Compute percentiles from raw samples using nearest-rank.
///
/// Returns `None` for an empty sample set rather than inventing a number.
pub fn percentiles(samples: &mut [Duration]) -> Option<Percentiles> {
    if samples.is_empty() {
        return None;
    }
    samples.sort_unstable();
    let at = |q: f64| -> Duration {
        // Nearest-rank: the smallest value at or above the qth percentile.
        let rank = (q * samples.len() as f64).ceil().max(1.0) as usize;
        samples[rank.min(samples.len()) - 1]
    };
    Some(Percentiles {
        p50: at(0.50),
        p95: at(0.95),
        p99: at(0.99),
        max: samples[samples.len() - 1],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_sources_grow_with_the_tier() {
        let sizes: Vec<_> = Tier::ALL.iter().map(|t| java_source(*t).len()).collect();
        assert!(sizes.windows(2).all(|w| w[0] < w[1]), "{sizes:?}");
    }

    #[test]
    fn generated_java_actually_parses() {
        let mut parser = javelin_parser::JavaParser::new().expect("grammar");
        let document = javelin_document::TextDocument::new(
            javelin_document::DocumentId::new("bench://Generated.java"),
            1,
            java_source(Tier::Small),
        );
        let tree = parser.parse(&document).expect("parse");
        assert!(!tree.has_errors(), "the generator must emit valid Java");
    }

    // A fixture that silently stopped emitting a shape a benchmark claims to
    // measure is exactly the failure Task 9 exists to fix, so each shape gets
    // its own guard rather than one combined assertion.

    #[test]
    fn the_generated_source_contains_a_var_local() {
        let source = java_source(Tier::Small);
        assert!(source.contains("var doubled = local * 2;"), "{source}");
    }

    #[test]
    fn the_generated_source_contains_an_unqualified_call() {
        // `method0`'s own body, calling `method1` with no receiver — the shape
        // `call_definition_at` resolves and `member_completions_at` never
        // sees.
        let source = java_source(Tier::Small);
        assert!(source.contains("method1(argument);"), "{source}");
    }

    #[test]
    fn the_generated_source_contains_an_overload_set() {
        let source = java_source(Tier::Small);
        assert!(source.contains("void over(int i) {}"), "{source}");
        assert!(source.contains("void over(Generated g) {}"), "{source}");
        assert!(source.contains("void over(String s) {}"), "{source}");
        assert!(source.contains("this.over(1);"), "{source}");
        assert!(source.contains("this.over(new Generated());"), "{source}");
        assert!(source.contains("this.over(\"s\");"), "{source}");
    }

    #[test]
    fn percentiles_use_nearest_rank() {
        let mut samples: Vec<_> = (1..=100).map(Duration::from_millis).collect();
        let stats = percentiles(&mut samples).expect("non-empty");
        assert_eq!(stats.p50, Duration::from_millis(50));
        assert_eq!(stats.p95, Duration::from_millis(95));
        assert_eq!(stats.p99, Duration::from_millis(99));
        assert_eq!(stats.max, Duration::from_millis(100));
    }

    #[test]
    fn no_samples_means_no_answer() {
        assert!(percentiles(&mut []).is_none());
    }
}
