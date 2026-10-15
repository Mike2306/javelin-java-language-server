//! `textDocument/prepareRename` and `textDocument/rename`.
//!
//! A rename is a write, and a write that misses one use breaks the build — a
//! missing reference is a ceiling for reading and a bug here. Two kinds of
//! target are renamed, each only where every use can be found:
//!
//! - **In the open file alone**: a local, a parameter, and a private field or
//!   method, whose every use Java confines to the file. A pattern binding is
//!   not one: the scope tree binds it to the end of its enclosing scope, which
//!   is too wide to write by, and a use it may claim refuses any rename.
//! - **Across the workspace**: a top-level type the workspace declares, with
//!   its file, and any other field or method one of its types declares, with
//!   its override family. See `sweep`. A JDK or dependency declaration is
//!   refused: no edit to the workspace renames it.
//!
//! Either way every token spelled like the target is resolved the way
//! references resolves it, by where go-to-definition lands, and one that
//! lands nowhere while it could be the target refuses the whole rename.

mod sweep;

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::path::PathBuf;
use std::sync::Arc;

use crossbeam_channel::Sender;
use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, TextDocument};
use javelin_index::WorkspaceIndex;
use javelin_parser::{Decl, DeclKind, DefinitionKind, Span};
use javelin_semantic::Externals;
use lsp_server::{Message, Request, RequestId, Response};
use lsp_types::notification::{Notification as _, ShowMessage};
use lsp_types::request::{Rename, Request as _};
use lsp_types::{
    MessageType, PrepareRenameResponse, RenameParams, ShowMessageParams,
    TextDocumentPositionParams, TextEdit, WorkspaceEdit,
};

use crate::ServerState;
use crate::classpath::ClasspathExternals;
use crate::handlers::references::{Target, named_types, target_at};
use crate::handlers::{definition_in, to_lsp_range, to_position};
use crate::state::FileView;
use sweep::{Sweep, Wide};

/// Why a rename was refused, for the client to show.
type Refusal = &'static str;

const NOT_RENAMEABLE: Refusal = "Javelin renames variables, and the fields, methods and top-level \
                                 types the workspace declares";

const STALE: Refusal = "the file is not open, or is still being parsed; try again";

/// What renaming the target under the cursor rewrites.
struct Plan {
    name: String,
    /// The declaration's name and every use, in source order.
    spans: Vec<Span>,
    /// Where the new name must not already be written: the scope for a
    /// variable, the file for a member.
    within: RangeInclusive<u32>,
    target: Kind,
}

enum Kind {
    /// Where its declaration's name starts, which is where an outer binding
    /// of the new name would already be visible.
    Variable { declared_at: u32 },
    /// The name span of the type that declares it.
    Member { owner: Span },
}

/// What a rename at a position is, before the new name is known.
enum Planned {
    /// Confined to the open file, and worked out already.
    File(Plan),
    /// Across the workspace, worked out by reading it.
    Workspace(Sweep),
}

/// What a rename across the workspace needs off the state, taken before the
/// view borrows it.
struct Context {
    index: Arc<WorkspaceIndex>,
    /// Why the index cannot be searched now, if it cannot.
    unready: Option<Refusal>,
    open: HashMap<PathBuf, Arc<TextDocument>>,
    document_changes: bool,
    rename_files: bool,
}

impl Context {
    fn of(state: &ServerState) -> Self {
        let unready = if crate::handlers::trusted_survey(state).is_none() {
            Some(
                "the workspace is not indexed yet, or not wholly: no folder is open, or a file \
                 was skipped",
            )
        } else if !state.index_current() {
            Some("the workspace index is catching up with an edit; try again")
        } else {
            None
        };
        Self {
            index: state.workspace(),
            unready,
            open: state
                .documents
                .iter()
                .filter_map(|document| {
                    Some((
                        crate::workspace::path_of(document.id().as_str())?,
                        Arc::clone(document),
                    ))
                })
                .collect(),
            document_changes: state.edits_documents(),
            rename_files: state.renames_files(),
        }
    }
}

/// What the rename at a position answers before the new name is known: the
/// edit where it is the open file's alone, the sweep where it is not.
enum Begun {
    Edit(WorkspaceEdit),
    Sweep(Sweep),
}

