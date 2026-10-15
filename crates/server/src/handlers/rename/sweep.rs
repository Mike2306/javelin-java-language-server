//! Renaming what other files can name: a top-level type the workspace
//! declares, its file with it, and a field or method one of its types
//! declares, with every method that overrides it or that it overrides.
//!
//! The rule is the one-file rename's, widened to the workspace: every
//! workspace file that spells the name is read — its open buffer, or the disk
//! — and every token of that spelling is resolved the way go-to-definition
//! resolves it. A token that lands on the target is rewritten, one that lands
//! elsewhere is left alone, and one that lands nowhere while it could be the
//! target refuses the whole rename. A text search over every file is the
//! superset the recorded member references are not, so those are not read.
//!
//! Runs off the request loop: it reads every workspace file.
//!
//! ponytail: only Java sources are searched, so a fully-qualified name in
//! XML, a `META-INF/services` entry or a properties file is left behind; a
//! comment's `{@link}` is too. The upgrade is a text pass over those files
//! offered as a separate, optional edit.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use javelin_core::{CancellationToken, Cancelled};
use javelin_document::{DocumentId, TextDocument};
use javelin_index::{WorkspaceFile, WorkspaceIndex};
use javelin_parser::{DeclKind, DefinitionKind, FileSyntax, JavaParser, ScopeTree, Span};
use javelin_semantic::Externals;
use lsp_types::{
    DocumentChangeOperation, DocumentChanges, OneOf, OptionalVersionedTextDocumentIdentifier,
    RenameFile, ResourceOp, TextDocumentEdit, TextEdit, Uri, WorkspaceEdit,
};

use super::{is_identifier, member_scope_at};
use crate::classpath::ClasspathExternals;
use crate::handlers::references::{Target, named_types, target_at};
use crate::handlers::{file_uri, to_lsp_range};
use crate::state::FileView;

/// `java.lang.Object`'s methods: every type has them, and the closure walk
/// may leave them out.
const OBJECT_METHODS: [&str; 9] = [
    "clone",
    "equals",
    "finalize",
    "getClass",
    "hashCode",
    "notify",
    "notifyAll",
    "toString",
    "wait",
];

/// What a rename across the workspace renames.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Wide {
    /// A top-level type, by its fully-qualified name.
    Type { fqn: String },
    /// A field or method, by its declaring file and the byte offset of its
    /// name there — the site the index keys it under.
    Member { path: PathBuf, start: u32 },
}

/// Everything a workspace rename reads, taken off the state on the loop so
/// the sweep can run on a thread of its own.
pub(crate) struct Sweep {
    pub(super) name: String,
    pub(super) wide: Wide,
    pub(super) index: Arc<WorkspaceIndex>,
    pub(super) externals: ClasspathExternals,
    /// Open buffers by path: what the user sees is what is renamed.
    pub(super) open: HashMap<PathBuf, Arc<TextDocument>>,
    /// The client applies versioned edits and file operations.
    pub(super) document_changes: bool,
    pub(super) rename_files: bool,
}

/// What a token resolved to.
enum Lands {
    Target,
    Elsewhere,
    /// Nothing, and it could be the target: the reason, for the refusal.
    Unknown(&'static str),
}

/// One file's renamed spans, with the text they were found in.
type Edited = (Arc<TextDocument>, PathBuf, Vec<Span>);

/// What resolving decides is the target, worked out before any file is read.
#[derive(Default)]
struct Scope {
    /// For a member: the sites of every member of the family.
    sites: HashSet<(PathBuf, u32)>,
    /// The types declaring one, which a static import may name.
    owners: HashSet<String>,
    /// Owners with a member of the name outside the family, which a static
    /// import of the name imports too.
    overloaded: HashSet<String>,
    /// The family's declaring files, whose string literals must not name it.
    declaring: HashSet<PathBuf>,
    /// Every type whose member list the new name must not join: the owners,
    /// what they inherit from and what inherits from them. A file writing
    /// one of their simple names may hold an anonymous subclass.
    related: Vec<String>,
    /// For a type: the package whose files see it without an import.
    package: Option<String>,
    /// The type's file, and what it becomes.
    file_rename: Option<(PathBuf, PathBuf)>,
}

impl Sweep {
    /// The edit renaming the target to `new_name` in every workspace file,
    /// or why there is none.
    pub(crate) fn run(
        self,
        new_name: &str,
        token: &CancellationToken,
    ) -> Result<Result<WorkspaceEdit, String>, Cancelled> {
        if !is_identifier(new_name) {
            return Ok(Err("the new name is not a Java identifier".to_owned()));
        }
        if new_name == self.name {
            return Ok(Ok(WorkspaceEdit::default()));
        }
        let scope = match &self.wide {
            Wide::Type { fqn } => self.type_scope(fqn, new_name),
            Wide::Member { path, start } => self.member_scope(path, *start, new_name),
        };
        let scope = match scope {
            Ok(scope) => scope,
            Err(refusal) => return Ok(Err(refusal)),
        };
        let mut parser = match JavaParser::new() {
            Ok(parser) => parser,
            Err(err) => return Ok(Err(format!("the parser could not start: {err}"))),
        };
        let mut edited: Vec<Edited> = Vec::new();
        for file in self.index.sources() {
            token.check()?;
            match self.file(&mut parser, &scope, file, new_name, token)? {
                Ok(Some(edit)) => edited.push(edit),
                Ok(None) => {}
                Err(refusal) => return Ok(Err(refusal)),
            }
        }
        Ok(self.edit(edited, scope.file_rename, new_name))
    }

