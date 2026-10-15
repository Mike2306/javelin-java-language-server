//! `textDocument/inlayHint`: a parameter's name before each argument of a
//! call, `substring(beginIndex: 0, endIndex: 3)`, and a type nobody wrote
//! down — after a `var`'s name, after an implicit lambda parameter, and at
//! the end of a multi-line chain's lines where it changes.
//!
//! The callee is the one definition picks — through `sole_candidate`, so a
//! call two overloads both fit is hinted by neither — and a call is hinted
//! only where the receiver's member list is whole and every parameter's name
//! is known: see `javelin_semantic::call_parameter_names_at`. A hint repeats nothing the argument says: an
//! argument already spelled like the parameter, `name` for `name` or
//! `getName()` for `name`, is left bare.
//!
//! A type is shown only where `javelin_semantic::hinted_type` vouches for it
//! — every member picked on the way from a list shown whole — and where no
//! name it was read through is one the resolver's tiers may have misread:
//! the gates `handlers::type_name_at` puts in front of the same resolver.
//!
//! Bounded by the range the client asks for, by a call count and by a
//! deadline, because the loop it runs on is single-threaded and a client may
//! ask for a whole generated file. Every hint is one job, worked in source
//! order, so a screen the deadline cuts short is cut at the bottom. What each
//! job found is kept per document until the document or anything it was read
//! against is replaced, so the next request resumes where this one stopped,
//! and a client that takes it is sent `workspace/inlayHint/refresh` to make
//! that next request: a cut screen fills in rather than waiting for a scroll.

use std::collections::HashMap;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use javelin_classpath::TypeWorld;
use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, TextDocument};
use javelin_index::{Survey, WorkspaceIndex};
use javelin_parser::{CallSite, Decl, DeclKind, Expr, LambdaParameter};
use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, InlayHintParams};

use crate::ServerState;
use crate::handlers::{to_lsp_position, to_position};

/// More calls than any screen holds.
const MAX_CALLS: usize = 500;

/// How long one request may resolve calls for — well inside the 50 ms an
/// interactive request is allowed, since the loop answers nothing else
/// meanwhile.
const DEADLINE: Duration = Duration::from_millis(20);