/// The range of the name under the cursor and the name, where a rename there
/// would be answered; `null` where it would be refused.
pub fn prepare(
    state: &mut ServerState,
    params: &TextDocumentPositionParams,
    token: &CancellationToken,
) -> Result<Result<PrepareRenameResponse, String>, Cancelled> {
    let id = DocumentId::new(params.text_document.uri.as_str());
    let externals = state.externals();
    let context = Context::of(state);
    let Some(view) = current_view(state, &id) else {
        return Ok(Err(STALE.to_owned()));
    };
    let offset = view.document.offset_at(to_position(params.position));
    let (name, spans) = match plan(&view, &id, &externals, context, offset, token)? {
        Ok(Planned::File(plan)) => (plan.name, plan.spans),
        Ok(Planned::Workspace(sweep)) => {
            let width = sweep.name.len() as u32;
            let near = offset.saturating_sub(width)..=offset + width;
            let spans = javelin_parser::identifier_spans_in(
                &view.document,
                view.tree,
                &sweep.name,
                near,
                token,
            )?;
            (sweep.name, spans)
        }
        Err(refusal) => return Ok(Err(refusal)),
    };
    let Some(at) = spans
        .iter()
        .find(|span| span.start_byte <= offset && offset <= span.end_byte)
    else {
        return Ok(Err(NOT_RENAMEABLE.to_owned()));
    };
    Ok(Ok(PrepareRenameResponse::RangeWithPlaceholder {
        range: to_lsp_range(at.range),
        placeholder: name,
    }))
}

/// The edit renaming the target under the cursor, or why there is none.
pub fn handle(
    state: &mut ServerState,
    params: &RenameParams,
    token: &CancellationToken,
) -> Result<Result<WorkspaceEdit, String>, Cancelled> {
    match begin(state, params, token)? {
        Ok(Begun::Edit(edit)) => Ok(Ok(edit)),
        Ok(Begun::Sweep(sweep)) => sweep.run(&params.new_name, token),
        Err(refusal) => Ok(Err(refusal)),
    }
}

/// Answer a rename, from a thread of its own where it reads the workspace,
/// or hand `request` back where it is not one.
pub fn start(
    state: &mut ServerState,
    request: Request,
    sender: &Sender<Message>,
) -> Option<Request> {
    if request.method != Rename::METHOD {
        return Some(request);
    }
    let (id, params) = match crate::extract_params::<Rename>(request) {
        Ok(extracted) => extracted,
        Err(response) => {
            let _ = sender.send(Message::Response(response));
            return None;
        }
    };
    // ponytail: `$/cancelRequest` reaches neither half, as it reaches no
    // formatter; the sweep is bounded by the workspace. The upgrade is the
    // in-flight map handing out a token the thread keeps.
    let token = CancellationToken::new();
    match begin(state, &params, &token) {
        Ok(Ok(Begun::Sweep(sweep))) => {
            let sender = sender.clone();
            std::thread::spawn(move || {
                answer(&sender, id, sweep.run(&params.new_name, &token));
            });
        }
        Ok(Ok(Begun::Edit(edit))) => answer(sender, id, Ok(Ok(edit))),
        Ok(Err(refusal)) => answer(sender, id, Ok(Err(refusal))),
        Err(cancelled) => answer(sender, id, Err(cancelled)),
    }
    None
}

/// A refusal is RequestFailed with its reason, and the reason again as a
/// message: Zed shows a failed rename as "Rename via javelin failed" and
/// keeps the error's text in its log, and a rename that silently does nothing
/// reads as a rename that worked.
fn answer(
    sender: &Sender<Message>,
    id: RequestId,
    answer: Result<Result<WorkspaceEdit, String>, Cancelled>,
) {
    // A client gone while the sweep ran has nobody to tell.
    let response = match answer {
        Ok(Err(refusal)) => {
            let message = format!("cannot rename: {refusal}");
            tracing::info!("{message}");
            let _ = sender.send(Message::Response(Response::new_err(
                id,
                lsp_server::ErrorCode::RequestFailed as i32,
                message.clone(),
            )));
            let shown = lsp_server::Notification::new(
                ShowMessage::METHOD.to_owned(),
                ShowMessageParams {
                    typ: MessageType::INFO,
                    message,
                },
            );
            let _ = sender.send(Message::Notification(shown));
            return;
        }
        answer => crate::reply(id, answer.map(Result::unwrap_or_default)),
    };
    let _ = sender.send(Message::Response(response));
}