    /// A top-level type: every file of its package sees it unimported.
    fn type_scope(&self, fqn: &str, new_name: &str) -> Result<Scope, String> {
        let Some(ty) = self.index.with_fqn(fqn).next() else {
            return Err(format!("`{fqn}` is no longer in the workspace index"));
        };
        let package = self
            .index
            .file_of(&ty.file)
            .and_then(|file| file.package.clone());
        let renamed = match &package {
            Some(package) => format!("{package}.{new_name}"),
            None => new_name.to_owned(),
        };
        if self.index.with_fqn(&renamed).next().is_some() {
            return Err(format!("the workspace already declares `{renamed}`"));
        }
        let mut file_rename = None;
        if ty.file.file_stem().and_then(|stem| stem.to_str()) == Some(self.name.as_str()) {
            let to = ty.file.with_file_name(format!("{new_name}.java"));
            if to.exists() {
                return Err(format!("{} already exists", to.display()));
            }
            if !self.rename_files {
                return Err(format!(
                    "the editor cannot rename files, and {} must become {new_name}.java",
                    file_name(&ty.file)
                ));
            }
            file_rename = Some((ty.file.clone(), to));
        }
        Ok(Scope {
            package,
            file_rename,
            ..Scope::default()
        })
    }

    /// A field, or a method and every method in its hierarchy that one of
    /// them overrides or is overridden by.
    fn member_scope(&self, path: &Path, start: u32, new_name: &str) -> Result<Scope, String> {
        let Some(target) = self
            .index
            .member_at(path, start)
            .filter(|member| member.name == self.name)
        else {
            return Err("the declaration is no longer in the workspace index".to_owned());
        };
        let mut scope = Scope::default();
        scope.sites.insert((path.to_owned(), start));
        let mut queue = vec![target.owner.clone()];
        let mut walked: HashSet<String> = HashSet::new();
        while let Some(owner) = queue.pop() {
            if !walked.insert(owner.clone()) {
                continue;
            }
            // Its own members and every supertype's, overridden ones included.
            let Some(closure) = self
                .externals
                .members(&owner)
                .filter(|found| found.complete)
            else {
                return Err(format!(
                    "the supertypes of `{owner}` are not all known, so what it overrides is not"
                ));
            };
            let mut found: Vec<(String, Option<(PathBuf, u32)>, usize)> = closure
                .members
                .iter()
                .filter(|member| member.name == self.name && member.owner != owner)
                .map(|member| {
                    let site = member
                        .site
                        .as_ref()
                        .filter(|site| site.name_range.is_some())
                        .map(|site| (PathBuf::from(&site.source), site.name_start));
                    (member.owner.clone(), site, member.parameters.len())
                })
                .collect();
            let subtypes = crate::hierarchy::all_subtypes(&self.externals, &self.index, &owner);
            for subtype in &subtypes {
                scope.related.push(subtype.fqn.clone());
                found.extend(
                    self.index
                        .members_of(&subtype.fqn)
                        .iter()
                        .filter(|member| member.name == self.name && !member.synthetic)
                        .map(|member| {
                            let file = self.index.file(member.file).path.clone();
                            (
                                member.owner.clone(),
                                Some((file, member.name_start)),
                                member.parameters.len(),
                            )
                        }),
                );
            }
            scope.related.push(owner.clone());
            for (other, site, arity) in found {
                if target.kind != DeclKind::Method {
                    continue;
                }
                let Some(site) = site else {
                    if arity == target.parameters.len() {
                        return Err(format!(
                            "it may override `{other}.{}`, which is not the workspace's to rename",
                            self.name
                        ));
                    }
                    continue;
                };
                let Some(member) = self.index.member_at(&site.0, site.1) else {
                    continue;
                };
                if member.kind != DeclKind::Method {
                    continue;
                }
                if member.parameters == target.parameters {
                    if scope.sites.insert(site) {
                        queue.push(member.owner.clone());
                    }
                } else if member.parameters.len() == target.parameters.len() {
                    return Err(format!(
                        "`{other}.{}` takes as many parameters, written differently, and may \
                         override it",
                        self.name
                    ));
                }
            }
        }
        if target.kind == DeclKind::Method && OBJECT_METHODS.contains(&self.name.as_str()) {
            return Err(format!(
                "it may override `java.lang.Object.{}`, which is not the workspace's to rename",
                self.name
            ));
        }
        for (path, start) in &scope.sites {
            let Some(member) = self.index.member_at(path, *start) else {
                return Err("the declaration is no longer in the workspace index".to_owned());
            };
            scope.owners.insert(member.owner.clone());
            scope.declaring.insert(path.clone());
        }
        for owner in &scope.owners {
            let file = |member: &javelin_index::WorkspaceMember| {
                (self.index.file(member.file).path.clone(), member.name_start)
            };
            if self
                .index
                .members_of(owner)
                .iter()
                .any(|member| member.name == self.name && !scope.sites.contains(&file(member)))
            {
                scope.overloaded.insert(owner.clone());
            }
        }
        if OBJECT_METHODS.contains(&new_name) {
            return Err(format!(
                "every type already has `{new_name}`, from java.lang.Object"
            ));
        }
        scope.related.sort();
        scope.related.dedup();
        for ty in &scope.related {
            let Some(closure) = self.externals.members(ty).filter(|found| found.complete) else {
                return Err(format!(
                    "the members of `{ty}` are not all known, so the new name may clash"
                ));
            };
            if let Some(clash) = closure
                .members
                .iter()
                .find(|member| member.name == new_name)
            {
                return Err(format!(
                    "`{}` already has a member named `{new_name}`",
                    clash.owner
                ));
            }
        }
        Ok(scope)
    }