/// One hint to try, at the offset it would sit near.
enum Job<'a> {
    Call(CallSite),
    Var(&'a Decl),
    Lambda(&'a LambdaParameter),
    /// The index of a link in the chain list.
    Link(usize),
}

/// What a document's hints are read against besides its own text.
#[derive(Debug)]
pub(crate) struct Inputs {
    world: Weak<TypeWorld>,
    workspace: Weak<WorkspaceIndex>,
    survey: Option<Weak<Survey>>,
}

impl Inputs {
    pub(crate) fn new(
        world: Weak<TypeWorld>,
        workspace: Weak<WorkspaceIndex>,
        survey: Option<Weak<Survey>>,
    ) -> Self {
        Self {
            world,
            workspace,
            survey,
        }
    }

    fn same(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.world, &other.world)
            && Weak::ptr_eq(&self.workspace, &other.workspace)
            && match (&self.survey, &other.survey) {
                (Some(a), Some(b)) => Weak::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
    }
}

/// Every job's answer so far for one document: a call's parameter names, by
/// the offset of its name, and a type label by the kind and offset of its
/// job. A parameter hint is placed from the names per request, since which
/// arguments are hinted depends on the range asked for.
#[derive(Debug, Default)]
pub(crate) struct Memo {
    against: Option<(Weak<TextDocument>, Inputs)>,
    names: HashMap<u32, Option<Vec<String>>>,
    labels: HashMap<(u8, u32), Option<String>>,
}

impl Memo {
    /// Forget everything unless it was worked out against `document` and
    /// `inputs`.
    fn against(&mut self, document: &Arc<TextDocument>, inputs: Inputs) {
        let current = self.against.as_ref().is_some_and(|(known, known_inputs)| {
            std::ptr::eq(known.as_ptr(), Arc::as_ptr(document)) && known_inputs.same(&inputs)
        });
        if !current {
            *self = Memo {
                against: Some((Arc::downgrade(document), inputs)),
                ..Memo::default()
            };
        }
    }

    fn holds(&self, (kind, at): (u8, u32)) -> bool {
        if kind == CALL {
            self.names.contains_key(&at)
        } else {
            self.labels.contains_key(&(kind, at))
        }
    }
}

const CALL: u8 = 0;
const VAR: u8 = 1;
const LAMBDA: u8 = 2;
const LINK: u8 = 3;

pub fn handle(
    state: &mut ServerState,
    params: &InlayHintParams,
    token: &CancellationToken,
) -> Result<Vec<InlayHint>, Cancelled> {
    answer(state, params, token, DEADLINE)
}

/// The hints for the range, worked under `deadline` past what earlier
/// requests already worked out, owing the client a refresh when the
/// deadline leaves work undone.
fn answer(
    state: &mut ServerState,
    params: &InlayHintParams,
    token: &CancellationToken,
    deadline: Duration,
) -> Result<Vec<InlayHint>, Cancelled> {
    let id = DocumentId::new(params.text_document.uri.as_str());
    let mut memo = state.take_hint_memo(&id);
    let answered = hints(state, params, token, deadline, &mut memo);
    state.put_hint_memo(id, memo);
    let (hints, left) = answered?;
    match left {
        Left::Nothing => {}
        Left::Work => state.owe_hint_refresh(),
        Left::Parse => {
            state.owe_hint_refresh_after_parse(DocumentId::new(params.text_document.uri.as_str()))
        }
    }
    Ok(hints)
}

/// What a request left unanswered, and so what the client is owed.
enum Left {
    Nothing,
    /// The deadline stopped the walk with jobs left.
    Work,
    /// No view of the buffer's version was installed yet.
    Parse,
}

/// The hints, and what they leave unanswered.
fn hints(
    state: &mut ServerState,
    params: &InlayHintParams,
    token: &CancellationToken,
    deadline: Duration,
    memo: &mut Memo,
) -> Result<(Vec<InlayHint>, Left), Cancelled> {
    token.check()?;
    let id = DocumentId::new(params.text_document.uri.as_str());
    let externals = state.externals();
    let inputs = state.hint_inputs();
    let index = state.workspace();
    // A type is read through the resolver's tiers, which the workspace's
    // first walk is what makes whole — and a Kotlin, Groovy or Scala source
    // may declare a sibling it does not hold.
    let survey = state
        .survey()
        .filter(|survey| !survey.skipped_large && !survey.hidden && !survey.foreign);
    let current = state.documents.get(&id).map(|document| document.version());
    // An open document not parsed yet, or a view of an older version while a
    // reparse runs, would place every hint against text the client no longer
    // shows: answer nothing, and ask again once the reparse installs.
    let waiting = if current.is_some() {
        Left::Parse
    } else {
        Left::Nothing
    };
    let Some(view) = state.view(&id) else {
        return Ok((Vec::new(), waiting));
    };
    if current != Some(view.document.version()) {
        return Ok((Vec::new(), waiting));
    }
    memo.against(&view.document, inputs);
    // From here: the view is what every request pays for, built once per
    // version; the deadline bounds what this one adds.
    let started = Instant::now();
    let start = view.document.offset_at(to_position(params.range.start));
    let end = view.document.offset_at(to_position(params.range.end));
    let root = view.tree.root();
    let calls = javelin_parser::call_sites(root, start..end, MAX_CALLS, token)?;
    // Every other walk stops where the call walk's cap stopped it, so a
    // client asking for a whole generated file pays for a screenful.
    let bound = match calls.iter().map(|site| site.name).max() {
        Some(last) if calls.len() >= MAX_CALLS => last,
        _ => end,
    };
    // Inside a local or an anonymous class, a field it inherits shadows a
    // local around it and its own list misses what it inherits; and what
    // Lombok writes past what is modelled leaves this file's own types
    // short — the bodies and the files the member diagnostic leaves alone.
    let bodies =
        crate::diagnostics::local_and_anonymous_bodies(&view.syntax.declarations, &(start..bound));
    let inside = |at: u32| {
        bodies
            .iter()
            .any(|body| body.start_byte <= at && at < body.end_byte)
    };
    if lombok_short(view.syntax, view.document.text(), &externals, &id) {
        return Ok((Vec::new(), Left::Nothing));
    }
    let links = javelin_parser::chain_links(&view.document, root, start..bound, MAX_CALLS, token)?;
    let mut jobs: Vec<(u32, Job<'_>)> = calls
        .into_iter()
        .map(|site| (site.name, Job::Call(site)))
        .collect();
    let mut vars = Vec::new();
    locals(&view.syntax.declarations, &(start..bound), &mut vars);
    jobs.extend(
        vars.into_iter()
            .map(|decl| (decl.name_span.end_byte, Job::Var(decl))),
    );
    jobs.extend(
        view.syntax
            .lambda_parameters_in(start..bound)
            .iter()
            .map(|lambda| (lambda.name_span.end_byte, Job::Lambda(lambda))),
    );
    jobs.extend(
        links
            .iter()
            .enumerate()
            .filter(|(_, link)| (start..bound).contains(&link.end))
            .map(|(index, link)| (link.end, Job::Link(index))),
    );
    jobs.retain(|(at, _)| !inside(*at));
    jobs.sort_by_key(|(at, _)| *at);
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    let text = view.document.text();
    let mut shadow = HashMap::new();
    let mut claim = |expr: Option<&Expr>, lambda: Option<&LambdaParameter>| -> Option<String> {
        let hinted = match (expr, lambda) {
            (Some(expr), _) => javelin_semantic::hinted_type(expr, &files, &externals)?,
            (None, Some(lambda)) => {
                javelin_semantic::lambda_parameter_hint(lambda, &files, &externals)?
            }
            (None, None) => return None,
        };
        // A name read through the tiers needs the walk's word on it; a type
        // read through none, a primitive's, does not.
        let survey = match &survey {
            Some(survey) => survey,
            None if hinted.written.is_empty() => return Some(hinted.label),
            None => return None,
        };
        let misread = hinted.written.iter().any(|name| {
            survey.generated.contains(name)
                || *shadow
                    .entry(name.clone())
                    .or_insert_with(|| crate::handlers::shadowed(name, view.syntax, &externals))
        });
        (!misread && !names_a_super_builder_impl(&hinted.label, &index)).then_some(hinted.label)
    };
    let key = |job: &Job<'_>| match job {
        Job::Call(site) => (CALL, site.name),
        Job::Var(decl) => (VAR, decl.name_span.end_byte),
        Job::Lambda(lambda) => (LAMBDA, lambda.name_span.end_byte),
        Job::Link(index) => (LINK, links[*index].end),
    };
    let mut hints = Vec::new();
    // At least one job a request, so a request always moves the memo on
    // and a refresh it owes is never a request that does nothing.
    let mut fresh = 0usize;
    let mut left = Left::Nothing;
    for (_, job) in jobs {
        token.check()?;
        if !memo.holds(key(&job)) {
            if fresh > 0 && started.elapsed() > deadline {
                left = Left::Work;
                break;
            }
            fresh += 1;
        }
        match job {
            Job::Call(site) => {
                // A `new`'s constructor is held to a type hint's gates, since
                // its pick reads the type's name and its parameters'.
                let names = memo.names.entry(site.name).or_insert_with(|| {
                    javelin_semantic::call_parameter_names_at(
                        &view.document,
                        view.tree,
                        &files,
                        &externals,
                        site.name,
                    )
                    .or_else(|| {
                        crate::handlers::constructor_in(
                            &view,
                            &id,
                            &externals,
                            survey.as_deref(),
                            site.name,
                            true,
                            // `lombok_short` answered `false` above.
                            false,
                        )
                        .map(|found| found.parameter_names)
                        .filter(|names| !names.is_empty())
                    })
                });
                if let Some(names) = names {
                    parameter_hints(&site, names, &view, start..end, text, &mut hints);
                }
            }
            Job::Var(decl) => {
                let label = memo
                    .labels
                    .entry((VAR, decl.name_span.end_byte))
                    .or_insert_with(|| claim(decl.initializer.as_deref(), None))
                    .clone();
                if let Some(label) = label {
                    hints.push(type_hint(&view, decl.name_span.end_byte, label, false));
                }
            }
            Job::Lambda(lambda) => {
                let label = memo
                    .labels
                    .entry((LAMBDA, lambda.name_span.end_byte))
                    .or_insert_with(|| claim(None, Some(lambda)))
                    .clone();
                if let Some(label) = label {
                    hints.push(type_hint(&view, lambda.name_span.end_byte, label, false));
                }
            }
            Job::Link(index) => {
                let link = &links[index];
                let mut label_of = |index: usize| -> Option<String> {
                    memo.labels
                        .entry((LINK, links[index].end))
                        .or_insert_with(|| claim(Some(&links[index].call), None))
                        .clone()
                };
                let Some(label) = label_of(index) else {
                    continue;
                };
                // Shown where the type changes: the link before it in the
                // same chain, whether or not that one is in view.
                let before = index
                    .checked_sub(1)
                    .filter(|before| links[*before].chain == link.chain);
                if before.is_some_and(|before| label_of(before).as_ref() == Some(&label)) {
                    continue;
                }
                hints.push(type_hint(&view, link.end, label, true));
            }
        }
    }
    Ok((hints, left))
}

/// Whether `label` names the private `FooBuilderImpl` a `@SuperBuilder`'s
/// `builder()` is typed as, where Java's type is `FooBuilder<?, ?>`.
fn names_a_super_builder_impl(label: &str, index: &javelin_index::WorkspaceIndex) -> bool {
    label
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .filter(|name| name.ends_with("BuilderImpl"))
        .any(|name| index.with_simple_name(name).any(|ty| ty.synthetic))
}

/// Whether Lombok may write into this file's own types what
/// `lombok_members` does not — the gates the member diagnostic puts on
/// the unqualified calls — or `@ExtensionMethod` onto any receiver.
pub(crate) fn lombok_short(
    syntax: &javelin_parser::FileSyntax,
    text: &str,
    externals: &crate::classpath::ClasspathExternals,
    id: &DocumentId,
) -> bool {
    // Every rule below needs Lombok imported, and reading a generated
    // god-class's annotations is not free.
    //
    // ponytail: a Lombok annotation written fully qualified in a file
    // importing nothing of Lombok's is not seen. Scanning the text for it is
    // file-sized work on the request loop and misses the inlay deadline on a
    // god-class; the upgrade is the parser recording annotation names.
    if !syntax
        .imports
        .iter()
        .any(|import| import.name.starts_with("lombok"))
    {
        return false;
    }
    let live = syntax
        .named_types()
        .into_iter()
        .map(|named| javelin_classpath::lombok_members(named.decl, &[], &syntax.imports).lombok)
        .fold(
            javelin_classpath::LombokUse::default(),
            javelin_classpath::LombokUse::or,
        );
    let workspace = externals.workspace();
    let mut stored = crate::workspace::path_of(id.as_str())
        .map(|path| {
            workspace
                .types_in(&path)
                .map(|ty| ty.lombok)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .peekable();
    let held = stored.peek().is_some();
    let stored = stored.fold(
        javelin_classpath::LombokUse::default(),
        javelin_classpath::LombokUse::or,
    );
    let helper = text.contains("lombok.experimental.Helper")
        || text.contains("@Helper")
            && syntax
                .imports
                .iter()
                .any(|import| !import.is_static && import.name.starts_with("lombok.experimental"));
    live.extensions
        || stored.extensions
        || live.unmodelled
        || stored.unmodelled
        || (live.annotated && !held)
        || helper
}

/// Every `var` local named inside `within` whose initialiser does not write
/// its type down where it stands.
fn locals<'a>(decls: &'a [Decl], within: &std::ops::Range<u32>, out: &mut Vec<&'a Decl>) {
    // Iterative: a generated file nests lambdas and anonymous classes deep.
    let mut stack: Vec<&'a Decl> = decls.iter().collect();
    while let Some(decl) = stack.pop() {
        if decl.span.end_byte < within.start || decl.span.start_byte > within.end {
            continue;
        }
        if decl.kind == DeclKind::LocalVariable
            && decl.declared_type.as_deref() == Some("var")
            && within.contains(&decl.name_span.start_byte)
            && decl.initializer.as_deref().is_some_and(|initializer| {
                !matches!(
                    initializer,
                    Expr::New { .. } | Expr::Literal { .. } | Expr::Cast { .. }
                )
            })
        {
            out.push(decl);
        }
        stack.extend(decl.children.iter().chain(&decl.parameters));
    }
}

fn type_hint(view: &crate::state::FileView<'_>, at: u32, label: String, padded: bool) -> InlayHint {
    InlayHint {
        position: to_lsp_position(view.document.position_at(at)),
        label: InlayHintLabel::String(format!(": {label}")),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: None,
        padding_left: padded.then_some(true),
        padding_right: None,
        data: None,
    }
}

fn parameter_hints(
    site: &CallSite,
    names: &[String],
    view: &crate::state::FileView<'_>,
    within: std::ops::Range<u32>,
    text: &str,
    hints: &mut Vec<InlayHint>,
) {
    // Definition picked by arity, so the count fits: zipped, a variadic
    // tail is hinted at its first argument and no further.
    for (argument, name) in site.arguments.iter().zip(names) {
        // Only what is in the range asked for, so adjacent requests do
        // not answer one hint twice.
        if !within.contains(&argument.start) {
            continue;
        }
        let written = text
            .get(argument.start as usize..argument.end as usize)
            .unwrap_or_default();
        if says(written, name) {
            continue;
        }
        hints.push(InlayHint {
            position: to_lsp_position(view.document.position_at(argument.start)),
            label: InlayHintLabel::String(format!("{name}:")),
            kind: Some(InlayHintKind::PARAMETER),
            text_edits: None,
            tooltip: None,
            padding_left: None,
            padding_right: Some(true),
            data: None,
        });
    }
}

/// Whether the argument as written already says the parameter's name: its
/// last identifier is the name, or `get` or `is` before it, in any case.
fn says(written: &str, name: &str) -> bool {
    let trimmed = written.trim().trim_end_matches("()");
    let last = trimmed
        .rsplit(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
        .next()
        .unwrap_or(trimmed);
    let accessor = |prefix: &str| {
        last.strip_prefix(prefix)
            .filter(|rest| rest.chars().next().is_some_and(char::is_uppercase))
    };
    [Some(last), accessor("get"), accessor("is")]
        .into_iter()
        .flatten()
        .any(|spelled| spelled.eq_ignore_ascii_case(name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_core::Config;
    use javelin_parser::DeclKind;

    fn hints(source: &str) -> Vec<(u32, u32, String)> {
        hints_against(source, Vec::new())
    }

    fn hints_against(
        source: &str,
        facts: Vec<javelin_classpath::TypeFacts>,
    ) -> Vec<(u32, u32, String)> {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        if !facts.is_empty() {
            state.install_classpath(crate::classpath::Classpath::holding(facts));
        }
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        asked(&mut state, 0, 99)
    }

    const URI: &str = "file:///A.java";

    /// The hints the state answers for lines `from..to`.
    fn asked(state: &mut ServerState, from: u32, to: u32) -> Vec<(u32, u32, String)> {
        asked_under(state, from, to, DEADLINE)
    }

    fn asked_under(
        state: &mut ServerState,
        from: u32,
        to: u32,
        deadline: Duration,
    ) -> Vec<(u32, u32, String)> {
        let params: InlayHintParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "range": { "start": { "line": from, "character": 0 }, "end": { "line": to, "character": 0 } },
        }))
        .expect("params");
        answer(state, &params, &CancellationToken::new(), deadline)
            .expect("not cancelled")
            .into_iter()
            .map(|hint| {
                let InlayHintLabel::String(label) = hint.label else {
                    panic!("a string label");
                };
                (hint.position.line, hint.position.character, label)
            })
            .collect()
    }

    #[test]
    fn a_calls_arguments_are_hinted_with_the_parameters_names() {
        let source = "class A {\n    void move(int x, int y) {}\n    \
                      void m(int x) { move(1, 2); move(x, getY()); }\n    int getY() { return 0; }\n}\n";
        assert_eq!(
            hints(source),
            vec![(2, 25, "x:".to_owned()), (2, 28, "y:".to_owned())],
            "`x` and `getY()` already say their parameters"
        );
    }

    /// A call two overloads fit is definition's refusal, and so this one's;
    /// a call with the wrong count of arguments is hinted by none.
    #[test]
    fn a_call_definition_cannot_pick_is_not_hinted() {
        let source = "class A {\n    void f(Object a) {}\n    void f(String b) {}\n    \
                      void g(int c) {}\n    void m(Object o) { f(null); g(1, 2); }\n}\n";
        assert_eq!(hints(source), Vec::new());
    }

    /// Error recovery can leave a parameter with a type and no name: no
    /// parameter of that method is named, rather than one hinted `:`.
    #[test]
    fn a_method_with_a_nameless_parameter_is_not_hinted() {
        let source = "class A {\n    void f(int a, String) {}\n    void m() { f(1, \"s\"); }\n}\n";
        assert_eq!(hints(source), Vec::new());
    }

    #[test]
    fn a_variadic_tail_is_hinted_at_its_first_argument() {
        let source = "class A {\n    void log(String format, Object... args) {}\n    \
                      void m() { log(\"a\", 1, 2); log(\"b\"); }\n}\n";
        assert_eq!(
            hints(source)
                .into_iter()
                .map(|(_, _, label)| label)
                .collect::<Vec<_>>(),
            vec!["format:", "args:", "format:"]
        );
    }

    /// A classpath method is hinted by the names its facts hold, and one
    /// read from a class file with no sources — no names — not at all.
    #[test]
    fn a_classpath_method_is_hinted_only_where_its_facts_name_every_parameter() {
        use javelin_classpath::{MemberFacts, MemberType, TypeFacts};
        let int = || MemberType::Primitive("int".to_owned());
        let method = |name: &str, names: &[&str]| MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            name: name.to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: MemberType::Class("java.lang.String".to_owned()),
            parameters: vec![int(), int()],
            parameter_names: names.iter().map(|name| (*name).to_owned()).collect(),
            variadic: false,
            name_start: 0,
            name_end: 0,
        };
        let string = TypeFacts {
            is_public: true,
            constructors: Vec::new(),
            doc_span: None,
            fqn: "java.lang.String".to_owned(),
            simple: "String".to_owned(),
            kind: DeclKind::Class,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: Vec::new(),
            supertypes_complete: true,
            members_complete: true,
            members: vec![
                method("substring", &["beginIndex", "endIndex"]),
                method("region", &[]),
                // A name short of the parameters is no name for any of them.
                method("slice", &["only"]),
            ],
            source: String::new(),
            name_start: 0,
            name_end: 0,
        };
        let source = "class A {\n    void m(String s) { s.substring(0, 3); s.region(1, 2); s.slice(4, 5); }\n}\n";
        assert_eq!(
            hints_against(source, vec![string])
                .into_iter()
                .map(|(_, _, label)| label)
                .collect::<Vec<_>>(),
            vec!["beginIndex:", "endIndex:"]
        );
    }

    /// A supertype nothing resolves could declare the overload Java picks,
    /// so a list that cannot be shown whole hints nothing; and `Object`'s
    /// own methods, which no file-local list holds, are never hinted.
    #[test]
    fn a_call_on_a_list_that_may_be_short_is_not_hinted() {
        let open = "class Client {\n    void send(Object payload) {}\n    \
                    void m(Object o) { send(\"x\"); equals(o); }\n}\n";
        assert_eq!(
            hints(open)
                .into_iter()
                .map(|(_, _, label)| label)
                .collect::<Vec<_>>(),
            vec!["payload:"]
        );
        let extending = "class Client extends com.lib.Base {\n    void send(Object payload) {}\n    \
                         void m() { send(\"x\"); }\n}\n";
        assert_eq!(hints(extending), Vec::new());
    }

    /// While a reparse is in flight the view is of older text; hints placed
    /// against it would sit in the wrong place in the buffer the client has.
    /// The client is asked back once the reparse installs, and not before,
    /// when it would only meet the same old view.
    #[test]
    fn a_view_older_than_the_buffer_answers_nothing_until_its_reparse_installs() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.refreshes_hints();
        let id = DocumentId::new(URI);
        let source = "class A {\n    void f(int a) {}\n    void m() { f(1); }\n}\n";
        state.did_open(id.clone(), 1, source.to_owned());
        assert_eq!(asked(&mut state, 0, 99).len(), 1, "the premise");
        state.did_change(
            &id,
            2,
            &[javelin_document::Change::full(format!(
                "// moved\n{source}"
            ))],
        );
        assert_eq!(asked(&mut state, 0, 99), Vec::new());
        assert!(!state.take_hint_refresh(), "not while the reparse runs");
        state.settle();
        assert!(state.take_hint_refresh(), "owed once it installs");
        assert_eq!(asked(&mut state, 0, 99), vec![(3, 17, "a:".to_owned())]);
        state.settle();
        assert!(!state.take_hint_refresh(), "and owed once");
    }

    /// A call spanning lines is hinted only on the lines asked for.
    #[test]
    fn only_arguments_inside_the_range_are_hinted() {
        let source = "class A {\n    void f(int a, int b) {}\n    void m() {\n        f(1,\n          2);\n    }\n}\n";
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        assert_eq!(asked(&mut state, 4, 5), vec![(4, 10, "b:".to_owned())]);
    }

    /// `issue` does not spell `sue`; `getName()` spells `name`.
    #[test]
    fn an_accessor_prefix_counts_only_before_an_upper_case_letter() {
        assert!(!says("issue", "sue"));
        assert!(says("x.getName()", "name") && says("isOpen", "open") && says("name", "name"));
    }

    /// A local `equals(Foo)` beside the `Object.equals(Object)` no local list
    /// holds: Java calls `Object`'s for an `Object` argument, so `Object`'s
    /// method names are hinted never.
    #[test]
    fn a_method_object_also_declares_is_not_hinted() {
        let source = "class Foo {\n    boolean equals(Foo other) { return true; }\n    \
                      void m(Object o) { equals(o); }\n}\n";
        assert_eq!(hints(source), Vec::new());
    }

    /// A supertype the world holds without all of its members: what it
    /// lacks could be the overload Java picks.
    #[test]
    fn a_call_through_an_incomplete_external_supertype_is_not_hinted() {
        use javelin_classpath::TypeFacts;
        let base = |members_complete| TypeFacts {
            is_public: true,
            constructors: Vec::new(),
            doc_span: None,
            fqn: "com.lib.Base".to_owned(),
            simple: "Base".to_owned(),
            kind: DeclKind::Class,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: Vec::new(),
            supertypes_complete: true,
            members_complete,
            members: Vec::new(),
            source: String::new(),
            name_start: 0,
            name_end: 0,
        };
        let source = "class Client extends com.lib.Base {\n    void send(Object payload) {}\n    \
                      void m() { send(\"x\"); }\n}\n";
        assert_eq!(
            hints_against(source, vec![base(true)]).len(),
            1,
            "the premise"
        );
        assert_eq!(hints_against(source, vec![base(false)]), Vec::new());
    }

    // ---- Tier 3, slice 1: type hints --------------------------------------

    use javelin_classpath::{MemberFacts, MemberType, TypeFacts};

    fn method_fact(name: &str, declared: MemberType, parameters: Vec<MemberType>) -> MemberFacts {
        MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            name: name.to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared,
            parameter_names: parameters.iter().map(|_| String::new()).collect(),
            parameters,
            variadic: false,
            name_start: 0,
            name_end: 0,
        }
    }

    fn type_fact(fqn: &str, type_parameters: &[&str], members: Vec<MemberFacts>) -> TypeFacts {
        TypeFacts {
            is_public: true,
            constructors: Vec::new(),
            doc_span: None,
            fqn: fqn.to_owned(),
            simple: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
            kind: DeclKind::Interface,
            type_parameters: type_parameters
                .iter()
                .map(|name| (*name).to_owned())
                .collect(),
            supertype_arguments: Vec::new(),
            supertypes: Vec::new(),
            supertypes_complete: true,
            members_complete: true,
            members,
            source: String::new(),
            name_start: 0,
            name_end: 0,
        }
    }

    /// `String` with `trim()` and a `split()` answering `List<String>`, a
    /// generic `List` and a `Consumer`.
    fn typed_world() -> Vec<TypeFacts> {
        let string = || MemberType::Class("java.lang.String".to_owned());
        let element = || MemberType::Variable("E".to_owned());
        let mut accept = method_fact(
            "accept",
            MemberType::Void,
            vec![MemberType::Variable("T".to_owned())],
        );
        accept.is_abstract = true;
        vec![
            type_fact(
                "java.lang.String",
                &[],
                vec![
                    method_fact("trim", string(), Vec::new()),
                    method_fact(
                        "split",
                        MemberType::Parameterized("java.util.List".to_owned(), vec![string()]),
                        Vec::new(),
                    ),
                ],
            ),
            type_fact(
                "java.util.List",
                &["E"],
                vec![
                    method_fact(
                        "get",
                        element(),
                        vec![MemberType::Primitive("int".to_owned())],
                    ),
                    method_fact("size", MemberType::Primitive("int".to_owned()), Vec::new()),
                    method_fact(
                        "forEach",
                        MemberType::Void,
                        vec![MemberType::Parameterized(
                            "java.util.function.Consumer".to_owned(),
                            vec![MemberType::Super(Box::new(element()))],
                        )],
                    ),
                ],
            ),
            type_fact("java.util.function.Consumer", &["T"], vec![accept]),
            // Without it no closure of the file's own types is whole, and
            // every name is one an inherited member type may mean.
            type_fact("java.lang.Object", &[], Vec::new()),
        ]
    }

    /// The type hints for `source`, with the workspace walked and `survey`
    /// what it saw.
    fn type_hints_with(
        source: &str,
        survey: Option<javelin_index::Survey>,
    ) -> Vec<(u32, u32, String)> {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.install_classpath(crate::classpath::Classpath::holding(typed_world()));
        surveyed(&mut state, survey);
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        typed_only(asked(&mut state, 0, 99))
    }

    fn surveyed(state: &mut ServerState, survey: Option<javelin_index::Survey>) {
        state.install_workspace(crate::workspace::Workspace {
            index: state.workspace(),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: survey.map(std::sync::Arc::new),
            absorbed: 0,
        });
    }

    fn typed_only(hints: Vec<(u32, u32, String)>) -> Vec<(u32, u32, String)> {
        hints
            .into_iter()
            .filter(|(_, _, label)| label.starts_with(": "))
            .collect()
    }

    fn type_hints(source: &str) -> Vec<(u32, u32, String)> {
        type_hints_with(source, Some(javelin_index::Survey::default()))
    }

    /// A `new` is hinted with the names of the constructor it selects, and
    /// a call two constructors fit is hinted by neither.
    #[test]
    fn a_new_is_hinted_with_its_constructors_parameter_names() {
        let hints = |source: &str| {
            let mut state = ServerState::new(Config::default()).expect("grammar loads");
            state.install_workspace(crate::workspace::Workspace {
                index: state.workspace(),
                elapsed: std::time::Duration::ZERO,
                refreshed: false,
                survey: Some(std::sync::Arc::default()),
                absorbed: 0,
            });
            state.did_open(DocumentId::new(URI), 1, source.to_owned());
            asked(&mut state, 0, 99)
        };
        let source = "class Box {\n    Box(int size) {}\n    Box(int width, int height) {}\n    Object m(int width) { return new Box(width, 2); }\n}\n";
        assert_eq!(hints(source), vec![(3, 48, "height:".to_owned())]);
        let two = source
            .replace("Box(int size)", "Box(String width, int height)")
            .replace("new Box(width, 2)", "new Box(unknown, 2)");
        assert_eq!(hints(&two), Vec::new());
        // `super(…)` and an enum constant are constructor calls too.
        let sup = "class Box {\n    Box(int width, int height) {}\n}\n\
                   class Crate extends Box {\n    Crate(int width) { super(width, 2); }\n}\n";
        assert_eq!(hints(sup), vec![(4, 36, "height:".to_owned())]);
        let constant = "enum Day {\n    MON(1);\n    Day(int hours) {}\n}\n";
        assert_eq!(hints(constant), vec![(1, 8, "hours:".to_owned())]);
    }

    const TYPED: &str = "import java.util.List;\nclass A {\n    void m(String s, List<String> l) {\n        \
                         var t = s.trim();\n        var n = new A();\n        var k = 1;\n        \
                         l.forEach(x -> x.trim());\n    }\n}\n";

    #[test]
    fn a_var_and_an_implicit_lambda_parameter_are_hinted_with_their_types() {
        assert_eq!(
            type_hints(TYPED),
            vec![
                (3, 13, ": String".to_owned()),
                (6, 19, ": String".to_owned())
            ],
            "`new A()` and `1` say their types where they stand"
        );
    }

    /// A request the deadline stops early still works one job, keeps what
    /// it worked out and owes a client that takes one a refresh; each request
    /// that refresh brings resumes where the last stopped, until the whole
    /// range is answered and nothing more is owed.
    #[test]
    fn a_request_the_deadline_cuts_short_is_resumed_by_the_next() {
        let opened = |refreshes: bool| {
            let mut state = ServerState::new(Config::default()).expect("grammar loads");
            state.install_classpath(crate::classpath::Classpath::holding(typed_world()));
            surveyed(&mut state, Some(javelin_index::Survey::default()));
            if refreshes {
                state.refreshes_hints();
            }
            state.did_open(DocumentId::new(URI), 1, TYPED.to_owned());
            state
        };
        let mut silent = opened(false);
        asked_under(&mut silent, 0, 99, Duration::ZERO);
        assert!(
            !silent.take_hint_refresh(),
            "a client that takes no refresh is sent none"
        );

        let mut state = opened(true);
        let first = asked_under(&mut state, 0, 99, Duration::ZERO);
        let mut last = first.clone();
        let mut rounds = 1;
        while state.take_hint_refresh() {
            assert!(
                !state.take_hint_refresh(),
                "one refresh waits for its answer"
            );
            state.hint_refresh_answered();
            last = asked_under(&mut state, 0, 99, Duration::ZERO);
            rounds += 1;
            assert!(rounds < 20, "every request moves the memo on");
        }
        assert!(rounds > 2, "{rounds} rounds");
        assert!(typed_only(first).len() < 2);
        assert_eq!(typed_only(last), type_hints(TYPED));
    }

    /// What a hint read before the JDK's facts arrived is forgotten when
    /// they do, so a type unknown then is hinted now.
    #[test]
    fn a_world_arriving_forgets_the_hints_worked_out_without_it() {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        surveyed(&mut state, Some(javelin_index::Survey::default()));
        state.did_open(DocumentId::new(URI), 1, TYPED.to_owned());
        assert_eq!(typed_only(asked(&mut state, 0, 99)), Vec::new());
        state.install_classpath(crate::classpath::Classpath::holding(typed_world()));
        assert_eq!(typed_only(asked(&mut state, 0, 99)), type_hints(TYPED));
    }

    /// Before the workspace's first walk a sibling may outrank what a name
    /// resolved to; a static import may mean another `String`.
    #[test]
    fn a_type_read_through_a_name_that_may_mean_another_is_not_hinted() {
        assert_eq!(type_hints_with(TYPED, None), Vec::new());
        // A primitive reads no name, so it needs no walk's word.
        let primitive = "class A {\n    void m(int x) {\n        var v = x;\n    }\n}\n";
        assert_eq!(
            type_hints_with(primitive, None),
            vec![(2, 13, ": int".to_owned())]
        );
        let imported = TYPED.replace(
            "import java.util.List;",
            "import java.util.List;\nimport static p.Q.String;",
        );
        assert_eq!(type_hints(&imported), Vec::new());
        let mut survey = javelin_index::Survey::default();
        survey.generated.insert("String".to_owned());
        assert_eq!(type_hints_with(TYPED, Some(survey)), Vec::new());
    }

    /// A chain is hinted at a line's end where its type changes, never at
    /// its last link; one break is a wrapped line, not a chain.
    #[test]
    fn a_multi_line_chain_is_hinted_where_its_type_changes() {
        let source = "class A {\n    int m(String s) {\n        return s.trim()\n            .trim()\n            \
                      .split()\n            .get(0)\n            .length();\n    }\n}\n";
        assert_eq!(
            type_hints(source),
            vec![
                (2, 23, ": String".to_owned()),
                (4, 20, ": List<String>".to_owned()),
                (5, 19, ": String".to_owned()),
            ]
        );
        let wrapped = "class A {\n    String m(String s) {\n        return s.trim()\n            .trim();\n    }\n}\n";
        assert_eq!(type_hints(wrapped), Vec::new());
        // A chain's first link is never compared with the last of the chain
        // before it.
        let two = "class A {\n    void m(String s) {\n        s.trim()\n            .trim()\n            \
                   .split();\n        s.trim()\n            .trim()\n            .split();\n    }\n}\n";
        assert_eq!(
            type_hints(two),
            vec![
                (2, 16, ": String".to_owned()),
                (5, 16, ": String".to_owned())
            ]
        );
    }

    /// A hint inside the range is compared with the link before it even
    /// when that one is out of view, so scrolling does not make one appear.
    #[test]
    fn a_chain_link_is_compared_with_the_one_before_it_out_of_view() {
        let source = "class A {\n    int m(String s) {\n        return s.trim()\n            .trim()\n            \
                      .split()\n            .size();\n    }\n}\n";
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.install_classpath(crate::classpath::Classpath::holding(typed_world()));
        state.install_workspace(crate::workspace::Workspace {
            index: state.workspace(),
            elapsed: std::time::Duration::ZERO,
            refreshed: false,
            survey: Some(std::sync::Arc::default()),
            absorbed: 0,
        });
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        assert_eq!(
            asked(&mut state, 3, 4),
            Vec::new(),
            "`.trim()` repeats `s.trim()`"
        );
    }

    /// Inside a local or an anonymous class, an inherited field may shadow
    /// the local a name reads, and the class's own list is short.
    #[test]
    fn nothing_inside_a_local_or_anonymous_class_is_hinted() {
        let outside = "class A {\n    void m(String x) {\n        var v = x;\n    }\n}\n";
        assert_eq!(type_hints(outside), vec![(2, 13, ": String".to_owned())]);
        let anonymous = "class A {\n    void m(String x) {\n        new Object() {\n            \
                         void g() {\n                var v = x;\n            }\n        };\n    }\n}\n";
        assert_eq!(type_hints(anonymous), Vec::new());
        let local = "class A {\n    void m(String x) {\n        class L {\n            \
                     void g() {\n                var v = x;\n            }\n        }\n    }\n}\n";
        assert_eq!(type_hints(local), Vec::new());
    }

    /// An annotation Lombok's model does not cover may write a member this
    /// file's own types are read without.
    #[test]
    fn a_file_lombok_writes_more_into_than_is_modelled_is_not_hinted() {
        let plain = "class A {\n    String d;\n    void m(String s) {\n        var t = s.trim();\n    }\n}\n";
        assert_eq!(type_hints(plain), vec![(3, 13, ": String".to_owned())]);
        let delegated = format!(
            "import lombok.experimental.Delegate;\n{}",
            plain.replace("    String d;", "    @Delegate String d;")
        );
        assert_eq!(type_hints(&delegated), Vec::new());
    }

    /// A Kotlin, Groovy or Scala source may declare a sibling the index does
    /// not hold, so a named type waits; a primitive does not.
    #[test]
    fn a_world_with_foreign_sources_hints_only_primitives() {
        let survey = javelin_index::Survey {
            foreign: true,
            ..Default::default()
        };
        assert_eq!(type_hints_with(TYPED, Some(survey.clone())), Vec::new());
        let primitive = "class A {\n    void m(int x) {\n        var v = x;\n    }\n}\n";
        assert_eq!(
            type_hints_with(primitive, Some(survey)),
            vec![(2, 13, ": int".to_owned())]
        );
    }
}
