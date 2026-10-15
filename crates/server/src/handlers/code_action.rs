//! `textDocument/codeAction` — the add-import quick fix and removing unused
//! imports.
//!
//! Add import: one action per type the unresolved name could mean, for each
//! `unresolved-type` warning in the request's context. The user picks, so
//! several candidates are a choice and not a guess. A candidate is only a type
//! the file could import — public, nested only in public types, and on the
//! classpath of the module the file belongs to.
//!
//! Remove unused imports: a single-type import whose simple name is written
//! nowhere else in the file. Removing an import nothing names cannot change
//! what any name means, so the edit is whole by construction.

use std::collections::{HashMap, HashSet};

use javelin_classpath::Resolution;
use javelin_core::{CancellationToken, Cancelled};
use javelin_document::DocumentId;
use javelin_parser::FileSyntax;
use lsp_types::{
    CodeAction, CodeActionKind, CodeActionOrCommand, CodeActionParams, Diagnostic, NumberOrString,
    Position, Range, TextEdit, Uri, WorkspaceEdit,
};

use crate::ServerState;
use crate::classpath::ClasspathExternals;
use crate::handlers::rename::current_view;
use crate::handlers::{to_lsp_position, to_position};
use crate::state::FileView;

/// The code the type diagnostic is published under; see `diagnostics.rs`.
const UNRESOLVED_TYPE: &str = "unresolved-type";

pub fn handle(
    state: &mut ServerState,
    params: &CodeActionParams,
    token: &CancellationToken,
) -> Result<Vec<CodeActionOrCommand>, Cancelled> {
    token.check()?;
    let uri = &params.text_document.uri;
    let id = DocumentId::new(uri.as_str());
    let externals = importing_from(state, uri);
    // Both edits go where the syntax says imports are, so it has to be this
    // buffer's syntax.
    let Some(view) = current_view(state, &id) else {
        return Ok(Vec::new());
    };
    let mut actions = Vec::new();
    if let Some(externals) = externals.filter(|_| wanted(params, &CodeActionKind::QUICKFIX)) {
        for diagnostic in &params.context.diagnostics {
            token.check()?;
            actions.extend(add_import(&view, uri, &externals, diagnostic));
        }
    }
    if wanted(params, &CodeActionKind::SOURCE_ORGANIZE_IMPORTS) {
        actions.extend(remove_unused(&view, uri, token)?);
    }
    Ok(actions)
}

/// Whether the client asked for actions of `kind`: `only` names kinds by
/// prefix, `source` taking in `source.organizeImports`.
fn wanted(params: &CodeActionParams, kind: &CodeActionKind) -> bool {
    params.context.only.as_ref().is_none_or(|only| {
        only.iter().any(|asked| {
            kind.as_str()
                .strip_prefix(asked.as_str())
                .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
        })
    })
}

/// What the file may import from: its own module's classpath, as the
/// diagnostic judged it. `None` for a file no module claims, which was not
/// judged at all.
fn importing_from(state: &ServerState, uri: &Uri) -> Option<ClasspathExternals> {
    let externals = state.externals();
    let Some(Some(modules)) = state.whole_dependencies() else {
        return Some(externals);
    };
    let path = crate::workspace::path_of(uri.as_str())?;
    Some(externals.scoped(modules.scope_of(&path)?))
}