    /// The renamed spans of one workspace file; `None` where it names
    /// nothing renamed.
    fn file(
        &self,
        parser: &mut JavaParser,
        scope: &Scope,
        file: &WorkspaceFile,
        new_name: &str,
        token: &CancellationToken,
    ) -> Result<Result<Option<Edited>, String>, Cancelled> {
        let path = &file.path;
        let document = match self.open.get(path) {
            Some(document) => Arc::clone(document),
            None => {
                let Some(uri) = file_uri(path) else {
                    return Ok(Err(format!("{} has no file URI", path.display())));
                };
                match std::fs::read_to_string(path) {
                    Ok(text) => Arc::new(TextDocument::new(DocumentId::new(uri.as_str()), 0, text)),
                    Err(err) => {
                        return Ok(Err(format!("{} could not be read: {err}", path.display())));
                    }
                }
            }
        };
        let text = document.text();
        let names_old = writes(text, &self.name);
        // Files where the new name could capture or be captured though the
        // old one is not written: the type's package, and its on-demand
        // importers; for a member, any file naming a type of its hierarchy,
        // which may hold an anonymous subclass.
        let watched = match &self.wide {
            Wide::Type { .. } => {
                file.package == scope.package
                    || file.imports.iter().any(|import| {
                        import.is_on_demand
                            && !import.is_static
                            && Some(&import.name) == scope.package.as_ref()
                    })
            }
            Wide::Member { .. } => scope
                .related
                .iter()
                .any(|fqn| writes(text, fqn.rsplit('.').next().unwrap_or(fqn))),
        };
        let names_new = writes(text, new_name);
        if !names_old && !(watched && names_new) {
            return Ok(Ok(None));
        }
        let Ok(tree) = parser.parse(&document) else {
            return Ok(Err(format!("{} could not be parsed", file_name(path))));
        };
        if tree.has_errors() {
            return Ok(Err(format!("{} has syntax errors", file_name(path))));
        }
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        let view = FileView {
            document: Arc::clone(&document),
            tree: &tree,
            syntax: &syntax,
            scopes: &scopes,
        };
        let id = document.id().clone();
        let named = named_types(&view);
        let mut spans = Vec::new();
        let mut bare = false;
        if names_old {
            for span in javelin_parser::identifier_spans_in(
                &document,
                &tree,
                &self.name,
                0..=u32::MAX,
                token,
            )? {
                token.check()?;
                match self.lands(&view, &id, path, &named, scope, span) {
                    Lands::Target => {
                        bare |= !scope.sites.contains(&(path.clone(), span.start_byte))
                            && !javelin_parser::has_receiver(&tree, span.start_byte);
                        spans.push(span);
                    }
                    Lands::Elsewhere => {}
                    Lands::Unknown(why) => {
                        return Ok(Err(format!("{why}, at {}", place(path, span))));
                    }
                }
            }
        }
        let strings = match &self.wide {
            Wide::Type { fqn } => (!spans.is_empty()).then_some(fqn.as_str()),
            Wide::Member { .. } => scope.declaring.contains(path).then_some(self.name.as_str()),
        };
        if let Some(word) = strings
            && javelin_parser::strings_mention(&document, &tree, word, token)?
        {
            return Ok(Err(format!(
                "a string literal in {} names `{word}`, as a reflective use may",
                file_name(path)
            )));
        }
        // A type's new name is checked wherever it is written; a member's
        // only where a bare use or a declaration could be captured by it.
        // ponytail: a member type inherited from a supertype in another file
        // or a JAR is not written here, so a renamed use inside a subclass can
        // be captured by one of the new name. The upgrade is checking each
        // enclosing type's member types through `externals` before rewriting.
        let capture = match &self.wide {
            Wide::Type { .. } => !spans.is_empty() || watched,
            Wide::Member { .. } => bare,
        };
        if names_new && capture {
            let written = javelin_parser::identifier_spans_in(
                &document,
                &tree,
                new_name,
                0..=u32::MAX,
                token,
            )?;
            if let Some(at) = written.first() {
                return Ok(Err(format!(
                    "`{new_name}` is already written at {}",
                    place(path, *at)
                )));
            }
        }
        if matches!(self.wide, Wide::Member { .. })
            && watched
            && names_new
            && declares_in_body(&syntax.declarations, new_name)
        {
            return Ok(Err(format!(
                "an anonymous class in {} declares `{new_name}`",
                file_name(path)
            )));
        }
        if spans.is_empty() {
            return Ok(Ok(None));
        }
        Ok(Ok(Some((document, path.clone(), spans))))
    }