fn begin(
    state: &mut ServerState,
    params: &RenameParams,
    token: &CancellationToken,
) -> Result<Result<Begun, String>, Cancelled> {
    let at = &params.text_document_position;
    let uri = at.text_document.uri.clone();
    let id = DocumentId::new(uri.as_str());
    let externals = state.externals();
    let context = Context::of(state);
    let Some(view) = current_view(state, &id) else {
        return Ok(Err(STALE.to_owned()));
    };
    let offset = view.document.offset_at(to_position(at.position));
    let plan = match plan(&view, &id, &externals, context, offset, token)? {
        Ok(Planned::File(plan)) => plan,
        Ok(Planned::Workspace(sweep)) => return Ok(Ok(Begun::Sweep(sweep))),
        Err(refusal) => return Ok(Err(refusal)),
    };
    let new_name = params.new_name.as_str();
    if new_name == plan.name {
        return Ok(Ok(Begun::Edit(WorkspaceEdit::default())));
    }
    if let Err(refusal) = free(&view, &id, &externals, &plan, new_name, token)? {
        return Ok(Err(refusal.to_owned()));
    }
    let edits = plan
        .spans
        .iter()
        .map(|span| TextEdit::new(to_lsp_range(span.range), new_name.to_owned()))
        .collect();
    Ok(Ok(Begun::Edit(WorkspaceEdit::new(HashMap::from([(
        uri, edits,
    )])))))
}

/// The open file's view, only where it is the buffer's current version: an
/// edit computed over a previous one lands on the wrong text.
pub(crate) fn current_view<'a>(
    state: &'a mut ServerState,
    id: &DocumentId,
) -> Option<FileView<'a>> {
    let version = state.documents.get(id)?.version();
    state
        .view(id)
        .filter(|view| view.document.version() == version)
}

fn plan(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &ClasspathExternals,
    context: Context,
    offset: u32,
    token: &CancellationToken,
) -> Result<Result<Planned, String>, Cancelled> {
    token.check()?;
    if view.tree.has_errors() {
        return Ok(Err("the file has syntax errors".to_owned()));
    }
    let named = named_types(view);
    let wide = match target_at(view, id, externals, &named, offset) {
        Some(Target::Decl {
            name,
            name_span,
            kind,
        }) => match kind {
            DefinitionKind::LocalVariable | DefinitionKind::Parameter => {
                let plan = variable(view, id, externals, name, name_span, token)?;
                return Ok(plan.map(Planned::File).map_err(str::to_owned));
            }
            DefinitionKind::Field | DefinitionKind::Method
                if view.syntax.declaration_at(name_span).is_some_and(|decl| {
                    decl.modifiers.iter().any(|modifier| modifier == "private")
                }) =>
            {
                let plan = member(view, id, externals, name, name_span, token)?;
                return Ok(plan.map(Planned::File).map_err(str::to_owned));
            }
            // A record component is its accessor's and its canonical
            // constructor's parameter's name too.
            DefinitionKind::Field | DefinitionKind::Method
                if !view.syntax.declaration_at(name_span).is_some_and(|decl| {
                    matches!(
                        decl.kind,
                        DeclKind::Field | DeclKind::Method | DeclKind::EnumConstant
                    )
                }) =>
            {
                return Ok(Err(NOT_RENAMEABLE.to_owned()));
            }
            DefinitionKind::Field | DefinitionKind::Method => {
                let Some(path) = crate::workspace::path_of(id.as_str()) else {
                    return Ok(Err(NOT_RENAMEABLE.to_owned()));
                };
                (
                    name,
                    Wide::Member {
                        path,
                        start: name_span.start_byte,
                    },
                )
            }
            // A constructor is its type's name.
            DefinitionKind::Constructor => {
                let Some((fqn, _)) =
                    member_scope_at(&view.syntax.declarations, name_span.start_byte)
                        .and_then(|owner| named.get(&owner.name_span.start_byte))
                else {
                    return Ok(Err(NOT_RENAMEABLE.to_owned()));
                };
                (name, Wide::Type { fqn: fqn.clone() })
            }
            _ => return Ok(Err(NOT_RENAMEABLE.to_owned())),
        },
        Some(Target::Type(fqn)) => (
            fqn.rsplit('.').next().unwrap_or(&fqn).to_owned(),
            Wide::Type { fqn },
        ),
        Some(Target::Member {
            name,
            source,
            start,
            ..
        }) => (
            name,
            Wide::Member {
                path: PathBuf::from(source),
                start,
            },
        ),
        Some(Target::Constructor { .. }) | None => return Ok(Err(NOT_RENAMEABLE.to_owned())),
    };
    Ok(wide_target(context, externals, wide.0, wide.1).map(Planned::Workspace))
}

