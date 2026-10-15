//! Turning a type name as written in source into a fully-qualified name.
//!
//! This is used twice: the src.zip reader resolves a JDK source file's own
//! `extends`/`implements` names against the table of every type name it has
//! collected so far, and the server resolves the open buffer's unknown names
//! against the same table. Both go through [`simple_name`], because the
//! invariant it enforces — *ambiguity answers nothing, never a guess* — only
//! holds if there is exactly one place that decides.

/// Whether a set of fully-qualified type names contains a given one.
///
/// Implemented over a real fact store once one exists, and over a bare list
/// of names by the src.zip reader's first pass in the meantime — both only
/// ever need this one method.
pub trait TypeTable {
    fn contains(&self, fqn: &str) -> bool;
}

/// The outcome of resolving a name as written against a [`TypeTable`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Exactly one reachable type has this name.
    One(String),
    /// Two or more do. The caller must answer nothing.
    Ambiguous,
    /// None does, as far as this table knows.
    Unknown,
}

/// Resolve a type name as written in a compilation unit.
///
/// `name` is what the author wrote, dots included: `String`, `Map.Entry`.
/// `package` and `imports` come from `FileSyntax`.
///
/// Tiers, in this order — the first with any hit decides, and two or more
/// hits *inside that one tier* is [`Resolution::Ambiguous`]:
///
///  1. Single-type imports whose last segment equals the name's first
///     segment. The import names its target directly, so unlike the tiers
///     below it this one needs no lookup against `table` to count as a hit;
///     two single-type imports that both end in the same simple name are a
///     javac-level error regardless of what either target resolves to, so
///     they are ambiguous on the import list alone.
///  2. The file's own package.
///  3. Explicit on-demand imports.
///  4. `java.lang`.
///
/// Tiers 2 through 4 have no import to name their target, so a candidate
/// only counts as a hit once `table.contains` confirms it.
///
/// A name that already contains a dot and resolves against `table` as
/// written — `java.util.List` — is answered directly, before any tier runs:
/// a name written with a package prefix is either already correct or nothing
/// here can improve on it. The dot is required: without it, a bare
/// default-package entry in `table` could short-circuit tier 1 for a simple
/// name a single-type import already claims, which is exactly the silent
/// cross-tier override the tier ranking exists to prevent.
///
/// A dotted name resolves its first segment through the tiers, then appends
/// the rest and checks `contains`: `Map.Entry` with `import java.util.Map;`
/// resolves `Map` to `java.util.Map`, then answers `java.util.Map.Entry` if
/// `table` has it and `Unknown` if not.
pub fn simple_name(
    name: &str,
    package: Option<&str>,
    imports: &[javelin_parser::ImportDecl],
    table: &dyn TypeTable,
) -> Resolution {
    if is_malformed(name) {
        return Resolution::Unknown;
    }
    if name.contains('.') && table.contains(name) {
        return Resolution::One(name.to_owned());
    }

    let (first, rest) = name.split_once('.').unwrap_or((name, ""));

    let single_type_imports: Vec<String> = imports
        .iter()
        .filter(|import| !import.is_static && !import.is_on_demand)
        .filter(|import| last_segment(&import.name) == first)
        .map(|import| import.name.clone())
        .collect();
    if let Some(resolution) = decide(single_type_imports, rest, table) {
        return resolution;
    }

    let package_hit: Vec<String> = std::iter::once(match package {
        Some(package) => format!("{package}.{first}"),
        None => first.to_owned(),
    })
    .filter(|candidate| table.contains(candidate))
    .collect();
    if let Some(resolution) = decide(package_hit, rest, table) {
        return resolution;
    }

    // ponytail: JLS 7.5.2 makes the implicit `java.lang.*` an on-demand
    // import at the same rank as an explicit one, so javac rejects a clash
    // between `java.lang.X` and an on-demand-imported X as ambiguous — what
    // `java.lang.Record` did to `import org.jooq.*;`. Ranking `java.lang`
    // below answers the explicit import instead: what such a file meant, but
    // an answer where javac has none. The upgrade is one tier for both, which
    // changes stored rows and so needs a `javelin_index::SCHEMA_VERSION` bump.
    let on_demand_imports: Vec<String> = imports
        .iter()
        // ponytail: `import static a.B.C;` and `import static a.B.*;` can name
        // a nested type, and neither is read, so such a name is `Unknown`. The
        // upgrade is counting each in its non-static tier once `table` confirms
        // the type — a stored-rows change, so a schema bump.
        .filter(|import| !import.is_static && import.is_on_demand)
        .map(|import| format!("{}.{first}", import.name))
        .filter(|candidate| table.contains(candidate))
        .collect();
    if let Some(resolution) = decide(on_demand_imports, rest, table) {
        return resolution;
    }

    let java_lang_hit: Vec<String> = std::iter::once(format!("java.lang.{first}"))
        .filter(|candidate| table.contains(candidate))
        .collect();
    if let Some(resolution) = decide(java_lang_hit, rest, table) {
        return resolution;
    }

    Resolution::Unknown
}