    /// Whether the token at `span` names the target.
    fn lands(
        &self,
        view: &FileView<'_>,
        id: &DocumentId,
        path: &Path,
        named: &HashMap<u32, (String, javelin_document::Range)>,
        scope: &Scope,
        span: Span,
    ) -> Lands {
        let at = span.start_byte;
        if view
            .syntax
            .package
            .as_ref()
            .is_some_and(|package| package.span.start_byte <= at && at < package.span.end_byte)
        {
            return Lands::Elsewhere;
        }
        // An import is read as written: the name up to this token is the
        // fully-qualified one it names.
        if let Some(import) = view.syntax.imports.iter().find(|import| {
            import.name_span.start_byte <= at && span.end_byte <= import.name_span.end_byte
        }) {
            let written =
                &view.document.text()[import.name_span.start_byte as usize..span.end_byte as usize];
            if written.contains('/') {
                return Lands::Unknown("a comment inside an import hides what it names");
            }
            let written: String = written.chars().filter(|c| !c.is_whitespace()).collect();
            return match &self.wide {
                Wide::Type { fqn } if written == *fqn => Lands::Target,
                Wide::Member { .. }
                    if import.is_static && span.end_byte == import.name_span.end_byte =>
                {
                    let Some((owner, _)) = written.rsplit_once('.') else {
                        return Lands::Elsewhere;
                    };
                    if !scope.owners.contains(owner) {
                        Lands::Elsewhere
                    } else if scope.overloaded.contains(owner) {
                        Lands::Unknown(
                            "a static import of the name also imports a member the rename leaves",
                        )
                    } else {
                        Lands::Target
                    }
                }
                _ => Lands::Elsewhere,
            };
        }
        match target_at(view, id, &self.externals, named, at) {
            Some(Target::Type(fqn)) => match &self.wide {
                Wide::Type { fqn: target } if fqn == *target => Lands::Target,
                _ => Lands::Elsewhere,
            },
            Some(Target::Decl {
                name_span, kind, ..
            }) => match &self.wide {
                Wide::Type { fqn } => {
                    let owner = member_scope_at(&view.syntax.declarations, name_span.start_byte)
                        .and_then(|owner| named.get(&owner.name_span.start_byte));
                    if kind == DefinitionKind::Constructor
                        && owner.is_some_and(|(owner, _)| owner == fqn)
                    {
                        Lands::Target
                    } else {
                        Lands::Elsewhere
                    }
                }
                Wide::Member { .. } => {
                    if matches!(kind, DefinitionKind::Field | DefinitionKind::Method)
                        && scope
                            .sites
                            .contains(&(path.to_owned(), name_span.start_byte))
                    {
                        Lands::Target
                    } else if kind == DefinitionKind::Method
                        && member_scope_at(&view.syntax.declarations, name_span.start_byte)
                            .and_then(|owner| named.get(&owner.name_span.start_byte))
                            .is_none_or(|(owner, _)| self.index.with_fqn(owner).next().is_none())
                    {
                        // A body the index holds no type for — an anonymous
                        // class, an enum constant's, a local class — may
                        // override a family member, and no row says which.
                        Lands::Unknown(
                            "an anonymous or local class declares a method of the name, which \
                             may override it",
                        )
                    } else {
                        Lands::Elsewhere
                    }
                }
            },
            Some(
                Target::Member { source, start, .. } | Target::Constructor { source, start, .. },
            ) => match &self.wide {
                Wide::Member { .. } if scope.sites.contains(&(PathBuf::from(&source), start)) => {
                    Lands::Target
                }
                Wide::Type { fqn } => {
                    let constructor =
                        self.index
                            .member_at(Path::new(&source), start)
                            .is_some_and(|member| {
                                member.kind == DeclKind::Constructor && member.owner == *fqn
                            });
                    if constructor {
                        Lands::Target
                    } else {
                        Lands::Elsewhere
                    }
                }
                Wide::Member { .. } => Lands::Elsewhere,
            },
            None => match &self.wide {
                Wide::Type { .. } => Lands::Unknown("a use of the name could not be resolved"),
                Wide::Member { .. } => {
                    let files =
                        javelin_semantic::Files::single(id.clone(), view.syntax, view.scopes);
                    // `@A(name = …)` names an annotation's element, and only
                    // an annotation type declares one.
                    let element =
                        javelin_parser::annotation_element_at(&view.document, view.tree, at)
                            .is_some()
                            && !scope.owners.iter().any(|owner| {
                                self.index
                                    .with_fqn(owner)
                                    .any(|ty| ty.kind == DeclKind::AnnotationType)
                            });
                    // A receiver whose whole member list lacks the name
                    // cannot be selecting the target.
                    let missing = javelin_semantic::missing_member_at(
                        &view.document,
                        view.tree,
                        &files,
                        &self.externals,
                        at,
                    )
                    .is_some();
                    if element || missing {
                        Lands::Elsewhere
                    } else {
                        Lands::Unknown("a use of the name could not be resolved")
                    }
                }
            },
        }
    }