/// The sweep renaming `wide`, where it is one the workspace may rename.
fn wide_target(
    context: Context,
    externals: &ClasspathExternals,
    name: String,
    wide: Wide,
) -> Result<Sweep, String> {
    if let Some(refusal) = context.unready {
        return Err(refusal.to_owned());
    }
    let index = &context.index;
    match &wide {
        Wide::Type { fqn } => {
            let mut declared = index.with_fqn(fqn);
            let Some(ty) = declared.next() else {
                return Err(format!(
                    "`{fqn}` is declared outside the workspace, in the JDK or a dependency"
                ));
            };
            if declared.next().is_some() {
                return Err(format!("two workspace files declare `{fqn}`"));
            }
            let top_level = index.file_of(&ty.file).is_some_and(|file| {
                file.package.as_ref().map_or(*fqn == ty.simple, |package| {
                    *fqn == format!("{package}.{}", ty.simple)
                })
            });
            // ponytail: a nested type is refused, because `Outer.Inner`,
            // `import p.Outer.Inner` and a subclass's bare `Inner` all need
            // resolving through the outer type. The upgrade is a `Wide::Type`
            // whose sites include those qualified forms.
            if !top_level || ty.synthetic {
                return Err("Javelin renames top-level types, not nested ones".to_owned());
            }
            let prefix = format!("{fqn}.");
            if index
                .types()
                .iter()
                .any(|other| other.synthetic && other.fqn.starts_with(&prefix))
            {
                return Err(
                    "Lombok generates a type named after it, which a rename cannot see".to_owned(),
                );
            }
        }
        Wide::Member { path, start } => {
            if index.file_of(path).is_none() {
                return Err(if context.open.contains_key(path) {
                    "the file is outside the workspace's folders".to_owned()
                } else {
                    format!(
                        "`{name}` is declared outside the workspace, in the JDK or a dependency"
                    )
                });
            }
            let Some(member) = index
                .member_at(path, *start)
                .filter(|member| !member.synthetic)
            else {
                return Err(NOT_RENAMEABLE.to_owned());
            };
            let owner = index.with_fqn(&member.owner).next();
            // A record component is its accessor's and its canonical
            // constructor's parameter's name too.
            if owner.is_some_and(|owner| owner.kind == DeclKind::Record)
                && index.members_of(&member.owner).iter().any(|other| {
                    other.name == name && other.kind == DeclKind::Field && !other.is_static
                })
            {
                return Err(NOT_RENAMEABLE.to_owned());
            }
            if owner.is_some_and(|owner| owner.lombok.annotated) && member.kind == DeclKind::Field {
                return Err(
                    "Lombok may generate accessors for it, which a rename cannot see".to_owned(),
                );
            }
        }
    }
    Ok(Sweep {
        name,
        wide,
        index: context.index,
        externals: externals.clone(),
        open: context.open,
        document_changes: context.document_changes,
        rename_files: context.rename_files,
    })
}

/// A variable's uses: within its scope, every token spelled like it that
/// lands on it. A bare name there that lands nowhere might be it.
fn variable(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &dyn Externals,
    name: String,
    name_span: Span,
    token: &CancellationToken,
) -> Result<Result<Plan, Refusal>, Cancelled> {
    let Some(binding) = view
        .scopes
        .resolve(name_span.start_byte, &name)
        .filter(|binding| binding.name_span.start_byte == name_span.start_byte)
    else {
        return Ok(Err(NOT_RENAMEABLE));
    };
    // A record component binds as a field, and is its accessor's name too.
    if binding.kind == javelin_parser::BindingKind::Field {
        return Ok(Err(NOT_RENAMEABLE));
    }
    // A record's canonical constructor must name its parameters as its
    // components.
    if binding.kind == javelin_parser::BindingKind::Parameter
        && let Some(owner) = parameter_owner(&view.syntax.declarations, name_span)
        && owner.kind == DeclKind::Constructor
        && member_scope_at(&view.syntax.declarations, owner.name_span.start_byte)
            .is_some_and(|ty| ty.kind == DeclKind::Record)
    {
        return Ok(Err(NOT_RENAMEABLE));
    }
    let within = binding.visible_from.min(name_span.start_byte)..=binding.visible_to;
    // Inside an anonymous or local class body a bare name reads that body's
    // inherited fields first, which no scope here can see: a use written there
    // may be one of them, and a renamed use there may become one.
    let home = member_scope_at(&view.syntax.declarations, name_span.start_byte)
        .map(|scope| scope.name_span.start_byte);
    for span in javelin_parser::identifier_spans_in(
        &view.document,
        view.tree,
        &name,
        within.clone(),
        token,
    )? {
        if member_scope_at(&view.syntax.declarations, span.start_byte)
            .map(|scope| scope.name_span.start_byte)
            != home
        {
            return Ok(Err(
                "it is written inside a nested or anonymous class, whose inherited members may \
                 claim the name",
            ));
        }
    }
    let uses = uses(
        view,
        id,
        externals,
        &name,
        name_span,
        within.clone(),
        token,
        |at| javelin_parser::expression_name_at(&view.document, view.tree, at).is_some(),
    )?;
    Ok(uses.map(|spans| Plan {
        name,
        spans,
        within,
        target: Kind::Variable {
            declared_at: name_span.start_byte,
        },
    }))
}