fn add_import(
    view: &FileView<'_>,
    uri: &Uri,
    externals: &ClasspathExternals,
    diagnostic: &Diagnostic,
) -> Vec<CodeActionOrCommand> {
    if diagnostic.code != Some(NumberOrString::String(UNRESOLVED_TYPE.to_owned())) {
        return Vec::new();
    }
    let start = view.document.offset_at(to_position(diagnostic.range.start)) as usize;
    let end = view.document.offset_at(to_position(diagnostic.range.end)) as usize;
    // An import's own name and a package-qualified name are dotted, and no
    // import makes either resolve.
    let Some(name) = view
        .document
        .text()
        .get(start..end)
        .filter(|name| is_simple(name))
    else {
        return Vec::new();
    };
    // The client's diagnostics may predate the buffer: an import typed since
    // makes the name resolve, and a second one for it does not compile.
    let package = view
        .syntax
        .package
        .as_ref()
        .map(|package| package.name.as_str());
    if externals.resolution(name, package, &view.syntax.imports) != Resolution::Unknown {
        return Vec::new();
    }
    let candidates = externals.importable(name);
    let only = candidates.len() == 1;
    candidates
        .into_iter()
        .map(|fqn| {
            CodeActionOrCommand::CodeAction(CodeAction {
                title: format!("Import `{fqn}`"),
                kind: Some(CodeActionKind::QUICKFIX),
                diagnostics: Some(vec![diagnostic.clone()]),
                edit: Some(edit_of(uri, vec![import_edit(view.syntax, &fqn)])),
                is_preferred: only.then_some(true),
                ..CodeAction::default()
            })
        })
        .collect()
}

/// One action deleting the line of every single-type import whose simple
/// name is written nowhere outside the imports — not as a name, not in a
/// comment's `{@link}`, not even in a string. A word in a string keeps an
/// import that could have gone; that is a missing answer, never a wrong one.
fn remove_unused(
    view: &FileView<'_>,
    uri: &Uri,
    token: &CancellationToken,
) -> Result<Option<CodeActionOrCommand>, Cancelled> {
    let text = view.document.text();
    let imports = &view.syntax.imports;
    // ponytail: a `\u` escape can spell a name the text search cannot see,
    // so such a file gets no action; the upgrade is searching the unescaped
    // text. A lone `\r` ends a line for the client and not for the line
    // index, so the line an edit names would be another; the upgrade is
    // `javelin-document`'s line index ending a line there too, as LSP does.
    if view.tree.has_errors()
        || text.contains("\\u")
        || text.split('\r').skip(1).any(|rest| !rest.starts_with('\n'))
    {
        return Ok(None);
    }
    let mut unseen: HashSet<&str> = imports.iter().filter_map(simple_of).collect();
    // One pass over everything but the import declarations themselves —
    // code, comments and strings, wherever they sit — striking each name
    // written there: work bounded by the file, once, and asked for on every
    // cursor move.
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    let mut from = 0;
    let bounds = imports
        .iter()
        .map(|import| {
            (
                import.span.start_byte as usize,
                import.span.end_byte as usize,
            )
        })
        .chain([(text.len(), text.len())]);
    for (start, end) in bounds {
        token.check()?;
        for word in text
            .get(from..start)
            .unwrap_or_default()
            .split(|c| !is_word(c))
        {
            unseen.remove(word);
        }
        if unseen.is_empty() {
            return Ok(None);
        }
        from = end;
    }
    let mut edits = Vec::new();
    for import in imports {
        if !simple_of(import).is_some_and(|simple| unseen.contains(simple)) {
            continue;
        }
        // The whole line, and only where the import is all it holds: a
        // second import or a comment beside it is not this edit's to delete.
        let line = import.span.range.start.line;
        let holds = text.lines().nth(line as usize).is_some_and(|written| {
            written.trim() == &text[import.span.start_byte as usize..import.span.end_byte as usize]
        });
        if import.span.range.end.line != line || !holds {
            continue;
        }
        edits.push(TextEdit::new(
            Range::new(Position::new(line, 0), Position::new(line + 1, 0)),
            String::new(),
        ));
    }
    if edits.is_empty() {
        return Ok(None);
    }
    Ok(Some(CodeActionOrCommand::CodeAction(CodeAction {
        title: "Remove unused imports".to_owned(),
        kind: Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS),
        edit: Some(edit_of(uri, edits)),
        ..CodeAction::default()
    })))
}

/// The simple name a single-type import brings in, where its name is written
/// as identifiers and dots alone: one with a space or a comment inside is
/// refused rather than read.
fn simple_of(import: &javelin_parser::ImportDecl) -> Option<&str> {
    (!import.is_static && !import.is_on_demand && import.name.split('.').all(is_simple))
        .then(|| import.name.rsplit('.').next())
        .flatten()
}