    /// The edit, as versioned document changes where the client takes them.
    fn edit(
        &self,
        edited: Vec<Edited>,
        file_rename: Option<(PathBuf, PathBuf)>,
        new_name: &str,
    ) -> Result<WorkspaceEdit, String> {
        let uri = |document: &TextDocument| -> Result<Uri, String> {
            document
                .id()
                .as_str()
                .parse()
                .map_err(|_| format!("`{}` is not a URI", document.id().as_str()))
        };
        if !self.document_changes {
            return Ok(WorkspaceEdit::new(
                edited
                    .into_iter()
                    .map(|(document, _, spans)| {
                        let edits = spans
                            .iter()
                            .map(|span| {
                                TextEdit::new(to_lsp_range(span.range), new_name.to_owned())
                            })
                            .collect();
                        Ok((uri(&document)?, edits))
                    })
                    .collect::<Result<_, String>>()?,
            ));
        }
        let mut operations = Vec::new();
        for (document, path, spans) in edited {
            let version = self.open.contains_key(&path).then(|| document.version());
            operations.push(DocumentChangeOperation::Edit(TextDocumentEdit {
                text_document: OptionalVersionedTextDocumentIdentifier {
                    uri: uri(&document)?,
                    version,
                },
                edits: spans
                    .iter()
                    .map(|span| {
                        OneOf::Left(TextEdit::new(to_lsp_range(span.range), new_name.to_owned()))
                    })
                    .collect(),
            }));
        }
        // After the edits, which address the file by its old name.
        if let Some((from, to)) = file_rename {
            let old_uri = match self.open.get(&from) {
                Some(document) => uri(document)?,
                None => file_uri(&from).ok_or_else(|| format!("{} has no URI", from.display()))?,
            };
            let new_uri = file_uri(&to).ok_or_else(|| format!("{} has no URI", to.display()))?;
            operations.push(DocumentChangeOperation::Op(ResourceOp::Rename(
                RenameFile {
                    old_uri,
                    new_uri,
                    options: None,
                    annotation_id: None,
                },
            )));
        }
        Ok(WorkspaceEdit {
            document_changes: Some(DocumentChanges::Operations(operations)),
            ..WorkspaceEdit::default()
        })
    }
}

/// Whether `word` is written in `text` as a whole word — a superset of where
/// it is an identifier, and the cheap test before a file is parsed.
fn writes(text: &str, word: &str) -> bool {
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '$';
    text.match_indices(word).any(|(at, _)| {
        !text[..at].chars().next_back().is_some_and(is_word)
            && !text[at + word.len()..].chars().next().is_some_and(is_word)
    })
}

/// Whether an anonymous class body among `decls` declares a member called
/// `name`, which the index holds no row for.
fn declares_in_body(decls: &[javelin_parser::Decl], name: &str) -> bool {
    decls.iter().any(|decl| {
        (decl.kind == DeclKind::AnonymousClass && decl.children.iter().any(|c| c.name == name))
            || declares_in_body(&decl.children, name)
    })
}

fn file_name(path: &Path) -> String {
    path.file_name().map_or_else(
        || path.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// `File.java:12`, one-based, for a refusal.
fn place(path: &Path, span: Span) -> String {
    format!("{}:{}", file_name(path), span.range.start.line + 1)
}

#[cfg(test)]
mod tests {
    use std::cmp::Reverse;
    use std::collections::BTreeMap;
    use std::time::Duration;

    use javelin_core::Config;
    use lsp_types::{PrepareRenameResponse, RenameParams, TextDocumentPositionParams};

    use super::*;
    use crate::ServerState;
    use crate::classpath::Classpath;
    use crate::handlers::to_position;
    use crate::workspace::testing::{Sources, indexed};

    /// A session over `files` with `open` open, indexed and walked whole,
    /// against a JDK of `Object`, `String` and `InputStream`.
    fn session(name: &str, files: &[(&str, &str)], open: &str) -> (Sources, ServerState, String) {
        let sources = indexed(name, files);
        let mut state = ServerState::new(Config::default()).expect("grammar loads");
        state.install_classpath(Classpath {
            world: crate::member_refs::library_world(),
            ..Classpath::hand_built(&[])
        });
        let uri = file_uri(&sources.path(open))
            .expect("a uri")
            .as_str()
            .to_owned();
        let text = files
            .iter()
            .find(|(path, _)| *path == open)
            .expect("open file")
            .1;
        state.did_open(DocumentId::new(&uri), 1, text.to_owned());
        state.install_workspace(crate::workspace::Workspace {
            index: Arc::clone(&sources.index),
            elapsed: Duration::ZERO,
            refreshed: false,
            survey: Some(Arc::default()),
            absorbed: 0,
        });
        state.takes_document_changes(true);
        (sources, state, uri)
    }

    fn position(source: &str, needle: &str) -> lsp_types::Position {
        let at = source.find(needle).expect("needle in source");
        let line = source[..at].matches('\n').count() as u32;
        let character = (at - source[..at].rfind('\n').map_or(0, |nl| nl + 1)) as u32;
        lsp_types::Position::new(line, character)
    }

    /// Every file after renaming the name at `needle` in the open file to
    /// `new_name`, by path under the root, or the refusal.
    fn rename(
        name: &str,
        files: &[(&str, &str)],
        open: &str,
        needle: &str,
        new_name: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        let (sources, mut state, uri) = session(name, files, open);
        let source = files
            .iter()
            .find(|(path, _)| *path == open)
            .expect("open")
            .1;
        rename_in(&sources, &mut state, &uri, files, source, needle, new_name)
    }

    fn rename_in(
        sources: &Sources,
        state: &mut ServerState,
        uri: &str,
        files: &[(&str, &str)],
        source: &str,
        needle: &str,
        new_name: &str,
    ) -> Result<BTreeMap<String, String>, String> {
        let params: RenameParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": position(source, needle),
            "newName": new_name,
        }))
        .expect("well-formed params");
        let edit = super::super::handle(state, &params, &CancellationToken::new())
            .expect("not cancelled")?;
        Ok(applied(sources, files, edit))
    }