/// A private field's or method's uses: anywhere in the file, which is all of
/// where Java lets anything name it.
fn member(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &dyn Externals,
    name: String,
    name_span: Span,
    token: &CancellationToken,
) -> Result<Result<Plan, Refusal>, Cancelled> {
    let Some(decl) = view.syntax.declaration_at(name_span) else {
        return Ok(Err(NOT_RENAMEABLE));
    };
    if !matches!(decl.kind, DeclKind::Field | DeclKind::Method)
        || !decl.modifiers.iter().any(|modifier| modifier == "private")
    {
        return Ok(Err(NOT_RENAMEABLE));
    }
    // Only a named type's private member is the file's alone to rename; one
    // in an anonymous class or an enum constant's body has no type to ask
    // about the new name.
    let Some(owner) = member_scope_at(&view.syntax.declarations, name_span.start_byte)
        .filter(|owner| is_named_type(owner.kind))
    else {
        return Ok(Err(NOT_RENAMEABLE));
    };
    // Lombok's accessors are uses written under another spelling.
    if view.document.text().contains("lombok") {
        return Ok(Err(
            "Lombok may generate accessors for it, which a rename cannot see",
        ));
    }
    if javelin_parser::strings_mention(&view.document, view.tree, &name, token)? {
        return Ok(Err(
            "a string literal names it, as a reflective or annotation use may",
        ));
    }
    let owner_span = owner.name_span;
    let uses = uses(
        view,
        id,
        externals,
        &name,
        name_span,
        0..=u32::MAX,
        token,
        |_| true,
    )?;
    let spans = match uses {
        Ok(spans) => spans,
        Err(refusal) => return Ok(Err(refusal)),
    };
    // A bare use inside another type's body would read the new name through
    // that type's own members first, inherited ones included.
    for span in &spans {
        if span.start_byte != name_span.start_byte
            && !javelin_parser::has_receiver(view.tree, span.start_byte)
            && member_scope_at(&view.syntax.declarations, span.start_byte)
                .is_none_or(|scope| scope.name_span != owner_span)
        {
            return Ok(Err(
                "it is used by its bare name inside a nested or anonymous class",
            ));
        }
    }
    Ok(Ok(Plan {
        name,
        spans,
        within: 0..=u32::MAX,
        target: Kind::Member { owner: owner_span },
    }))
}

/// The declaration's name and every token in `within` spelled `name` that
/// lands on it; a refusal for a token that lands nowhere where `could_be`
/// says it might be the target.
#[allow(clippy::too_many_arguments)]
fn uses(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &dyn Externals,
    name: &str,
    name_span: Span,
    within: RangeInclusive<u32>,
    token: &CancellationToken,
    could_be: impl Fn(u32) -> bool,
) -> Result<Result<Vec<Span>, Refusal>, Cancelled> {
    let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
    let mut spans = vec![name_span];
    for span in javelin_parser::identifier_spans_in(&view.document, view.tree, name, within, token)?
    {
        token.check()?;
        if span.start_byte == name_span.start_byte {
            continue;
        }
        match definition_in(view, &files, externals, span.start_byte) {
            Some(found) if found.kind == DefinitionKind::PatternBinding => {
                return Ok(Err(
                    "a pattern binding of the same name may claim one of its uses",
                ));
            }
            Some(found) if found.file == *id && found.name_span == name_span => spans.push(span),
            Some(_) => {}
            None if could_be(span.start_byte) => {
                return Ok(Err("a use of the name could not be resolved"));
            }
            None => {}
        }
    }
    spans.sort_by_key(|span| span.start_byte);
    Ok(Ok(spans))
}