fn edit_of(uri: &Uri, edits: Vec<TextEdit>) -> WorkspaceEdit {
    WorkspaceEdit::new(HashMap::from([(uri.clone(), edits)]))
}

fn is_simple(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        && chars.all(|c| c.is_alphanumeric() || c == '_' || c == '$')
}

/// `import fqn;` where a reader looks for it: in name order among the file's
/// non-static imports, after them where none sorts later, before the static
/// ones where there are only those, after the package, or at the top.
fn import_edit(syntax: &FileSyntax, fqn: &str) -> TextEdit {
    let line = format!("import {fqn};");
    // At the import itself rather than its line's start, which may be inside
    // a block comment or before `package p;` written on the same line.
    let before = |import: &javelin_parser::ImportDecl| {
        let start = to_lsp_position(import.span.range.start);
        Range::new(start, start)
    };
    let plain: Vec<_> = syntax
        .imports
        .iter()
        .filter(|import| !import.is_static)
        .collect();
    if let Some(later) = plain.iter().find(|import| import.name.as_str() > fqn) {
        return TextEdit::new(before(later), format!("{line}\n"));
    }
    if let Some(last) = plain.last() {
        let end = to_lsp_position(last.span.range.end);
        return TextEdit::new(Range::new(end, end), format!("\n{line}"));
    }
    if let Some(first) = syntax.imports.first() {
        return TextEdit::new(before(first), format!("{line}\n\n"));
    }
    if let Some(package) = &syntax.package {
        let end = to_lsp_position(package.span.range.end);
        return TextEdit::new(Range::new(end, end), format!("\n\n{line}"));
    }
    TextEdit::new(Range::default(), format!("{line}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classpath::{Classpath, fact};
    use javelin_core::Config;

    const URI: &str = "file:///w/com/example/A.java";

    fn private(fqn: &str) -> javelin_classpath::TypeFacts {
        javelin_classpath::TypeFacts {
            is_public: false,
            ..fact(fqn)
        }
    }

    /// The titles offered for an unresolved-type warning over `name`, and
    /// the source after applying the first.
    fn fixes(
        source: &str,
        name: &str,
        facts: Vec<javelin_classpath::TypeFacts>,
    ) -> (Vec<String>, Option<String>) {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        state.install_classpath(Classpath::holding(facts));
        let document = javelin_document::TextDocument::new(DocumentId::new(URI), 1, source);
        let at = source.rfind(name).expect("name in source") as u32;
        let range = Range::new(
            to_lsp(document.position_at(at)),
            to_lsp(document.position_at(at + name.len() as u32)),
        );
        let params: CodeActionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "range": range,
            "context": { "diagnostics": [{
                "range": range, "severity": 2, "code": UNRESOLVED_TYPE,
                "source": "javelin", "message": "not a type"
            }], "only": ["quickfix"] },
        }))
        .expect("well-formed params");
        let actions =
            handle(&mut state, &params, &CancellationToken::new()).expect("not cancelled");
        let mut titles = Vec::new();
        let mut applied = None;
        for action in actions {
            let CodeActionOrCommand::CodeAction(action) = action else {
                panic!("a command");
            };
            titles.push(action.title);
            if applied.is_none() {
                let edit = &action.edit.expect("an edit").changes.expect("changes")
                    [&URI.parse::<lsp_types::Uri>().expect("uri")][0];
                let start = document.offset_at(to_position(edit.range.start)) as usize;
                let end = document.offset_at(to_position(edit.range.end)) as usize;
                let mut out = source.to_owned();
                out.replace_range(start..end, &edit.new_text);
                applied = Some(out);
            }
        }
        (titles, applied)
    }

    fn to_lsp(position: javelin_document::Position) -> Position {
        Position::new(position.line, position.character)
    }

    #[test]
    fn every_public_candidate_is_offered_and_sorted_into_the_imports() {
        let source = "package p;\n\nimport a.Alpha;\nimport z.Zed;\n\nclass A { List x; }\n";
        let (titles, applied) = fixes(
            source,
            "List",
            vec![
                fact("java.util.List"),
                fact("java.awt.List"),
                private("javax.q.List"),
            ],
        );
        assert_eq!(
            titles,
            ["Import `java.awt.List`", "Import `java.util.List`"]
        );
        assert_eq!(
            applied.as_deref(),
            Some(
                "package p;\n\nimport a.Alpha;\nimport java.awt.List;\nimport z.Zed;\n\nclass A { List x; }\n"
            )
        );
    }

    #[test]
    fn a_type_nested_in_a_non_public_one_is_not_offered() {
        let source = "class A { Entry e; }";
        let (titles, _) = fixes(
            source,
            "Entry",
            vec![
                private("java.q.Map"),
                fact("java.q.Map.Entry"),
                fact("java.util.Map"),
                fact("java.util.Map.Entry"),
            ],
        );
        assert_eq!(titles, ["Import `java.util.Map.Entry`"]);
    }

    #[test]
    fn jdk_internal_packages_are_not_offered() {
        let (titles, _) = fixes(
            "class A { Unsafe u; }",
            "Unsafe",
            vec![fact("sun.misc.Unsafe"), fact("jdk.internal.misc.Unsafe")],
        );
        assert!(titles.is_empty(), "{titles:?}");
        // A public type in a package no module exports, under a prefix no
        // denylist would name.
        let (titles, _) = fixes(
            "class A { Logger l; }",
            "Logger",
            vec![
                fact("java.util.logging.Logger"),
                fact("jdk.jfr.internal.Logger"),
            ],
        );
        assert_eq!(titles, ["Import `java.util.logging.Logger`"]);
    }

    #[test]
    fn the_import_lands_after_the_package_or_at_the_top_when_there_are_none() {
        let (_, applied) = fixes(
            "package p;\nclass A { List x; }",
            "List",
            vec![fact("java.util.List")],
        );
        assert_eq!(
            applied.as_deref(),
            Some("package p;\n\nimport java.util.List;\nclass A { List x; }")
        );
        let (_, applied) = fixes("class A { List x; }", "List", vec![fact("java.util.List")]);
        assert_eq!(
            applied.as_deref(),
            Some("import java.util.List;\n\nclass A { List x; }")
        );
        let (_, applied) = fixes(
            "import a.B;\nimport static z.Z.f;\nclass A { List x; }",
            "List",
            vec![fact("java.util.List")],
        );
        assert_eq!(
            applied.as_deref(),
            Some("import a.B;\nimport java.util.List;\nimport static z.Z.f;\nclass A { List x; }")
        );
    }

    #[test]
    fn the_import_goes_in_front_of_the_next_one_not_the_start_of_its_line() {
        let (_, applied) = fixes(
            "package p; import z.Zed;\nclass A { List x; }",
            "List",
            vec![fact("java.util.List")],
        );
        assert_eq!(
            applied.as_deref(),
            Some("package p; import java.util.List;\nimport z.Zed;\nclass A { List x; }")
        );
        let (_, applied) = fixes(
            "/* c\n */ import z.Zed;\nclass A { List x; }",
            "List",
            vec![fact("java.util.List")],
        );
        assert_eq!(
            applied.as_deref(),
            Some("/* c\n */ import java.util.List;\nimport z.Zed;\nclass A { List x; }")
        );
    }

    #[test]
    fn a_name_an_import_already_resolves_gets_nothing_from_a_stale_diagnostic() {
        let (titles, _) = fixes(
            "import java.awt.List;\nclass A { List x; }",
            "List",
            vec![fact("java.util.List"), fact("java.awt.List")],
        );
        assert!(titles.is_empty(), "{titles:?}");
    }

    #[test]
    fn other_diagnostics_and_dotted_names_get_nothing() {
        let (titles, _) = fixes(
            "import java.util.List;\nclass A {}",
            "java.util.List",
            vec![fact("java.util.List")],
        );
        assert!(titles.is_empty());
    }

    /// The source after the "Remove unused imports" action a request with
    /// `only` gets, or `None` where it gets none.
    fn organized(source: &str, only: Option<&[&str]>) -> Option<String> {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        let params: CodeActionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "range": Range::default(),
            "context": { "diagnostics": [], "only": only },
        }))
        .expect("well-formed params");
        let actions =
            handle(&mut state, &params, &CancellationToken::new()).expect("not cancelled");
        let document = javelin_document::TextDocument::new(DocumentId::new(URI), 1, source);
        actions.into_iter().find_map(|action| {
            let CodeActionOrCommand::CodeAction(action) = action else {
                panic!("a command");
            };
            (action.kind == Some(CodeActionKind::SOURCE_ORGANIZE_IMPORTS)).then(|| {
                let mut out = source.to_owned();
                let edits = &action.edit.expect("an edit").changes.expect("changes")
                    [&URI.parse::<lsp_types::Uri>().expect("uri")];
                for edit in edits.iter().rev() {
                    let start = document.offset_at(to_position(edit.range.start)) as usize;
                    let end = document.offset_at(to_position(edit.range.end)) as usize;
                    out.replace_range(start..end, &edit.new_text);
                }
                out
            })
        })
    }

    #[test]
    fn an_import_named_nowhere_else_goes_and_every_other_kind_stays() {
        let source = "package p;\n\nimport a.Used;\nimport a.Gone;\nimport a.Linked;\n\
            import a.Also;\nimport b.*;\nimport static c.C.f;\n\n\
            /** {@link Linked} */\nclass A { Used u; }\n";
        assert_eq!(
            organized(source, None).as_deref(),
            Some(
                "package p;\n\nimport a.Used;\nimport a.Linked;\nimport b.*;\n\
                 import static c.C.f;\n\n/** {@link Linked} */\nclass A { Used u; }\n"
            )
        );
    }

    #[test]
    fn a_package_annotation_named_before_the_imports_keeps_its_import() {
        let source = "@Marked\npackage p;\n\nimport a.Marked;\nimport a.Gone;\n";
        assert_eq!(
            organized(source, None).as_deref(),
            Some("@Marked\npackage p;\n\nimport a.Marked;\n")
        );
    }

    #[test]
    fn a_line_holding_more_than_the_import_is_left_alone() {
        for source in [
            "import a.One; import a.Two;\nclass A {}\n",
            "import a.One; // kept for the generator\nclass A {}\n",
        ] {
            assert_eq!(organized(source, None), None, "{source}");
        }
    }

    #[test]
    fn a_name_written_after_the_imports_or_between_them_keeps_its_import() {
        let source = "import a.Foo;\nclass A { Foo f; }\nimport a.Bar;\nclass B {}\n";
        assert_eq!(
            organized(source, None).as_deref(),
            Some("import a.Foo;\nclass A { Foo f; }\nclass B {}\n")
        );
    }

    #[test]
    fn a_file_the_text_cannot_be_trusted_on_gets_no_action() {
        for source in [
            "import a. Foo;\nclass A { java.util.List<Foo> f; }\n",
            "import a./*x*/Foo;\nclass A { Foo f; }\n",
            "/* x\r y */\nimport a.Used;\nimport a.Gone;\nclass A { Used u; }\n",
            "import a.Gone;\nclass A { void f( }\n",
            "import a.Gone;\nclass A { String s = \"\\u0041\"; }\n",
        ] {
            assert_eq!(organized(source, None), None, "{source}");
        }
    }

    #[test]
    fn the_action_comes_only_for_the_kinds_asked_for() {
        let source = "import a.Gone;\nclass A {}\n";
        assert!(organized(source, Some(&["source"])).is_some());
        assert!(organized(source, Some(&["source.organizeImports"])).is_some());
        assert!(organized(source, Some(&["quickfix"])).is_none());
        assert!(organized(source, Some(&["sourceX"])).is_none());
    }
}