/// `hits` is one tier's candidates. `None` means the tier had none, so the
/// caller should move on; `Some` is the tier's final answer, win or not.
fn decide(hits: Vec<String>, rest: &str, table: &dyn TypeTable) -> Option<Resolution> {
    match hits.len() {
        0 => None,
        1 => {
            let base = hits.into_iter().next().expect("checked len == 1");
            Some(if rest.is_empty() {
                Resolution::One(base)
            } else {
                let full = format!("{base}.{rest}");
                if table.contains(&full) {
                    Resolution::One(full)
                } else {
                    Resolution::Unknown
                }
            })
        }
        _ => Some(Resolution::Ambiguous),
    }
}

/// An empty name, or a dotted name with an empty segment (`.`, `a..b`,
/// `Map.`) — never a real Java name, so never worth a tier lookup.
fn is_malformed(name: &str) -> bool {
    name.is_empty() || name.split('.').any(str::is_empty)
}

fn last_segment(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_document::{Position, Range};
    use javelin_parser::{ImportDecl, Span};

    struct FakeTable(Vec<&'static str>);

    impl TypeTable for FakeTable {
        fn contains(&self, fqn: &str) -> bool {
            self.0.contains(&fqn)
        }
    }

    fn zero_span() -> Span {
        Span {
            start_byte: 0,
            end_byte: 0,
            range: Range::new(Position::default(), Position::default()),
        }
    }

    fn single_import(name: &str) -> ImportDecl {
        ImportDecl {
            name: name.to_owned(),
            span: zero_span(),
            name_span: zero_span(),
            is_static: false,
            is_on_demand: false,
        }
    }

    fn on_demand_import(name: &str) -> ImportDecl {
        ImportDecl {
            name: name.to_owned(),
            span: zero_span(),
            name_span: zero_span(),
            is_static: false,
            is_on_demand: true,
        }
    }

    fn static_import(name: &str) -> ImportDecl {
        ImportDecl {
            name: name.to_owned(),
            span: zero_span(),
            name_span: zero_span(),
            is_static: true,
            is_on_demand: false,
        }
    }

    #[test]
    fn a_single_type_import_decides() {
        let table = FakeTable(vec!["java.util.List"]);
        let imports = [single_import("java.util.List")];

        assert_eq!(
            simple_name("List", None, &imports, &table),
            Resolution::One("java.util.List".into())
        );
    }

    #[test]
    fn the_files_own_package_beats_an_on_demand_import() {
        let table = FakeTable(vec!["com.example.Widget", "java.util.Widget"]);
        let imports = [on_demand_import("java.util")];

        assert_eq!(
            simple_name("Widget", Some("com.example"), &imports, &table),
            Resolution::One("com.example.Widget".into())
        );
    }

    #[test]
    fn an_on_demand_import_beats_java_lang() {
        let table = FakeTable(vec!["java.util.Formatter", "java.lang.Formatter"]);
        let imports = [on_demand_import("java.util")];

        assert_eq!(
            simple_name("Formatter", None, &imports, &table),
            Resolution::One("java.util.Formatter".into())
        );
    }

    #[test]
    fn two_on_demand_imports_offering_the_same_name_answer_ambiguous() {
        let table = FakeTable(vec!["java.util.List", "java.awt.List"]);
        let imports = [on_demand_import("java.util"), on_demand_import("java.awt")];

        assert_eq!(
            simple_name("List", None, &imports, &table),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn two_single_type_imports_offering_the_same_name_answer_ambiguous() {
        let table = FakeTable(vec!["a.Widget", "b.Widget"]);
        let imports = [single_import("a.Widget"), single_import("b.Widget")];

        assert_eq!(
            simple_name("Widget", None, &imports, &table),
            Resolution::Ambiguous
        );
    }

    #[test]
    fn a_name_no_reachable_package_offers_is_unknown() {
        let table = FakeTable(vec!["java.util.List"]);
        let imports: [ImportDecl; 0] = [];

        assert_eq!(
            simple_name("Frobnicator", Some("com.example"), &imports, &table),
            Resolution::Unknown
        );
    }

    #[test]
    fn an_already_qualified_name_resolves_without_imports() {
        let table = FakeTable(vec!["java.util.List"]);
        let imports: [ImportDecl; 0] = [];

        assert_eq!(
            simple_name("java.util.List", None, &imports, &table),
            Resolution::One("java.util.List".into())
        );
    }

    #[test]
    fn a_nested_name_resolves_through_its_first_segment() {
        let table = FakeTable(vec!["java.util.Map", "java.util.Map.Entry"]);
        let imports = [single_import("java.util.Map")];

        assert_eq!(
            simple_name("Map.Entry", None, &imports, &table),
            Resolution::One("java.util.Map.Entry".into())
        );
    }

    #[test]
    fn a_nested_name_whose_outer_resolves_but_whose_inner_does_not_is_unknown() {
        let table = FakeTable(vec!["java.util.Map"]);
        let imports = [single_import("java.util.Map")];

        assert_eq!(
            simple_name("Map.Bogus", None, &imports, &table),
            Resolution::Unknown
        );
    }

    // Global constraint 2: never panic on invalid input from a buffer that is
    // usually mid-edit, and answer Unknown rather than something plausible.
    #[test]
    fn malformed_names_never_panic() {
        let table = FakeTable(vec!["java.util.List"]);
        let imports: [ImportDecl; 0] = [];

        for name in ["", ".", "a..b", "Map.", &"a.".repeat(10_000)] {
            assert_eq!(
                simple_name(name, None, &imports, &table),
                Resolution::Unknown,
                "input: {name:?}"
            );
        }
    }

    // A static import — including a static on-demand one — is ignored for
    // type resolution: it never contributes a hit.
    #[test]
    fn a_static_import_is_never_a_hit() {
        let table = FakeTable(vec!["a.Widget"]);
        let imports = [static_import("a.Widget")];

        assert_eq!(
            simple_name("Widget", None, &imports, &table),
            Resolution::Unknown
        );
    }

    #[test]
    fn a_single_type_import_beats_the_files_own_package() {
        let table = FakeTable(vec!["a.Widget", "com.example.Widget"]);
        let imports = [single_import("a.Widget")];

        assert_eq!(
            simple_name("Widget", Some("com.example"), &imports, &table),
            Resolution::One("a.Widget".into())
        );
    }

    // Tier 1 does not filter through `table.contains`: a single-type import
    // commits to its target syntactically, whether or not this table (built
    // from a partial src.zip read, say) knows about it yet.
    #[test]
    fn a_single_type_import_resolves_even_when_the_table_does_not_know_its_target() {
        let table = FakeTable(vec![]);
        let imports = [single_import("a.Widget")];

        assert_eq!(
            simple_name("Widget", None, &imports, &table),
            Resolution::One("a.Widget".into())
        );
    }
}