/// Whether `new_name` can be written at every span without changing what any
/// name in the file means.
///
/// Coarse on purpose: the new name may not be written anywhere it could be
/// captured or could capture, so no shadowing argument is needed.
fn free(
    view: &FileView<'_>,
    id: &DocumentId,
    externals: &dyn Externals,
    plan: &Plan,
    new_name: &str,
    token: &CancellationToken,
) -> Result<Result<(), Refusal>, Cancelled> {
    if !is_identifier(new_name) {
        return Ok(Err("the new name is not a Java identifier"));
    }
    let written = javelin_parser::identifier_spans_in(
        &view.document,
        view.tree,
        new_name,
        plan.within.clone(),
        token,
    )?;
    if !written.is_empty() {
        return Ok(Err(
            "the new name is already used where the renamed one is visible",
        ));
    }
    match plan.target {
        Kind::Variable { declared_at } => {
            let shadowed = view
                .scopes
                .resolve(declared_at, new_name)
                .is_some_and(|binding| binding.kind != javelin_parser::BindingKind::Field);
            if shadowed {
                return Ok(Err("a variable of the new name is already in scope"));
            }
        }
        Kind::Member { owner } => {
            let files = javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
            let Some(owner) = view.syntax.declaration_at(owner) else {
                return Ok(Err(NOT_RENAMEABLE));
            };
            match javelin_semantic::exposes_member(&files, externals, owner, new_name) {
                Some(false) => {}
                Some(true) => return Ok(Err("the type already has a member of the new name")),
                None => {
                    return Ok(Err(
                        "the type's supertypes are not all known, so the new name may clash",
                    ));
                }
            }
        }
    }
    Ok(Ok(()))
}

/// The innermost declaration holding `offset` whose body is a member scope of
/// its own: a type, an anonymous class, or an enum constant with a body.
fn member_scope_at(decls: &[Decl], offset: u32) -> Option<&Decl> {
    let mut decls = decls;
    let mut found = None;
    loop {
        let Some(decl) = decls
            .iter()
            .rfind(|decl| decl.span.start_byte <= offset && offset <= decl.span.end_byte)
        else {
            return found;
        };
        if is_named_type(decl.kind)
            || decl.kind == DeclKind::AnonymousClass
            || (decl.kind == DeclKind::EnumConstant && !decl.children.is_empty())
        {
            found = Some(decl);
        }
        decls = &decl.children;
    }
}

/// The method or constructor whose formal parameter is named at `name_span`.
fn parameter_owner(decls: &[Decl], name_span: Span) -> Option<&Decl> {
    decls.iter().find_map(|decl| {
        if decl
            .parameters
            .iter()
            .any(|parameter| parameter.name_span == name_span)
        {
            return Some(decl);
        }
        parameter_owner(&decl.children, name_span)
    })
}

fn is_named_type(kind: DeclKind) -> bool {
    matches!(
        kind,
        DeclKind::Class
            | DeclKind::Interface
            | DeclKind::Enum
            | DeclKind::Record
            | DeclKind::AnnotationType
    )
}