    fn applied(
        sources: &Sources,
        files: &[(&str, &str)],
        edit: WorkspaceEdit,
    ) -> BTreeMap<String, String> {
        let mut out: BTreeMap<String, String> = files
            .iter()
            .map(|(path, text)| ((*path).to_owned(), (*text).to_owned()))
            .collect();
        let relative = |uri: &Uri| {
            crate::workspace::path_of(uri.as_str())
                .expect("a file uri")
                .strip_prefix(&sources.root)
                .expect("under the root")
                .to_string_lossy()
                // The fixtures name files with `/`, which Windows spells `\`.
                .replace('\\', "/")
        };
        let operations = match (edit.document_changes, edit.changes) {
            (Some(DocumentChanges::Operations(operations)), None) => operations,
            (None, Some(changes)) => changes
                .into_iter()
                .map(|(uri, edits)| {
                    DocumentChangeOperation::Edit(TextDocumentEdit {
                        text_document: OptionalVersionedTextDocumentIdentifier {
                            uri,
                            version: None,
                        },
                        edits: edits.into_iter().map(OneOf::Left).collect(),
                    })
                })
                .collect(),
            (None, None) => Vec::new(),
            other => panic!("unexpected {other:?}"),
        };
        for operation in operations {
            match operation {
                DocumentChangeOperation::Edit(edit) => {
                    let text = out
                        .get_mut(&relative(&edit.text_document.uri))
                        .expect("an edited file is a workspace file");
                    let document = TextDocument::new(DocumentId::new("file:///x"), 1, text.clone());
                    let offset = |at| document.offset_at(to_position(at)) as usize;
                    let mut edits: Vec<TextEdit> = edit
                        .edits
                        .into_iter()
                        .map(|edit| match edit {
                            OneOf::Left(edit) => edit,
                            OneOf::Right(annotated) => annotated.text_edit,
                        })
                        .collect();
                    edits.sort_by_key(|edit| Reverse(offset(edit.range.start)));
                    for edit in edits {
                        text.replace_range(
                            offset(edit.range.start)..offset(edit.range.end),
                            &edit.new_text,
                        );
                    }
                }
                DocumentChangeOperation::Op(ResourceOp::Rename(rename)) => {
                    let text = out
                        .remove(&relative(&rename.old_uri))
                        .expect("renamed file");
                    out.insert(relative(&rename.new_uri), text);
                }
                DocumentChangeOperation::Op(other) => panic!("unexpected {other:?}"),
            }
        }
        out
    }

    const GREETER: &str = "package com.example;\npublic class Greeter {\n    \
                           public Greeter() {}\n    public void greet() {}\n    \
                           public static Greeter make() { return new Greeter(); }\n}\n";
    const MAIN: &str = "package com.example.app;\nimport com.example.Greeter;\nclass Main {\n    \
                        void run(Greeter g) {\n        g.greet();\n        \
                        new Greeter().greet();\n        Greeter.make().greet();\n    }\n}\n";

    fn greeter_files() -> [(&'static str, &'static str); 2] {
        [
            ("com/example/Greeter.java", GREETER),
            ("com/example/app/Main.java", MAIN),
        ]
    }

    #[test]
    fn a_public_method_is_renamed_in_every_file_that_calls_it() {
        let out = rename(
            "rename-method",
            &greeter_files(),
            "com/example/app/Main.java",
            "greet();",
            "hello",
        )
        .unwrap();
        assert_eq!(
            out["com/example/Greeter.java"],
            GREETER.replace("greet()", "hello()")
        );
        assert_eq!(
            out["com/example/app/Main.java"],
            MAIN.replace("greet()", "hello()")
        );
    }