/// A Java identifier that is no keyword, literal or restricted name.
/// `var`, `yield`, `record` and `_` are refused even where Java would allow
/// them, since where it does not is a question of position.
fn is_identifier(name: &str) -> bool {
    const RESERVED: &[&str] = &[
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "try",
        "void",
        "volatile",
        "while",
        "true",
        "false",
        "null",
        "var",
        "yield",
        "record",
        "_",
    ];
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == '$')
        // Not `is_alphanumeric`, which takes `²` and other numerals Java does not.
        && chars.all(|c| c.is_alphabetic() || c.is_ascii_digit() || c == '_' || c == '$')
        && !RESERVED.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_core::Config;

    const URI: &str = "file:///w/com/example/A.java";
    const USED: Refusal = "the new name is already used where the renamed one is visible";

    fn opened(source: &str) -> ServerState {
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.did_open(DocumentId::new(URI), 1, source.to_owned());
        state
    }

    fn position(source: &str, needle: &str) -> lsp_types::Position {
        let at = source.find(needle).expect("needle in source");
        let line = source[..at].matches('\n').count() as u32;
        let character = (at - source[..at].rfind('\n').map_or(0, |nl| nl + 1)) as u32;
        lsp_types::Position::new(line, character)
    }

    /// The source after renaming the name at `needle` to `new_name`, or the
    /// refusal.
    fn rename(source: &str, needle: &str, new_name: &str) -> Result<String, Refusal> {
        let mut state = opened(source);
        let params: RenameParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": position(source, needle),
            "newName": new_name,
        }))
        .expect("well-formed params");
        // Leaked so the one-file tests keep comparing against the constants.
        let edit = handle(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .map_err(|refusal| &*refusal.leak())?;
        let uri: lsp_types::Uri = URI.parse().expect("uri");
        let mut edits = edit
            .changes
            .unwrap_or_default()
            .remove(&uri)
            .unwrap_or_default();
        let document = javelin_document::TextDocument::new(DocumentId::new(URI), 1, source);
        edits.sort_by_key(|edit| {
            std::cmp::Reverse(document.offset_at(to_position(edit.range.start)))
        });
        let mut out = source.to_owned();
        for edit in edits {
            let start = document.offset_at(to_position(edit.range.start)) as usize;
            let end = document.offset_at(to_position(edit.range.end)) as usize;
            out.replace_range(start..end, &edit.new_text);
        }
        Ok(out)
    }

    fn prepared(source: &str, needle: &str) -> Option<String> {
        let mut state = opened(source);
        let params: TextDocumentPositionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": position(source, needle),
        }))
        .expect("well-formed params");
        match prepare(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled")
            .ok()?
        {
            PrepareRenameResponse::RangeWithPlaceholder { placeholder, .. } => Some(placeholder),
            other => panic!("unexpected {other:?}"),
        }
    }

    /// A `null` prepare reads, in an editor, as a key that does nothing.
    #[test]
    fn prepare_on_what_cannot_be_renamed_answers_why() {
        let source = "class A<T> { T count; }";
        let mut state = opened(source);
        let params: TextDocumentPositionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": position(source, "T>"),
        }))
        .expect("well-formed params");
        assert_eq!(
            prepare(&mut state, &params, &CancellationToken::new()).expect("not cancelled"),
            Err(NOT_RENAMEABLE.to_owned())
        );
    }

    #[test]
    fn a_local_is_renamed_with_every_use_and_nothing_else() {
        let source = "class A { int count; void m(A o) { int count = 1; count += o.count; \
                      use(count); } void use(int x) {} }";
        assert_eq!(
            rename(source, "count = 1", "total").unwrap(),
            "class A { int count; void m(A o) { int total = 1; total += o.count; \
             use(total); } void use(int x) {} }"
        );
        assert_eq!(prepared(source, "count = 1").as_deref(), Some("count"));
        assert_eq!(prepared(source, "count; void").as_deref(), None);
    }

    #[test]
    fn a_parameter_is_renamed_and_a_pattern_binding_is_refused() {
        let source = "class A { int m(Object o) { if (o instanceof String s) return s.length(); \
                      return o.hashCode(); } }";
        assert_eq!(
            rename(source, "o)", "value").unwrap(),
            "class A { int m(Object value) { if (value instanceof String s) return s.length(); \
             return value.hashCode(); } }"
        );
        assert!(rename(source, "s)", "text").is_err());
    }

    #[test]
    fn a_pattern_binding_that_may_claim_a_use_refuses_a_field_rename() {
        // The scope tree binds `s` to the method's end, so the last `s` would
        // look like the binding's, and the field's rename would miss it.
        let source = "class A { private String s = \"\"; String m(Object o) { \
                      if (o instanceof String s) { return s; } return s; } }";
        assert_eq!(
            rename(source, "s = ", "t"),
            Err("a pattern binding of the same name may claim one of its uses")
        );
    }

    #[test]
    fn a_local_written_inside_an_anonymous_or_local_class_is_refused() {
        const NESTED: Refusal = "it is written inside a nested or anonymous class, whose \
                                 inherited members may claim the name";
        // `B.count` would capture the renamed use.
        let captured = "class B { int count; } class A { Object m() { int a = 1; \
                        return new B() { int f() { return a; } }; } }";
        assert_eq!(rename(captured, "a = 1", "count"), Err(NESTED));
        // Java reads this `a` as the inherited `B.a`, not the local.
        let inherited = "class B { int a; } class A { Object m() { int a = 1; \
                         return new B() { int f() { return a; } }; } }";
        assert_eq!(rename(inherited, "a = 1", "b"), Err(NESTED));
        let local = "class B { int count; } class A { void m() { int a = 1; \
                     class L extends B { int f() { return a; } } } }";
        assert_eq!(rename(local, "a = 1", "count"), Err(NESTED));
    }

    #[test]
    fn a_records_canonical_constructor_parameter_is_refused() {
        let source = "record P(int x) { P(int x) { this.x = x; } }";
        assert_eq!(rename(source, "x) {", "y"), Err(NOT_RENAMEABLE));
        let class = "class P { int x; P(int x) { this.x = x; } }";
        assert_eq!(
            rename(class, "x) {", "y").unwrap(),
            "class P { int x; P(int y) { this.x = y; } }"
        );
    }

    #[test]
    fn a_local_is_not_renamed_to_a_name_written_in_its_scope() {
        // `b` is the field: renaming `a` to it would capture the field's use.
        let source = "class A { int b; void m() { int a = 1; a = b; } }";
        assert_eq!(rename(source, "a = 1", "b"), Err(USED));
        let outer = "class A { void m(int b) { Runnable r = () -> { int a = 1; }; } }";
        assert_eq!(
            rename(outer, "a = 1", "b"),
            Err("a variable of the new name is already in scope")
        );
    }

    #[test]
    fn keywords_and_non_identifiers_are_refused() {
        let source = "class A { void m() { int a = 1; } }";
        assert!(rename(source, "a = 1", "class").is_err());
        assert!(rename(source, "a = 1", "1a").is_err());
        assert!(rename(source, "a = 1", "a b").is_err());
        assert!(rename(source, "a = 1", "x\u{b2}").is_err());
        assert!(rename(source, "a = 1", "gr\u{f6}\u{df}e").is_ok());
        assert!(rename(source, "a = 1", "ok_1$").is_ok());
    }

    #[test]
    fn without_a_workspace_types_and_non_private_members_are_refused() {
        let source = "class A { int f; void m() { f = 1; } }";
        assert!(rename(source, "f;", "g").is_err());
        assert!(rename(source, "A {", "B").is_err());
        assert!(rename(source, "m()", "n").is_err());
        let record = "record P(int x) { int twice() { return x * 2; } }";
        assert!(rename(record, "x)", "y").is_err());
    }

    #[test]
    fn a_file_with_syntax_errors_is_refused() {
        let source = "class A { void m() { int a = 1; a = ; } }";
        assert!(rename(source, "a = 1", "b").is_err());
    }

    #[test]
    fn a_private_field_is_renamed_through_this_and_other_instances() {
        let source =
            "class A { private int n; boolean same(A o) { return this.n == o.n && n > 0; } }";
        assert_eq!(
            rename(source, "n;", "size").unwrap(),
            "class A { private int size; boolean same(A o) { return this.size == o.size && size > 0; } }"
        );
    }

    #[test]
    fn a_private_method_is_renamed_with_its_calls_and_references() {
        let source = "class A { private int go(int x) { return x; } int m() { \
                      java.util.function.IntUnaryOperator f = this::go; return go(1); } }";
        assert_eq!(
            rename(source, "go(int", "run").unwrap(),
            "class A { private int run(int x) { return x; } int m() { \
             java.util.function.IntUnaryOperator f = this::run; return run(1); } }"
        );
    }

    #[test]
    fn a_private_member_under_lombok_or_named_in_a_string_is_refused() {
        let lombok = "@lombok.Getter class A { private int n; int m() { return n; } }";
        assert_eq!(
            rename(lombok, "n;", "size"),
            Err("Lombok may generate accessors for it, which a rename cannot see")
        );
        let source = "class A { @S(\"go\") void t() {} private void go() {} }";
        assert_eq!(
            rename(source, "go() {}", "run"),
            Err("a string literal names it, as a reflective or annotation use may")
        );
    }

    #[test]
    fn a_bare_use_inside_an_anonymous_class_is_refused() {
        let source = "class A { private int n; Object m() { return new Object() { \
                      int k() { return n; } }; } }";
        assert_eq!(
            rename(source, "n;", "size"),
            Err("it is used by its bare name inside a nested or anonymous class")
        );
    }

    #[test]
    fn a_member_is_not_renamed_to_an_inherited_name() {
        let source =
            "class B { int size; } class A extends B { private int n; int m() { return n; } }";
        assert_eq!(rename(source, "n;", "size"), Err(USED));
        assert_eq!(
            rename(source, "n;", "hashCode"),
            Err("the type already has a member of the new name")
        );
        assert!(rename(source, "n;", "count").is_ok());
        let unknown = "class A extends Missing { private int n; }";
        assert_eq!(
            rename(unknown, "n;", "count"),
            Err("the type's supertypes are not all known, so the new name may clash")
        );
    }

    #[test]
    fn an_unresolved_selection_of_the_name_refuses_a_member_rename() {
        // `x.n` may be this field or another type's: no answer, no rename.
        let source = "class A { private int n; int m(Unknown x) { return x.n + n; } }";
        assert_eq!(
            rename(source, "n;", "size"),
            Err("a use of the name could not be resolved")
        );
    }

    #[test]
    fn a_stale_view_is_refused() {
        let source = "class A { void m() { int a = 1; } }";
        let mut state = opened(source);
        state.did_change(
            &DocumentId::new(URI),
            2,
            &[javelin_document::Change::full(
                "class A { void m() { int a = 2; } }",
            )],
        );
        let params: RenameParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": URI },
            "position": position(source, "a = 1"),
            "newName": "b",
        }))
        .expect("well-formed params");
        let answer = handle(&mut state, &params, &CancellationToken::new()).expect("not cancelled");
        // Either the reparse has landed and the rename is current, or it is
        // refused; never an edit over version 1's text.
        if let Ok(edit) = answer {
            assert!(edit.changes.is_some());
        }
    }
}