    #[test]
    fn an_open_file_edit_carries_its_version_and_a_closed_one_none() {
        let files = greeter_files();
        let (sources, mut state, uri) =
            session("rename-versions", &files, "com/example/app/Main.java");
        let params: RenameParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": position(MAIN, "greet();"),
            "newName": "hello",
        }))
        .unwrap();
        let edit = super::super::handle(&mut state, &params, &CancellationToken::new())
            .unwrap()
            .unwrap();
        let Some(DocumentChanges::Operations(operations)) = edit.document_changes else {
            panic!("document changes were asked for");
        };
        let versions: BTreeMap<String, Option<i32>> = operations
            .into_iter()
            .map(|operation| match operation {
                DocumentChangeOperation::Edit(edit) => {
                    let path = crate::workspace::path_of(edit.text_document.uri.as_str()).unwrap();
                    let relative = path.strip_prefix(&sources.root).unwrap();
                    (
                        relative.to_string_lossy().replace('\\', "/"),
                        edit.text_document.version,
                    )
                }
                other => panic!("unexpected {other:?}"),
            })
            .collect();
        assert_eq!(
            versions,
            BTreeMap::from([
                ("com/example/Greeter.java".to_owned(), None),
                ("com/example/app/Main.java".to_owned(), Some(1)),
            ])
        );
    }

    #[test]
    fn a_same_named_method_elsewhere_is_left_and_a_method_reference_is_renamed() {
        let other = "package com.example;\nclass Other {\n    void greet() {}\n    \
                     void m(Greeter g) { greet(); Runnable r = g::greet; }\n}\n";
        let files = [
            ("com/example/Greeter.java", GREETER),
            ("com/example/app/Main.java", MAIN),
            ("com/example/Other.java", other),
        ];
        let out = rename(
            "rename-elsewhere",
            &files,
            "com/example/app/Main.java",
            "greet();",
            "hello",
        )
        .unwrap();
        assert_eq!(
            out["com/example/Other.java"],
            other.replace("g::greet", "g::hello")
        );
    }

    #[test]
    fn an_anonymous_class_overriding_the_method_refuses_the_rename() {
        let other = "package com.example;\nclass Other {\n    \
                     Greeter m() { return new Greeter() { public void greet() {} }; }\n}\n";
        let files = [
            ("com/example/Greeter.java", GREETER),
            ("com/example/app/Main.java", MAIN),
            ("com/example/Other.java", other),
        ];
        assert_eq!(
            rename(
                "rename-anonymous",
                &files,
                "com/example/app/Main.java",
                "greet();",
                "hello"
            ),
            Err(
                "an anonymous or local class declares a method of the name, which may override \
                 it, at Other.java:3"
                    .to_owned()
            )
        );
    }

    #[test]
    fn an_enum_constant_is_renamed_in_selections_and_case_labels() {
        let color = "package com.example;\npublic enum Color {\n    RED, GREEN\n}\n";
        let user = "package com.example;\nclass User {\n    \
                    int m(Color c) {\n        switch (c) {\n            case RED: return 1;\n            \
                    default: return c == Color.RED ? 2 : 0;\n        }\n    }\n}\n";
        let files = [
            ("com/example/Color.java", color),
            ("com/example/User.java", user),
        ];
        let out = rename(
            "rename-enum-constant",
            &files,
            "com/example/User.java",
            "RED ?",
            "CRIMSON",
        )
        .unwrap();
        assert_eq!(
            out["com/example/Color.java"],
            color.replace("RED", "CRIMSON")
        );
        assert_eq!(out["com/example/User.java"], user.replace("RED", "CRIMSON"));
    }

    #[test]
    fn a_class_is_renamed_with_its_file_its_imports_and_its_constructors() {
        let out = rename(
            "rename-type",
            &greeter_files(),
            "com/example/app/Main.java",
            "Greeter g",
            "Host",
        )
        .unwrap();
        assert!(!out.contains_key("com/example/Greeter.java"));
        assert_eq!(
            out["com/example/Host.java"],
            GREETER.replace("Greeter", "Host")
        );
        assert_eq!(
            out["com/example/app/Main.java"],
            MAIN.replace("Greeter", "Host")
        );
    }

    #[test]
    fn a_constructor_renames_its_class() {
        let (sources, mut state, uri) = session(
            "rename-constructor",
            &greeter_files(),
            "com/example/Greeter.java",
        );
        let out = rename_in(
            &sources,
            &mut state,
            &uri,
            &greeter_files(),
            GREETER,
            "Greeter() {}",
            "Host",
        )
        .unwrap();
        assert_eq!(
            out["com/example/Host.java"],
            GREETER.replace("Greeter", "Host")
        );
    }

    #[test]
    fn a_class_whose_file_the_editor_cannot_rename_is_refused() {
        let (sources, mut state, uri) = session(
            "rename-no-files",
            &greeter_files(),
            "com/example/app/Main.java",
        );
        state.takes_document_changes(false);
        assert_eq!(
            rename_in(
                &sources,
                &mut state,
                &uri,
                &greeter_files(),
                MAIN,
                "Greeter g",
                "Host"
            ),
            Err(
                "the editor cannot rename files, and Greeter.java must become Host.java".to_owned()
            )
        );
        // A method needs no file renamed.
        assert!(
            rename_in(
                &sources,
                &mut state,
                &uri,
                &greeter_files(),
                MAIN,
                "greet();",
                "hello"
            )
            .is_ok()
        );
    }

    #[test]
    fn jdk_and_dependency_declarations_are_refused() {
        let main =
            "package com.example;\nclass Main {\n    int m(String s) { return s.length(); }\n}\n";
        let files = [("com/example/Main.java", main)];
        assert_eq!(
            rename(
                "rename-jdk",
                &files,
                "com/example/Main.java",
                "String s",
                "Text"
            ),
            Err(
                "`java.lang.String` is declared outside the workspace, in the JDK or a dependency"
                    .to_owned()
            )
        );
        assert_eq!(
            rename(
                "rename-jdk-member",
                &files,
                "com/example/Main.java",
                "length",
                "size"
            ),
            Err(
                "`length` is declared outside the workspace, in the JDK or a dependency".to_owned()
            )
        );
    }

    #[test]
    fn an_override_family_is_renamed_together() {
        let shape = "package com.example;\npublic interface Shape {\n    double area();\n}\n";
        let circle = "package com.example;\npublic class Circle implements Shape {\n    \
                      public double area() { return 1; }\n}\n";
        let square = "package com.example;\npublic class Square extends Circle {\n    \
                      @Override public double area() { return 2; }\n    \
                      public double area(int scale) { return scale; }\n}\n";
        let user = "package com.example;\nclass User {\n    \
                    double m(Shape s, Square q) { return s.area() + q.area() + q.area(3); }\n}\n";
        let files = [
            ("com/example/Shape.java", shape),
            ("com/example/Circle.java", circle),
            ("com/example/Square.java", square),
            ("com/example/User.java", user),
        ];
        let out = rename(
            "rename-family",
            &files,
            "com/example/User.java",
            "area() +",
            "size",
        )
        .unwrap();
        assert_eq!(out["com/example/Shape.java"], shape.replace("area", "size"));
        assert_eq!(
            out["com/example/Circle.java"],
            circle.replace("area", "size")
        );
        // The overload of another arity is not the family's.
        assert_eq!(
            out["com/example/Square.java"],
            square.replace("area() {", "size() {")
        );
        assert_eq!(
            out["com/example/User.java"],
            user.replace("area() +", "size() +")
        );
    }

    #[test]
    fn a_method_overriding_a_library_one_is_refused() {
        let stream = "package com.example;\npublic class Stream extends java.io.InputStream {\n    \
                      public int read() { return 0; }\n    public String toString() { return \"\"; }\n}\n";
        let files = [("com/example/Stream.java", stream)];
        assert_eq!(
            rename(
                "rename-library-override",
                &files,
                "com/example/Stream.java",
                "read",
                "next"
            ),
            Err(
                "it may override `java.io.InputStream.read`, which is not the workspace's to \
                 rename"
                    .to_owned()
            )
        );
        assert_eq!(
            rename(
                "rename-object-override",
                &files,
                "com/example/Stream.java",
                "toString",
                "text"
            ),
            Err(
                "it may override `java.lang.Object.toString`, which is not the workspace's to \
                 rename"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_new_name_the_hierarchy_already_has_is_refused() {
        assert_eq!(
            rename(
                "rename-clash",
                &greeter_files(),
                "com/example/app/Main.java",
                "greet();",
                "make"
            ),
            Err("`com.example.Greeter` already has a member named `make`".to_owned())
        );
        assert_eq!(
            rename(
                "rename-type-clash",
                &greeter_files(),
                "com/example/app/Main.java",
                "Greeter g",
                "Main"
            ),
            Err("`Main` is already written at Main.java:3".to_owned())
        );
    }

    #[test]
    fn a_field_is_renamed_through_every_selection_and_a_subclass_bare_use() {
        let base = "package com.example;\npublic class Base {\n    public int count;\n}\n";
        let sub = "package com.example;\nclass Sub extends Base {\n    \
                   int twice() { return count * 2; }\n}\n";
        let user = "package com.example;\nclass User {\n    int m(Base b) { return b.count; }\n}\n";
        let files = [
            ("com/example/Base.java", base),
            ("com/example/Sub.java", sub),
            ("com/example/User.java", user),
        ];
        let out = rename(
            "rename-field",
            &files,
            "com/example/User.java",
            "count;",
            "size",
        )
        .unwrap();
        for (path, text) in files {
            assert_eq!(out[path], text.replace("count", "size"), "{path}");
        }
        // A local of the new name where a bare use is rewritten would capture it.
        let local = "package com.example;\nclass Sub extends Base {\n    \
                     int twice() { int size = 2; return count * size; }\n}\n";
        assert_eq!(
            rename(
                "rename-field-capture",
                &[files[0], ("com/example/Sub.java", local), files[2]],
                "com/example/User.java",
                "count;",
                "size"
            ),
            Err("`size` is already written at Sub.java:3".to_owned())
        );
    }

    #[test]
    fn a_static_import_of_a_method_is_renamed_with_it() {
        let util = "package com.example;\npublic class Util {\n    \
                    public static int twice(int x) { return x * 2; }\n}\n";
        let user = "package com.example.app;\nimport static com.example.Util.twice;\nclass User {\n    \
                    int m() { return twice(2); }\n}\n";
        let files = [
            ("com/example/Util.java", util),
            ("com/example/app/User.java", user),
        ];
        let out = rename(
            "rename-static-import",
            &files,
            "com/example/Util.java",
            "twice",
            "double2",
        )
        .unwrap();
        assert_eq!(
            out["com/example/Util.java"],
            util.replace("twice", "double2")
        );
        assert_eq!(
            out["com/example/app/User.java"],
            user.replace("twice", "double2")
        );
    }

    #[test]
    fn a_use_that_cannot_be_resolved_refuses_the_rename() {
        let other =
            "package com.example;\nclass Other {\n    void m(Unknown u) { u.greet(); }\n}\n";
        let files = [
            ("com/example/Greeter.java", GREETER),
            ("com/example/app/Main.java", MAIN),
            ("com/example/Other.java", other),
        ];
        assert_eq!(
            rename(
                "rename-unresolved",
                &files,
                "com/example/app/Main.java",
                "greet();",
                "hello"
            ),
            Err("a use of the name could not be resolved, at Other.java:3".to_owned())
        );
    }

    #[test]
    fn a_string_naming_the_method_in_its_file_refuses_the_rename() {
        let greeter = "package com.example;\npublic class Greeter {\n    \
                       public void greet() {}\n    String how() { return \"greet\"; }\n}\n";
        let files = [("com/example/Greeter.java", greeter)];
        assert_eq!(
            rename(
                "rename-string",
                &files,
                "com/example/Greeter.java",
                "greet()",
                "hello"
            ),
            Err(
                "a string literal in Greeter.java names `greet`, as a reflective use may"
                    .to_owned()
            )
        );
    }

    #[test]
    fn a_nested_type_is_refused() {
        let outer =
            "package com.example;\npublic class Outer {\n    public static class Inner {}\n}\n";
        let files = [("com/example/Outer.java", outer)];
        assert_eq!(
            rename(
                "rename-nested",
                &files,
                "com/example/Outer.java",
                "Inner",
                "Core"
            ),
            Err("Javelin renames top-level types, not nested ones".to_owned())
        );
    }

    #[test]
    fn nothing_is_renamed_across_files_before_the_walk_is_whole() {
        let (sources, mut state, uri) = session(
            "rename-unready",
            &greeter_files(),
            "com/example/app/Main.java",
        );
        state.install_workspace(crate::workspace::Workspace {
            index: Arc::clone(&sources.index),
            elapsed: Duration::ZERO,
            refreshed: false,
            survey: Some(Arc::new(javelin_index::Survey {
                skipped_large: true,
                ..javelin_index::Survey::default()
            })),
            absorbed: 0,
        });
        assert!(
            rename_in(
                &sources,
                &mut state,
                &uri,
                &greeter_files(),
                MAIN,
                "greet();",
                "hello"
            )
            .unwrap_err()
            .starts_with("the workspace is not indexed yet")
        );
    }

    #[test]
    fn prepare_on_a_use_in_another_file_offers_the_name() {
        let (_sources, mut state, uri) = session(
            "rename-prepare",
            &greeter_files(),
            "com/example/app/Main.java",
        );
        let params: TextDocumentPositionParams = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": uri },
            "position": position(MAIN, "greet();"),
        }))
        .expect("well-formed params");
        let answer = super::super::prepare(&mut state, &params, &CancellationToken::new())
            .expect("not cancelled");
        let Ok(PrepareRenameResponse::RangeWithPlaceholder { range, placeholder }) = answer else {
            panic!("unexpected {answer:?}");
        };
        assert_eq!(placeholder, "greet");
        assert_eq!(range.start, position(MAIN, "greet();"));
    }
}
