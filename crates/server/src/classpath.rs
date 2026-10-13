//! The classpath: the background job that reads it, and the engine's window
//! onto the result.
//!
//! Reading a JDK's `src.zip` costs about eleven seconds. That number is the
//! whole reason this module is shaped the way it is: the read happens on a
//! detached thread started from the `initialized` *notification*, never from
//! `initialize`, and the finished world arrives on a channel the request loop
//! already selects over. Nothing on the startup path opens the zip, spawns a
//! process, or walks a filesystem looking for a JDK.
//!
//! Until it arrives — and forever, on a machine with no JDK — every feature
//! answers from the open buffer alone, which is exactly what Phase 2 answered.

use std::cell::OnceCell;
use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use javelin_classpath::{
    DeclKind, Jdk, MemberFacts, MemberType, Resolution, TypeFacts, TypeWorld, simple_name,
};
use javelin_core::{CancellationToken, Config};
use javelin_index::{CacheDb, WorkspaceIndex};

use crate::modules::Scope;
use javelin_parser::ImportDecl;
use javelin_semantic::{
    ExternalMember, ExternalMembers, ExternalSite, ExternalType, Externals, Primitive,
};

/// The eight primitives and the classes they box to.
///
/// Written out rather than derived so that [`ClasspathExternals::unboxed`] is
/// *total* over the box classes a world holds, which the [`Externals`]
/// contract requires: `apply.rs` excludes a candidate on `unboxed` answering
/// `None`, so a mapping that missed one box class the world does hold would
/// produce a wrong overload pick from an absent fact.
const BOXES: [(Primitive, &str); 8] = [
    (Primitive::Boolean, "java.lang.Boolean"),
    (Primitive::Byte, "java.lang.Byte"),
    (Primitive::Short, "java.lang.Short"),
    (Primitive::Int, "java.lang.Integer"),
    (Primitive::Long, "java.lang.Long"),
    (Primitive::Char, "java.lang.Character"),
    (Primitive::Float, "java.lang.Float"),
    (Primitive::Double, "java.lang.Double"),
];

/// Every fully-qualified name a file may mean: the workspace's own sources and
/// the JDK's, asked as one table.
///
/// One table rather than two tiers, because `simple_name` already has the
/// tiers that matter and they are the language's — a single-type import, then
/// the file's own package, then an on-demand import, then `java.lang`. A
/// sibling in the same package beating `java.lang.Foo` falls out of that
/// ordering for free; resolving against the workspace first and the JDK second
/// would be a *fifth* tier nobody wrote down, and would answer where the
/// language says the name is ambiguous. Two reachable types of one simple name
/// in one tier is [`Resolution::Ambiguous`] whichever side each came from,
/// which is the honest answer.
struct Reachable<'a> {
    workspace: &'a WorkspaceIndex,
    world: &'a TypeWorld,
    scope: Option<&'a Scope>,
}

impl javelin_classpath::TypeTable for Reachable<'_> {
    fn contains(&self, fqn: &str) -> bool {
        self.workspace.with_fqn(fqn).next().is_some()
            || (self.world.contains(fqn) && sees(self.world, self.scope, fqn))
    }
}

/// Whether a type the world holds is on the scope's classpath: the JDK's
/// always, a dependency's when one of the scope's JARs holds it.
fn sees(world: &TypeWorld, scope: Option<&Scope>, fqn: &str) -> bool {
    scope.is_none_or(|scope| world.is_platform(fqn) || scope.sees(fqn))
}

/// The engine's window onto whatever classpath has arrived, and onto whatever
/// the workspace walk has found.
///
/// Holds an `Arc` rather than a borrow so a handler can take one out of
/// [`crate::ServerState`] and still hand the state a `&mut` for the document
/// view it needs alongside it. Cloning is a refcount bump; both halves are
/// immutable and shared by every request.
///
/// [`Externals::resolve`] and [`Externals::members`] read the workspace half.
/// `is_subtype`, `boxed` and `unboxed` answer about the JDK alone, and that is
/// a soundness decision rather than an unfinished one: `apply.rs`'s `(External, Declared)` arm excludes on the
/// world merely *having heard of* a type, and its `may_reach` walks past an
/// external supertype for the same reason — both rest on a fact never being
/// about a file the user can edit. A workspace fqn that answered
/// [`Externals::is_subtype`] would make `class Other extends Bar` in the file
/// next door excludable from a parameter the buffer declares, which Java
/// accepts. Keeping the workspace out of those answers leaves both arms with
/// "cannot say", which is the refusing direction. The `ponytail:` comments on
/// both of them say what a fact carrying its provenance would buy instead.
///
/// Members are on the other side of that line because nothing in `apply.rs`
/// excludes on one: a member makes a candidate *visible* to
/// `sole_candidate`, and a workspace or JAR parameter type reaches
/// `cannot_pass` as an external type outside the platform. That excludes only
/// a JDK argument, whose whole closure is the JDK's, so it cannot reach the
/// parameter whatever the workspace or a JAR declares.
#[derive(Debug, Clone)]
pub struct ClasspathExternals {
    world: Arc<TypeWorld>,
    workspace: Arc<WorkspaceIndex>,
    /// One module's JARs, which every name this window resolves is read
    /// through; `None` for the whole build's.
    scope: Option<Arc<Scope>>,
    /// [`Self::extended_platform`], computed the first time it is asked.
    extended: Arc<std::sync::OnceLock<HashSet<String>>>,
}

impl ClasspathExternals {
    /// A window onto `world` and `workspace`. Public for a caller that holds
    /// both from somewhere other than the background jobs — a benchmark.
    pub fn new(world: Arc<TypeWorld>, workspace: Arc<WorkspaceIndex>) -> Self {
        Self {
            world,
            workspace,
            scope: None,
            extended: Arc::default(),
        }
    }

    /// Whether a use of a member `owner` declares, which has no workspace
    /// site, is worth recording: every JAR type's, and a JDK type's only
    /// where it is [`extended_platform`](Self::extended_platform) — or the
    /// table would hold every `toString` and `List.add` in the workspace.
    ///
    /// Never a type two JARs declare differently: which one's facts are kept
    /// follows the build tool's print order, which the world's key ignores,
    /// so a stored ordinal could name another overload next session.
    pub(crate) fn records_library(&self, owner: &str) -> bool {
        if self.workspace.with_fqn(owner).next().is_some()
            || self.world.facts(owner).is_none()
            || self.world.rivalled(owner)
        {
            return false;
        }
        !self.world.is_platform(owner) || self.extended_platform().contains(owner)
    }

    /// Every JDK type a workspace type writes as a supertype, and every JDK
    /// type above those, `java.lang.Object` aside.
    ///
    /// ponytail: an enum's `Enum` and a record's `Record`, which nobody
    /// writes, are not in it, so `day.name()` is listed in the open file
    /// only. The upgrade is the kind's supertype added per workspace type.
    fn extended_platform(&self) -> &HashSet<String> {
        self.extended.get_or_init(|| {
            let mut extended = HashSet::new();
            let mut queue: VecDeque<String> = self
                .workspace
                .types()
                .iter()
                .flat_map(|ty| self.source_supertypes(ty).0)
                .collect();
            while let Some(fqn) = queue.pop_front() {
                if fqn == "java.lang.Object"
                    || !self.world.is_platform(&fqn)
                    || self.workspace.with_fqn(&fqn).next().is_some()
                    || !extended.insert(fqn.clone())
                {
                    continue;
                }
                if let Some(facts) = self.world.facts(&fqn) {
                    queue.extend(facts.supertypes.iter().cloned());
                }
            }
            extended
        })
    }

    /// The same window, resolving names against one module's JARs: a type
    /// only another module's JAR holds resolves to nothing. Only what a
    /// name *means* narrows — the facts behind a name that resolved, and
    /// subtyping, are the world's as before — so narrower can only turn a
    /// known name unknown, which every reader but the type diagnostic
    /// treats as "cannot say".
    pub(crate) fn scoped(&self, scope: Arc<Scope>) -> Self {
        Self {
            scope: Some(scope),
            extended: Arc::default(),
            ..self.clone()
        }
    }
}

impl Externals for ClasspathExternals {
    /// `handlers::shadowed`: the gate definition and hover put in
    /// front of a type name, put in front of an annotation's too.
    ///
    /// ponytail: `type_name_at`'s third gate, a name the index never read
    /// (`Survey::generated`), is not here, because this window holds no
    /// survey, so a generated `@interface` sibling of an imported one's name
    /// can send a key to the import's element. The upgrade is handing the
    /// survey's generated names to `ClasspathExternals`.
    fn may_shadow_type(&self, name: &str, syntax: &javelin_parser::FileSyntax) -> bool {
        crate::handlers::shadowed(name, syntax, self)
    }

    /// Delegates to the one place this phase decides what a name means, over
    /// the workspace's own sources and the JDK's together.
    ///
    /// [`Resolution::Ambiguous`] and [`Resolution::Unknown`] both answer
    /// `None`, and the trait promises a caller cannot tell them apart: both
    /// mean *answer nothing*, and a caller able to distinguish them would
    /// eventually branch on the difference and guess.
    fn resolve(&self, name: &str, package: Option<&str>, imports: &[ImportDecl]) -> Option<String> {
        let table = Reachable {
            workspace: self.workspace.as_ref(),
            world: self.world.as_ref(),
            scope: self.scope.as_deref(),
        };
        match simple_name(name, package, imports, &table) {
            Resolution::One(fqn) => Some(fqn),
            Resolution::Ambiguous | Resolution::Unknown => None,
        }
    }

    /// Delegates to [`TypeWorld::is_subtype`], which answers `Some(true)` for
    /// `is_subtype(x, x)` on any name it holds — the membership probe
    /// `apply.rs` reads it as. The workspace index is consulted only for a
    /// `to`, which no JDK type's closure reaches; as `from` it is deliberately
    /// not, and the struct doc says what that costs and what it buys.
    fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
        self.world.is_subtype_beside(from, to, |fqn| {
            self.workspace.with_fqn(fqn).next().is_some()
        })
    }

    fn boxed(&self, primitive: Primitive) -> Option<String> {
        let (_, class) = BOXES.iter().find(|(kind, _)| *kind == primitive)?;
        self.holds(class).map(str::to_owned)
    }

    fn unboxed(&self, fqn: &str) -> Option<Primitive> {
        let (primitive, class) = BOXES.iter().find(|(_, class)| *class == fqn)?;
        self.holds(class).map(|_| *primitive)
    }

    /// Every member `fqn` declares and every one it inherits, closed over a
    /// supertype graph that mixes the workspace's own types and the JDK's.
    ///
    /// Each frame is whichever side holds the name — see `ClasspathExternals::frame` for
    /// the precedence and the ambiguity rule — and a workspace frame's member
    /// types and supertypes are resolved as it is visited, against its own
    /// file. A `.` on a receiver declared in the file next door offers that
    /// type's members, its workspace supertypes' and the JDK's above them.
    ///
    /// Depth-first and stack-based, the same shape as
    /// [`TypeWorld::is_subtype`] and spending the same
    /// [`TypeWorld::MAX_VISITS`], for the same reason: the closure above a JDK
    /// type is not bounded by anything the user typed, and this runs on the
    /// request path once per namespace per `.`.
    ///
    /// Where it differs from `is_subtype` is what it does with a gap. That
    /// walk answers `None` the moment the closure stops being showable, since
    /// a *demonstrated* "is not a subtype" needs the whole of it. Here every
    /// frame reached contributes members that are true whatever lies above it,
    /// so a gap lowers `complete` and the walk carries on — a partial list is
    /// useful, a list *claimed* whole is not.
    ///
    /// Every kind a fact carries is one a `.` can select already —
    /// `javelin-classpath`'s reader admits exactly that list and no other — so
    /// nothing here has a kind to drop, and a dropped member would be a
    /// missing answer with nothing to show why.
    ///
    /// ponytail: two walks over one supertype graph, in two crates, because
    /// they collect different things and stop on different conditions —
    /// factoring them into one traversal means a visitor and a control-flow
    /// enum for two callers. The ceiling is that a change to the cycle guard
    /// or the budget has to be made twice; the shared constants are what keep
    /// the second half of that from drifting silently.
    fn members(&self, fqn: &str) -> Option<ExternalMembers> {
        self.closure(fqn, false).0
    }

    fn raw_members(&self, fqn: &str) -> Option<ExternalMembers> {
        self.closure_of(fqn, false, true).0
    }

    /// `fqn`'s own constructors, from the frame [`Externals::members`] starts
    /// at, where its list is whole: the workspace's one copy, in a file that
    /// parsed cleanly, is no processor's output and has nothing Lombok writes
    /// past the model; or the JDK's, from an entry that parsed. A JAR's are
    /// never read. A private one is no other file's to call.
    fn constructors(&self, fqn: &str) -> Option<ExternalMembers> {
        let (members, type_parameters) = match self.frame(fqn)? {
            Frame::Source(ty) => {
                let rows = self.workspace.constructors_of(&ty.fqn);
                // A file that did not parse, or a processor's, gives members
                // that are not `certain`, which no selection vouches for.
                if rows.is_empty() || ty.lombok.unmodelled {
                    return None;
                }
                let members = self.source_members(ty, rows, |member| !member.is_private);
                (members, ty.type_parameters.clone())
            }
            Frame::Jdk(facts) => {
                if facts.constructors.is_empty() || !facts.members_complete {
                    return None;
                }
                let members = facts
                    .constructors
                    .iter()
                    .enumerate()
                    .map(|constructor| external_member(constructor, facts))
                    .collect();
                (members, facts.type_parameters.clone())
            }
        };
        Some(ExternalMembers {
            members: members.into_iter().map(Arc::new).collect(),
            type_parameters,
            complete: true,
        })
    }

    /// What Lombok generates into `fqn` as the file at `file` declares it,
    /// as the index last read that file — a quarter-second behind an open
    /// buffer — with the private ones, since the asker is the declaring
    /// file. Nothing for a file the index does not hold: another file's
    /// type of the same name is not this one.
    fn generated_members(&self, fqn: &str, file: &javelin_semantic::FileId) -> Vec<ExternalMember> {
        // An open buffer is named by its URI, a file the worker reads by
        // its path.
        let id = file.as_str();
        let Some(path) = crate::workspace::path_of(id)
            .or_else(|| (!id.contains("://")).then(|| std::path::PathBuf::from(id)))
        else {
            return Vec::new();
        };
        let mut types = self.workspace.with_fqn(fqn).filter(|ty| ty.file == path);
        match (types.next(), types.next()) {
            (Some(ty), None) => self.source_members(ty, self.workspace.members_of(fqn), |member| {
                member.synthetic
            }),
            _ => Vec::new(),
        }
    }
}

impl ClasspathExternals {
    /// [`Externals::members`], and every type the walk visited or was named
    /// and could not reach — the types whose change could change the answer,
    /// which is what a caller recording dependencies needs beside it. The
    /// names each workspace frame writes are collected only if `recording`,
    /// so the request path's `members` does not build a list it discards.
    pub(crate) fn closure(
        &self,
        fqn: &str,
        recording: bool,
    ) -> (Option<ExternalMembers>, Vec<String>) {
        self.closure_of(fqn, recording, false)
    }

    /// [`Self::closure`], binding no supertype edge when `raw`.
    fn closure_of(
        &self,
        fqn: &str,
        recording: bool,
        raw: bool,
    ) -> (Option<ExternalMembers>, Vec<String>) {
        let Some(root) = self.frame(fqn) else {
            return (None, Vec::new());
        };
        // The root's own members are the floor of what the walk emits, and a
        // god-class receiver reaches thousands of them; sizing to it removes
        // the realloc chain from the path `member_god_warm` measures.
        let mut members = Vec::with_capacity(root.member_count(&self.workspace));
        let mut complete = true;
        // Keyed on the fqn, which is what identifies a type on either side —
        // so a diamond onto the same supertype contributes its members once,
        // while two types that happen to declare the same member name
        // contribute two, exactly as the file-local walk does with an override
        // and the method it overrides.
        let mut seen: Vec<&str> = Vec::new();
        let mut missing: Vec<String> = Vec::new();
        // Every name a visited workspace frame writes: each means what the
        // workspace holds now, and a type declared later can change that.
        let mut written: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let root_fqn = root.fqn();
        let type_parameters = root.type_parameters().to_vec();
        // Each frame carries what its own type parameters mean in the root's:
        // `None` for the root itself, whose variables are its own.
        let mut stack: Vec<(Frame<'_>, Option<Binding>)> = vec![(root, None)];

        while let Some((frame, binding)) = stack.pop() {
            // A cycle is invalid Java and ordinary input from a real archive;
            // drop the repeat rather than truncate the walk.
            if seen.contains(&frame.fqn()) {
                continue;
            }
            if seen.len() >= TypeWorld::MAX_VISITS {
                complete = false;
                break;
            }
            seen.push(frame.fqn());
            // The type asked about is not a supertype of itself, so it keeps
            // its own statics whatever its kind: a receiver naming an
            // interface can select that interface's static methods.
            let inherited = frame.fqn() != root_fqn;
            let (declared, supertypes, frame_complete) = match frame {
                Frame::Jdk(facts) => self.jdk_frame(facts),
                Frame::Source(ty) => {
                    if recording {
                        written.extend(written_names(ty, &self.workspace));
                    }
                    let (declared, supertypes, complete) = self.source_frame(ty);
                    // A workspace copy of a JDK or a JAR type — GWT's
                    // super-source, a stub — may not be the class the program
                    // runs against, so neither list is known to be its. A JAR
                    // the workspace's own build wrote is that source compiled,
                    // as long as the class says nothing the source does not: a
                    // stale JAR or a bytecode enhancer's is still a rival. An
                    // override of `Object`'s adds no name to the closure.
                    let object = self.world.facts(TypeWorld::OBJECT);
                    let rival = self.world.facts(&ty.fqn).is_some_and(|facts| {
                        !(self.world.only_own_output(&ty.fqn)
                            && facts.members_complete
                            && facts.members.iter().all(|compiled| {
                                declared.iter().any(|source| source.name == compiled.name)
                                    || object.is_some_and(|object| {
                                        object.members.iter().any(|o| o.name == compiled.name)
                                    })
                            }))
                    });
                    (declared, supertypes, complete && !rival)
                }
            };
            // A supertype reached with none of its parameters bound — a raw
            // edge, or one the walk cannot read — is inherited erased (JLS
            // 4.8), its instance methods' own variables included.
            let erased = binding.as_ref().is_some_and(|binding| binding.is_empty())
                && !frame.type_parameters().is_empty();
            for mut member in declared {
                if erased && !member.is_static {
                    member.type_parameters.clear();
                }
                if let Some(binding) = &binding {
                    member.declared = bound(&member.declared, binding);
                    for parameter in &mut member.parameters {
                        *parameter = bound(parameter, binding);
                    }
                }
                // JLS 8.4.8: an interface's static method is not inherited, so
                // offering one on an implementer offers code that will not
                // compile. Constants *are* inherited, which is why only
                // methods are skipped.
                //
                // The identical rule to the one `javelin_semantic::members`
                // applies to the file-local walk, in two places because the
                // two have different things to apply it to: there a `Decl`, an
                // owner and a `static` modifier read out of a buffer, here a
                // fact and its kind, with no declaration on either side of the
                // seam to share.
                if inherited
                    && frame.kind() == DeclKind::Interface
                    && member.kind == DeclKind::Method
                    && member.is_static
                {
                    continue;
                }
                members.push(Arc::new(member));
            }
            complete &= frame_complete;
            for supertype in supertypes {
                // A JDK type's supertypes are the JDK's: a workspace that
                // vendors a copy of `java.lang.Object` must not take over the
                // closure of every JDK receiver, which `is_subtype` goes on
                // answering from the JDK's own facts. A dependency's are
                // looked up workspace-first, like a source type's: a JAR
                // compiled against the workspace's own module names it, and
                // the source is newer than any installed copy of it.
                let next = match frame {
                    Frame::Jdk(facts) if self.world.is_platform(&facts.fqn) => {
                        self.world.facts(&supertype).map(Frame::Jdk)
                    }
                    Frame::Jdk(_) | Frame::Source(_) => self.frame(&supertype),
                };
                match next {
                    Some(next) => {
                        let binding = if raw {
                            Binding::new()
                        } else {
                            edge_binding(
                                self.edge_arguments(frame, &supertype).as_deref(),
                                next.type_parameters(),
                                binding.as_ref(),
                            )
                        };
                        stack.push((next, Some(binding)));
                    }
                    // Named by a supertype list, but held nowhere — or held
                    // twice in the workspace, which is the same "cannot say".
                    // Nothing above it can be reached.
                    None => {
                        complete = false;
                        missing.push(supertype);
                    }
                }
            }
        }

        let mut frames: Vec<String> = seen.into_iter().map(str::to_owned).collect();
        frames.extend(missing);
        frames.extend(written);
        (
            Some(ExternalMembers {
                members,
                type_parameters,
                complete,
            }),
            frames,
        )
    }
}

/// What a frame's type parameters mean in the asked type's own variables.
type Binding = std::collections::HashMap<String, ExternalType>;

/// The binding a supertype with parameters `above` is reached with through
/// an edge written with `arguments`, rewritten through the lower frame's own
/// binding. A raw edge — no arguments, or none the walk could read — and one
/// with the wrong number bind nothing, so every parameter above stays
/// unresolved rather than meeting a same-named variable of the asked type.
fn edge_binding(
    arguments: Option<&[ExternalType]>,
    above: &[String],
    binding: Option<&Binding>,
) -> Binding {
    let Some(arguments) = arguments.filter(|arguments| arguments.len() == above.len()) else {
        return Binding::new();
    };
    above
        .iter()
        .cloned()
        .zip(arguments.iter().map(|argument| match binding {
            Some(binding) => bound(argument, binding),
            None => argument.clone(),
        }))
        .collect()
}

/// `ty` with every variable replaced by what `binding` says it means, and one
/// the binding does not name unresolved.
fn bound(ty: &ExternalType, binding: &Binding) -> ExternalType {
    match ty {
        ExternalType::Variable(name) => binding
            .get(name)
            .cloned()
            .unwrap_or_else(|| ExternalType::Unresolved(name.clone())),
        ExternalType::Parameterized(fqn, arguments) => ExternalType::Parameterized(
            fqn.clone(),
            arguments
                .iter()
                .map(|argument| bound(argument, binding))
                .collect(),
        ),
        ExternalType::Extends(inner) => ExternalType::Extends(Box::new(bound(inner, binding))),
        ExternalType::Super(inner) => ExternalType::Super(Box::new(bound(inner, binding))),
        other => other.clone(),
    }
}

/// Every identifier `ty`'s written supertypes and member types spell, and its
/// own simple name — over-inclusive on purpose: a dependency too many costs a
/// re-resolution, one too few a stale reference.
fn written_names(ty: &javelin_index::WorkspaceType, workspace: &WorkspaceIndex) -> Vec<String> {
    let members = workspace.members_of(&ty.fqn);
    let constructors = workspace.constructors_of(&ty.fqn);
    let written = ty.supertypes.iter().map(String::as_str).chain(
        members.iter().chain(constructors).flat_map(|member| {
            member
                .declared
                .as_deref()
                .into_iter()
                .chain(member.parameters.iter().map(String::as_str))
        }),
    );
    let mut names = vec![ty.simple.clone()];
    for text in written {
        names.extend(
            text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .filter(|word| !word.is_empty())
                .map(str::to_owned),
        );
    }
    names
}

/// What a name written in a workspace file means by that file's own
/// declarations, before any package or import is asked.
#[derive(Debug, Clone, PartialEq, Eq)]
enum InFile {
    /// A type the file declares that is in scope where the name is written.
    Declared(String),
    /// The file declares a type of that name, but not one in scope there —
    /// the veto `resolve_type_ref`'s `declares_type_named` applies, so this
    /// walk refuses exactly where the file-local one does.
    Refused,
    /// The file declares nothing of that name; the tiers decide.
    Absent,
}

/// One type the member closure visits: a JDK fact, or a workspace type the
/// index read out of a source file.
#[derive(Debug, Clone, Copy)]
enum Frame<'a> {
    Jdk(&'a TypeFacts),
    Source(&'a javelin_index::WorkspaceType),
}

impl<'a> Frame<'a> {
    fn fqn(&self) -> &'a str {
        match self {
            Frame::Jdk(facts) => &facts.fqn,
            Frame::Source(ty) => &ty.fqn,
        }
    }

    fn kind(&self) -> DeclKind {
        match self {
            Frame::Jdk(facts) => facts.kind,
            Frame::Source(ty) => ty.kind,
        }
    }

    /// The type's own type parameter names, what a variable its members and
    /// supertype edges name means.
    fn type_parameters(&self) -> &'a [String] {
        match self {
            Frame::Jdk(facts) => &facts.type_parameters,
            Frame::Source(ty) => &ty.type_parameters,
        }
    }

    fn member_count(&self, workspace: &WorkspaceIndex) -> usize {
        match self {
            Frame::Jdk(facts) => facts.members.len(),
            Frame::Source(ty) => workspace.members_of(&ty.fqn).len(),
        }
    }
}

impl ClasspathExternals {
    /// The type a member closure means by `fqn`.
    ///
    /// The workspace's, when the index holds exactly one of that name — a
    /// type the user can edit is the one their code means — and the JDK's
    /// otherwise. Two workspace types of one name is the ambiguity rule and
    /// answers `None`, never whichever the walk saw first.
    fn frame<'a>(&'a self, fqn: &str) -> Option<Frame<'a>> {
        let mut declared = self.workspace.with_fqn(fqn);
        match (declared.next(), declared.next()) {
            (Some(one), None) => Some(Frame::Source(one)),
            (Some(_), Some(_)) => None,
            (None, _) => self.world.facts(fqn).map(Frame::Jdk),
        }
    }

    /// What `written` — a type name, dots allowed — means by the declarations
    /// of `file`, read from inside `owner`.
    ///
    /// A type the file declares is in scope where `owner`'s members are
    /// written when it is top-level in the file, or a member type of `owner`
    /// or of a type around it; the innermost such one wins, as JLS 6.4.1 has
    /// it. Inherited member types are not modelled — a type the file declares
    /// that only inheritance would bring into scope is refused, never guessed.
    fn declared_in_file(
        &self,
        written: &str,
        owner: &str,
        file: &std::path::Path,
        package: Option<&str>,
    ) -> InFile {
        let (first, rest) = written.split_once('.').unwrap_or((written, ""));
        let mut declared = self
            .workspace
            .with_simple_name(first)
            .filter(|candidate| candidate.file == file)
            .peekable();
        if declared.peek().is_none() {
            return InFile::Absent;
        }
        let in_scope = declared
            .filter_map(|candidate| {
                let parent = candidate
                    .fqn
                    .strip_suffix(candidate.simple.as_str())?
                    .strip_suffix('.')
                    .unwrap_or("");
                let top_level = parent == package.unwrap_or("");
                let encloses = owner == parent || owner.starts_with(&format!("{parent}."));
                (top_level || encloses).then_some((parent.len(), candidate))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, candidate)| candidate.fqn.clone());
        let Some(base) = in_scope else {
            return InFile::Refused;
        };
        if rest.is_empty() {
            return InFile::Declared(base);
        }
        let full = format!("{base}.{rest}");
        if self.workspace.with_fqn(&full).next().is_some() {
            InFile::Declared(full)
        } else {
            InFile::Refused
        }
    }

    /// Whether a type around `ty` in its file has a supertype, whose member
    /// types are in scope where `ty`'s own supertypes are written. An enum's
    /// counts: `java.lang.Enum` declares one.
    ///
    /// A `@SuperBuilder`'s builder writes its owner's own `extends` with the
    /// builder's name after it, so it is read where the owner writes it: a
    /// name an inherited member type would take there breaks Lombok's code.
    fn inherits_around(&self, ty: &javelin_index::WorkspaceType) -> bool {
        let mut owner = ty.fqn.as_str();
        if ty.synthetic {
            owner = owner.rsplit_once('.').map_or(owner, |(parent, _)| parent);
        }
        while let Some((parent, _)) = owner.rsplit_once('.') {
            let Some(around) = self
                .workspace
                .with_fqn(parent)
                .find(|candidate| candidate.file == ty.file)
            else {
                return false;
            };
            if !around.supertypes.is_empty() || around.kind == DeclKind::Enum {
                return true;
            }
            owner = parent;
        }
        false
    }

    /// A JDK frame's members, its supertypes and whether they are all of them.
    fn jdk_frame(&self, facts: &TypeFacts) -> (Vec<ExternalMember>, Vec<String>, bool) {
        let members = facts
            .members
            .iter()
            .enumerate()
            .map(|member| external_member(member, facts))
            .collect();
        // JLS 4.10.2 puts every reference type under `Object`, and
        // `sources.rs` synthesises the `extends` nobody writes for a class
        // alone — an interface's fact names no supertype at all. Reaching it
        // here is what puts `toString` on an interface receiver, and what
        // stops `complete` claiming a list that is missing every member of
        // `Object`. The same exemption `is_subtype` makes, for the same
        // reason, and the visit set absorbs the frames that name it anyway.
        let implicit = (facts.kind != DeclKind::Class).then(|| TypeWorld::OBJECT.to_owned());
        let supertypes = facts.supertypes.iter().cloned().chain(implicit).collect();
        // A written supertype that resolved to nothing means whatever it
        // declares is missing from this list; the ones that did resolve are
        // still worth walking. A frame whose own members may be short —
        // `TypeFacts::members_complete` — makes the list short the same way.
        (
            members,
            supertypes,
            facts.supertypes_complete && facts.members_complete,
        )
    }

    /// The supertypes a workspace type writes, each resolved against its own
    /// file — its own declarations first, then its package and imports — and
    /// whether every one resolved. The unwritten one, `Object` or its kind's,
    /// is not among them.
    ///
    /// The one resolution both edges read: the member closure walks it
    /// upward, and the hierarchies walk it downward from the other end.
    pub fn source_supertypes(&self, ty: &javelin_index::WorkspaceType) -> (Vec<String>, bool) {
        let table = Reachable {
            workspace: self.workspace.as_ref(),
            world: self.world.as_ref(),
            scope: self.scope.as_deref(),
        };
        let file = self.workspace.file_of(&ty.file);
        let package = file.and_then(|file| file.package.as_deref());
        let imports: &[ImportDecl] = file.map_or(&[], |file| file.imports.as_slice());
        let mut complete = true;
        let mut supertypes = Vec::new();
        for written in &ty.supertypes {
            let stripped = javelin_classpath::written_type_name(written);
            let stripped = stripped.as_str();
            match self.declared_in_file(stripped, &ty.fqn, &ty.file, package) {
                InFile::Declared(fqn) => supertypes.push(fqn),
                InFile::Refused => complete = false,
                InFile::Absent => match simple_name(stripped, package, imports, &table) {
                    Resolution::One(fqn) => supertypes.push(fqn),
                    Resolution::Ambiguous | Resolution::Unknown => complete = false,
                },
            }
        }
        // A processor's output is whatever the last build left, which can be
        // behind the sources it was generated from: it may be navigated, and
        // nothing claims a name is absent from a closure that reaches it.
        // This flag is what every such claim reads — the type diagnostic's
        // `frames` and, through `source_frame`, the member list's wholeness.
        if javelin_index::is_processor_output(&ty.file) {
            complete = false;
        }
        (supertypes, complete)
    }

    /// The supertypes `ty` writes that can be named with certainty, each
    /// with what was written: read as [`in_file`](Self::in_file) reads a
    /// name inside a type argument, so nothing a member type of a frame
    /// around `ty`, a static import or a type variable may mean, and nothing
    /// at all where those frames' closures cannot be shown whole — which a
    /// processor's output's never is. A file that did not parse cleanly
    /// writes nothing certain.
    pub(crate) fn certain_supertypes(
        &self,
        ty: &javelin_index::WorkspaceType,
    ) -> Vec<(String, String)> {
        if !self
            .workspace
            .file_of(&ty.file)
            .is_some_and(|file| file.parsed_cleanly)
        {
            return Vec::new();
        }
        let frames = OnceCell::new();
        let resolve = self.in_file(ty, &frames);
        ty.supertypes
            .iter()
            .filter_map(|written| {
                let name = javelin_classpath::written_type_name(written);
                let fqn = resolve(&name, true)?;
                Some((name, fqn))
            })
            .collect()
    }

    /// The members `fqn` itself declares, with their types resolved — the
    /// one frame of [`Externals::members`]' closure that is the type's own,
    /// for a caller that wants nothing inherited.
    pub fn own_members(&self, fqn: &str) -> Option<Vec<ExternalMember>> {
        Some(match self.frame(fqn)? {
            Frame::Jdk(facts) => self.jdk_frame(facts).0,
            Frame::Source(ty) => self.source_frame(ty).0,
        })
    }

    /// How `name` resolves in a file with this package and these imports —
    /// [`Externals::resolve`] without collapsing ambiguity into unknown, for
    /// the one caller that must tell them apart: a diagnostic may say a name
    /// is unknown and must never say so of an ambiguous one.
    pub(crate) fn resolution(
        &self,
        name: &str,
        package: Option<&str>,
        imports: &[ImportDecl],
    ) -> Resolution {
        let table = Reachable {
            workspace: self.workspace.as_ref(),
            world: self.world.as_ref(),
            scope: self.scope.as_deref(),
        };
        simple_name(name, package, imports, &table)
    }

    /// Whether the workspace or the world holds a type named `fqn`.
    pub(crate) fn knows_type(&self, fqn: &str) -> bool {
        self.workspace.with_fqn(fqn).next().is_some()
            || (self.world.facts(fqn).is_some() && sees(&self.world, self.scope.as_deref(), fqn))
    }

    /// Where the world's fact for `fqn` writes the type's own name — `None`
    /// for a fact with no source to jump into, a class file's, and for one
    /// read before names were sited.
    ///
    /// Asks the world alone. A name the workspace also declares is the
    /// workspace's to place, and a caller asks the index first.
    pub(crate) fn type_site(&self, fqn: &str) -> Option<ExternalSite> {
        let facts = self.world.facts(fqn)?;
        (!facts.source.is_empty() && facts.name_end > facts.name_start).then(|| ExternalSite {
            doc_span: facts.doc_span,
            source: facts.source.clone(),
            name_start: facts.name_start,
            name_end: facts.name_end,
            name_range: None,
        })
    }

    /// Whether some type the workspace or the world holds lies under `prefix`.
    pub(crate) fn holds_prefix(&self, prefix: &str) -> bool {
        self.workspace.holds_prefix(prefix) || self.world.holds_prefix(prefix)
    }

    /// The JDK's and the dependencies' facts this window reads.
    pub(crate) fn world(&self) -> &TypeWorld {
        &self.world
    }

    /// The workspace index this window reads.
    pub fn workspace(&self) -> &WorkspaceIndex {
        &self.workspace
    }

    /// The type arguments `frame` wrote on its edge to `supertype`, in its own
    /// variables; `None` for a raw edge or one the walk cannot read.
    fn edge_arguments(&self, frame: Frame<'_>, supertype: &str) -> Option<Vec<ExternalType>> {
        match frame {
            Frame::Jdk(facts) => facts
                .supertype_arguments
                .iter()
                .find(|(fqn, _)| fqn == supertype)
                .map(|(_, arguments)| arguments.iter().map(member_type).collect()),
            Frame::Source(ty) => {
                let frames = OnceCell::new();
                let resolve = self.in_file(ty, &frames);
                ty.supertypes
                    .iter()
                    .filter(|written| written.contains('<'))
                    .find_map(|written| {
                        match javelin_classpath::resolve_generic_with(
                            written,
                            &ty.type_parameters,
                            &[],
                            &resolve,
                        ) {
                            MemberType::Parameterized(fqn, arguments) if fqn == supertype => {
                                Some(arguments.iter().map(member_type).collect())
                            }
                            _ => None,
                        }
                    })
            }
        }
    }

    /// What a type name written inside `ty` means: the declaring file's own
    /// declarations first, the way the file-local walk reads its own text,
    /// then its package and imports — and nothing a static import may name,
    /// which those tiers do not know.
    ///
    /// Refused besides: a type variable of a type around `ty`, which an inner
    /// class may name; and a name a member type of a frame of `ty`'s closures,
    /// or of a type around it, may mean (JLS 6.4.1) — inside a type argument,
    /// any name the tiers read where such a closure is not whole. A top-level
    /// name keeps the tiers' reading there, as it had before arguments were
    /// read: refusing it would cost every member of a type whose supertype
    /// the world does not hold.
    fn in_file<'s>(
        &'s self,
        ty: &'s javelin_index::WorkspaceType,
        frames: &'s OnceCell<Option<Vec<String>>>,
    ) -> impl Fn(&str, bool) -> Option<String> + 's {
        let file = self.workspace.file_of(&ty.file);
        let package = file.and_then(|file| file.package.as_deref());
        let imports: &[ImportDecl] = file.map_or(&[], |file| file.imports.as_slice());
        let around = self.around(ty);
        let variables: Vec<&str> = around[1..]
            .iter()
            .copied()
            .flat_map(|outer| outer.type_parameters.iter().map(String::as_str))
            .collect();
        move |name: &str, in_argument: bool| {
            let first = name.split('.').next().unwrap_or(name);
            if variables.contains(&first) {
                return None;
            }
            // A member type named `first` of any frame but `except`, the type
            // the name was found declared as.
            let inherited_but = |except: &str| match self.frames_around(ty, frames) {
                Some(frames) => frames.iter().any(|frame| {
                    let member = format!("{frame}.{first}");
                    member != except && self.knows_type(&member)
                }),
                None => in_argument,
            };
            let inherited = || inherited_but("");
            match self.declared_in_file(name, &ty.fqn, &ty.file, package) {
                InFile::Declared(fqn) => {
                    // Only `ty`'s own member type outranks one `ty` or a type
                    // around it inherits (JLS 6.4.1); one declared further out
                    // does not.
                    let own = format!("{}.{first}", ty.fqn);
                    let own = fqn == own || fqn.starts_with(&format!("{own}."));
                    let declared = fqn.strip_suffix(&name[first.len()..]).unwrap_or("");
                    (own || !inherited_but(declared)).then_some(fqn)
                }
                InFile::Refused => None,
                InFile::Absent if static_import_may_name(imports, name) || inherited() => None,
                InFile::Absent => {
                    let table = Reachable {
                        workspace: self.workspace.as_ref(),
                        world: self.world.as_ref(),
                        scope: self.scope.as_deref(),
                    };
                    match simple_name(name, package, imports, &table) {
                        Resolution::One(fqn) => Some(fqn),
                        Resolution::Ambiguous | Resolution::Unknown => None,
                    }
                }
            }
        }
    }

    /// [`closure_frames`](Self::closure_frames) of `ty` and the types around
    /// it, walked on the first ask: an edge with no arguments, and a name a
    /// variable or the file settles, never needs it.
    fn frames_around<'c>(
        &self,
        ty: &javelin_index::WorkspaceType,
        frames: &'c OnceCell<Option<Vec<String>>>,
    ) -> &'c Option<Vec<String>> {
        frames.get_or_init(|| self.closure_frames(&self.around(ty)))
    }

    /// `ty` and every type around it in its file, innermost first.
    fn around<'s>(
        &'s self,
        ty: &'s javelin_index::WorkspaceType,
    ) -> Vec<&'s javelin_index::WorkspaceType> {
        let mut chain = vec![ty];
        let mut owner = ty.fqn.as_str();
        while let Some((parent, _)) = owner.rsplit_once('.')
            && let Some(outer) = self
                .workspace
                .with_fqn(parent)
                .find(|candidate| candidate.file == ty.file)
        {
            chain.push(outer);
            owner = parent;
        }
        chain
    }

    /// Every type in the supertype closures of `types`, themselves included,
    /// whose member types are in scope inside the innermost — `None` where a
    /// closure cannot be shown whole.
    fn closure_frames(&self, types: &[&javelin_index::WorkspaceType]) -> Option<Vec<String>> {
        let mut seen: HashSet<String> = HashSet::new();
        let mut queue: VecDeque<String> = types.iter().map(|ty| ty.fqn.clone()).collect();
        while let Some(fqn) = queue.pop_front() {
            if !seen.insert(fqn.clone()) {
                continue;
            }
            if seen.len() > TypeWorld::MAX_VISITS {
                return None;
            }
            let (supertypes, kind) = match self.frame(&fqn)? {
                Frame::Source(ty) => {
                    let (supertypes, complete) = self.source_supertypes(ty);
                    if !complete {
                        return None;
                    }
                    (supertypes, ty.kind)
                }
                Frame::Jdk(facts) => {
                    // An enum's, a record's and an annotation type's unwritten
                    // supertype is the one drop the reader makes on purpose.
                    if !facts.supertypes_complete
                        && !matches!(
                            facts.kind,
                            DeclKind::Enum | DeclKind::Record | DeclKind::AnnotationType
                        )
                    {
                        return None;
                    }
                    (facts.supertypes.clone(), facts.kind)
                }
            };
            queue.extend(supertypes);
            queue.extend(
                match kind {
                    DeclKind::Enum => Some("java.lang.Enum"),
                    DeclKind::Record => Some("java.lang.Record"),
                    DeclKind::AnnotationType => Some("java.lang.annotation.Annotation"),
                    _ => None,
                }
                .map(str::to_owned),
            );
        }
        Some(seen.into_iter().collect())
    }

    /// A workspace frame's members, its supertypes and whether they are all of
    /// them — every written type resolved now, against the declaring file's
    /// own package and imports, through the table `resolve` answers from.
    ///
    /// Resolved at query time rather than on the walk's thread for the reason
    /// references' occurrences are: what a simple name means depends on every
    /// type the workspace and the JDK hold, and both change after the walk.
    fn source_frame(
        &self,
        ty: &javelin_index::WorkspaceType,
    ) -> (Vec<ExternalMember>, Vec<String>, bool) {
        let file = self.workspace.file_of(&ty.file);
        let package = file.and_then(|file| file.package.as_deref());
        let imports: &[ImportDecl] = file.map_or(&[], |file| file.imports.as_slice());
        // A private member is listed by a symbol search and selectable from
        // no other file, which is every question this door answers.
        let members = self.source_members(ty, self.workspace.members_of(&ty.fqn), |member| {
            !member.is_private
        });

        let (mut supertypes, mut complete) = self.source_supertypes(ty);
        // A written supertype the file does not declare is read through the
        // tiers, which know neither the member types an enclosing type
        // inherits nor the names a static import brings in — every name, on
        // demand, and its own name alone, singly (JLS 7.5.3).
        let by_tiers: Vec<String> = ty
            .supertypes
            .iter()
            .map(|written| javelin_classpath::written_type_name(written))
            .filter(|stripped| {
                self.declared_in_file(stripped, &ty.fqn, &ty.file, package) == InFile::Absent
            })
            .collect();
        if by_tiers
            .iter()
            .any(|stripped| static_import_may_name(imports, stripped))
            || (!by_tiers.is_empty() && self.inherits_around(ty))
        {
            complete = false;
        }
        // Error recovery can drop a member a broken file declares.
        complete &= file.is_some_and(|file| file.parsed_cleanly);
        // Lombok may write a member the walk did not generate.
        complete &= !ty.lombok.unmodelled;
        // The supertype nobody writes. `Object` for every class — whether or
        // not it wrote an `extends`, since the index keeps `extends` and
        // `implements` in one list and the visit set drops a second arrival —
        // and for every interface; JLS 8.9, 8.10 and 9.6 for the other three.
        // An enum's `values()` and `valueOf` and a record's accessors are
        // among its members, generated by the walk.
        let implicit = match ty.kind {
            DeclKind::Enum => "java.lang.Enum",
            DeclKind::Record => "java.lang.Record",
            DeclKind::AnnotationType => "java.lang.annotation.Annotation",
            _ => TypeWorld::OBJECT,
        };
        supertypes.push(implicit.to_owned());
        (members, supertypes, complete)
    }

    /// The rows of `ty` that `keep` keeps — its indexed members or its
    /// constructors — their written types resolved in `ty`'s own file and
    /// sited where the index saw each one.
    fn source_members(
        &self,
        ty: &javelin_index::WorkspaceType,
        rows: &[javelin_index::WorkspaceMember],
        keep: impl Fn(&javelin_index::WorkspaceMember) -> bool,
    ) -> Vec<ExternalMember> {
        let frames = OnceCell::new();
        let in_file = self.in_file(ty, &frames);
        // A written type in this file, from inside `ty`, its arguments too:
        // the owner's variables are `Variable`s the closure binds, and a
        // generic method's own stay unresolved for a call to bind.
        let resolve = |written: Option<&str>, owner: &[String], own: &[String]| match written {
            Some(written) => member_type(&javelin_classpath::resolve_generic_with(
                written, owner, own, &in_file,
            )),
            None => ExternalType::Unresolved(String::new()),
        };
        // `in_file` refuses every name an inherited member type, a variable
        // around `ty` or a static import may mean — as long as the closures
        // it reads are whole. A broken file may not say what it wrote.
        let certain = self
            .workspace
            .file_of(&ty.file)
            .is_some_and(|file| file.parsed_cleanly)
            && self.frames_around(ty, &frames).is_some();
        let source = ty.file.to_string_lossy().into_owned();
        rows.iter()
            .enumerate()
            .filter(|(_, member)| keep(member))
            .map(|(ordinal, member)| {
                // A member's scope is its owner's parameters and then its own;
                // one that does not start with the type's — a generated
                // builder's — has every name its own, which binds nothing.
                let owner: &[String] = if member.type_parameters.starts_with(&ty.type_parameters) {
                    &ty.type_parameters
                } else {
                    &[]
                };
                let own = &member.type_parameters[owner.len()..];
                (ordinal, member, owner, own)
            })
            .map(|(ordinal, member, owner, own)| ExternalMember {
                type_parameters: own.to_vec(),
                parameter_names: member.parameter_names.clone(),
                name: member.name.clone(),
                kind: member.kind,
                is_static: member.is_static,
                // ponytail: a workspace member's abstractness is not indexed;
                // `members::lambda_parameter` refuses any closure holding a
                // workspace member for that reason. The upgrade is a column
                // beside `is_static`.
                is_abstract: false,
                is_public: member.is_public,
                certain,
                // An enum constant's type is the enum, which nobody writes
                // down — the rule `sources.rs` applies to the JDK's.
                declared: if member.kind == DeclKind::EnumConstant {
                    ExternalType::Class(ty.fqn.clone())
                } else {
                    resolve(member.declared.as_deref(), owner, own)
                },
                parameters: member
                    .parameters
                    .iter()
                    .map(|written| resolve(Some(written), owner, own))
                    .collect(),
                variadic: member.variadic,
                owner: ty.fqn.clone(),
                ordinal: ordinal as u32,
                site: Some(ExternalSite {
                    doc_span: member.doc_span,
                    source: source.clone(),
                    name_start: member.name_start,
                    name_end: member.name_end,
                    name_range: Some(member.name_range),
                }),
            })
            .collect()
    }
}

/// One member fact as the engine's member, declared by `frame`.
///
/// `frame` is the type the walk read the member off rather than the type the
/// question was asked about: its `fqn` is what own-shadows-inherited compares
/// (see [`Externals::members`] for what filling `owner` in with the latter
/// would cost), and its `source` is where the member's own `site` must point —
/// a member found while visiting a supertype is declared in the supertype's
/// entry, never the receiver's.
fn external_member((ordinal, member): (usize, &MemberFacts), frame: &TypeFacts) -> ExternalMember {
    ExternalMember {
        type_parameters: member.type_parameters.clone(),
        parameter_names: member.parameter_names.clone(),
        name: member.name.clone(),
        kind: member.kind,
        is_static: member.is_static,
        is_abstract: member.is_abstract,
        is_public: member.is_public,
        certain: false,
        declared: member_type(&member.declared),
        parameters: member.parameters.iter().map(member_type).collect(),
        variadic: member.variadic,
        owner: frame.fqn.clone(),
        ordinal: ordinal as u32,
        // A fact read out of a class file has no source to jump into, and
        // one a sources JAR could not tell from an overload has no span.
        site: (!frame.source.is_empty() && member.name_end > member.name_start).then(|| {
            ExternalSite {
                doc_span: member.doc_span,
                source: frame.source.clone(),
                name_start: member.name_start,
                name_end: member.name_end,
                name_range: None,
            }
        }),
    }
}

/// A fact's member type as the engine's, which is the keyword mapping and
/// nothing else: the two enums are the same shape because
/// `javelin-classpath` cannot name a [`Primitive`] and the engine cannot read
/// a fact.
fn member_type(ty: &MemberType) -> ExternalType {
    match ty {
        MemberType::Void => ExternalType::Void,
        MemberType::Primitive(keyword) => primitive(keyword).map_or_else(
            // A keyword this mapping does not know says nothing about the type
            // rather than guessing at one, and carries the text for whatever
            // renders it — the same claim a `MemberType::Unresolved` makes.
            || ExternalType::Unresolved(keyword.clone()),
            ExternalType::Primitive,
        ),
        MemberType::Class(fqn) => ExternalType::Class(fqn.clone()),
        MemberType::Parameterized(fqn, arguments) => {
            ExternalType::Parameterized(fqn.clone(), arguments.iter().map(member_type).collect())
        }
        MemberType::Variable(name) => ExternalType::Variable(name.clone()),
        MemberType::Extends(bound) => ExternalType::Extends(Box::new(member_type(bound))),
        MemberType::Super(bound) => ExternalType::Super(Box::new(member_type(bound))),
        MemberType::Unresolved(written) => ExternalType::Unresolved(written.clone()),
    }
}

/// The primitive a keyword names, read off [`BOXES`] — the one list of the
/// eight in this file, so a primitive cannot be in the box mapping and missing
/// from this one.
fn primitive(keyword: &str) -> Option<Primitive> {
    BOXES
        .iter()
        .map(|(primitive, _)| *primitive)
        .find(|primitive| primitive.keyword() == keyword)
}

impl ClasspathExternals {
    /// `class` back, but only when this world actually holds it.
    ///
    /// A world built from a JDK with no `src.zip` knows no `java.lang.Integer`
    /// and must not be used to exclude anything on the strength of a name it
    /// cannot see.
    fn holds(&self, class: &'static str) -> Option<&'static str> {
        self.world.facts(class).map(|_| class)
    }
}

/// A finished classpath read, and what it cost.
#[derive(Debug)]
pub(crate) struct Classpath {
    pub world: Arc<TypeWorld>,
    pub elapsed: Duration,
    /// `"cache"` or `"src.zip"` — the difference between a tenth of a second
    /// and eleven of them, and the first thing to look at when a warm start
    /// is slow.
    pub source: &'static str,
    /// The `src.zip` these facts describe and the JDK version it belongs to,
    /// which is where a jump into the JDK reads its file from and what keys
    /// the directory it is extracted into. Carried on the cache-hit path as
    /// well, because a warm start otherwise knows every member's entry name
    /// and has no archive to open it in. `None` for a hand-built classpath and
    /// for a JDK with no `lib/src.zip`, both of which degrade to no jump.
    pub sources: Option<(PathBuf, String)>,
    /// How the dependencies came out, on the thread's last message; `None`
    /// while they may still be coming.
    pub dependencies: Option<DependencyReport>,
}

/// What diagnostics need to know about the dependencies once they are read.
#[derive(Debug, Clone)]
pub(crate) struct DependencyReport {
    /// See `dependencies::Dependencies::complete`.
    pub complete: bool,
    /// See `dependencies::Dependencies::processors`.
    pub processors: bool,
    /// See `dependencies::Dependencies::loose`.
    pub loose: bool,
    /// See `dependencies::Dependencies::codegen`.
    pub codegen: bool,
    /// See `dependencies::Dependencies::rewrites`.
    pub rewrites: bool,
    /// See `dependencies::Dependencies::release`.
    pub release: Option<(u32, u32)>,
    /// See `dependencies::Dependencies::modules`.
    pub modules: Option<Arc<crate::modules::Modules>>,
    /// The settled world's key: the JDK's sources and the dependencies'
    /// key, which member references record what they were resolved against by.
    pub key: u64,
}

#[cfg(test)]
impl Classpath {
    /// A world holding exactly these names, as if a read had just finished.
    ///
    /// Never a real JDK: a unit test that read one would depend on the machine
    /// it runs on, and on that machine having a `src.zip`.
    pub(crate) fn hand_built(fqns: &[&str]) -> Self {
        Self::holding(fqns.iter().copied().map(fact).collect())
    }

    /// The same, for a test that needs the facts themselves — a kind,
    /// supertypes, a completeness flag or members, which is everything the
    /// member closure turns on and none of which a bare name can say.
    pub(crate) fn holding(facts: Vec<javelin_classpath::TypeFacts>) -> Self {
        Self {
            world: Arc::new(TypeWorld::new(facts)),
            elapsed: Duration::ZERO,
            source: "hand-built",
            sources: None,
            dependencies: None,
        }
    }
}

#[cfg(test)]
pub(crate) fn fact(fqn: &str) -> javelin_classpath::TypeFacts {
    javelin_classpath::TypeFacts {
        constructors: Vec::new(),
        doc_span: None,
        name_start: 0,
        name_end: 0,
        fqn: fqn.to_owned(),
        simple: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
        kind: DeclKind::Class,
        type_parameters: Vec::new(),
        supertype_arguments: Vec::new(),
        supertypes: Vec::new(),
        supertypes_complete: true,
        members_complete: true,
        members: Vec::new(),
        // Hand-built, so there is no zip entry to name; empty is the honest
        // value rather than a made-up one.
        source: String::new(),
    }
}

/// A fact carrying what the member closure walks. `supertypes_complete` is
/// true; the one test that wants it false sets it on the returned value.
#[cfg(test)]
fn declaring(
    fqn: &str,
    kind: DeclKind,
    supertypes: &[&str],
    members: Vec<MemberFacts>,
) -> javelin_classpath::TypeFacts {
    javelin_classpath::TypeFacts {
        name_start: 0,
        name_end: 0,
        kind,
        supertypes: supertypes.iter().map(|name| (*name).to_owned()).collect(),
        members,
        ..fact(fqn)
    }
}

/// Where the JDK's type facts are cached.
///
/// Under the cache root rather than a per-workspace directory: the facts are
/// keyed by JDK identity, not by project, and one read of `src.zip` should
/// warm every workspace on the machine.
pub fn cache_path(config: &Config) -> javelin_core::Result<PathBuf> {
    Ok(javelin_core::paths::cache_root(config.index.cache_dir.as_deref())?.join("jdk.sqlite"))
}

/// Start the read on a thread of its own, if it is wanted at all. Answers
/// whether a thread was started, which is the only thing a caller can check
/// without waiting for a read that takes seconds.
///
/// The thread is deliberately never joined. `read_type_facts` can be seconds
/// from finishing when the user closes the editor, and a shutdown that waited
/// for it — or for the `CacheDb` it holds — is the "server did not shut down"
/// dialog. `cancel` makes it stop promptly if the process outlives the
/// session; nothing waits to find out.
pub(crate) fn spawn(
    config: &Config,
    roots: Vec<PathBuf>,
    cancel: CancellationToken,
    child: javelin_build::ChildSlot,
    results: Sender<Classpath>,
    progress: crate::progress::Reporter,
) -> bool {
    if !config.index.background_indexing {
        tracing::info!("classpath: background indexing is off, nothing will be read");
        return false;
    }
    let config = config.clone();
    match std::thread::Builder::new()
        .name("javelin-classpath".to_owned())
        .spawn(move || {
            let started = Instant::now();
            let jdk =
                javelin_classpath::discover(config.jdk.home.as_deref(), &config.jdk.search_paths);
            let published = run(&config, jdk, &cancel, &results, &progress);
            // The JDK's world is out; the build tool can take minutes now
            // without delaying anything it answers.
            let asking = config
                .build
                .import_on_startup
                .then(|| progress.begin("Reading the build's classpath"));
            let mut dependencies = crate::dependencies::read(&config, &roots, &cancel, &child);
            drop(asking);
            let mut owed = std::mem::take(&mut dependencies.owed);
            let mut unsourced = dependencies.unsourced.take();
            let own_output = std::mem::take(&mut dependencies.own_output);
            // ponytail: with a fetch to wait for, every dependency fact is
            // held twice for as long as the build tool runs. The upgrade is
            // cloning only the unsourced JARs' ranges.
            let mut regrafted =
                (!owed.is_empty() || unsourced.is_some()).then(|| dependencies.facts.clone());
            let base = published.as_ref().map(|(jdk, _)| Arc::clone(jdk));
            if cancel.is_cancelled() {
                return;
            }
            let key = {
                use std::hash::{Hash, Hasher};
                let mut hasher = std::hash::DefaultHasher::new();
                published
                    .as_ref()
                    .map(|(_, sources)| sources)
                    .hash(&mut hasher);
                dependencies.key.hash(&mut hasher);
                hasher.finish()
            };
            let report = Some(DependencyReport {
                key,
                complete: dependencies.complete,
                processors: dependencies.processors,
                loose: dependencies.loose,
                codegen: dependencies.codegen,
                rewrites: dependencies.rewrites,
                release: dependencies.release,
                modules: dependencies.modules.clone(),
            });
            // Always a last message, so the loop learns how whole the
            // dependencies are even when they added nothing.
            let (world, sources) = match (published, dependencies.facts.is_empty()) {
                (Some((jdk, sources)), true) => (jdk, sources),
                (Some((jdk, sources)), false) => (
                    Arc::new(
                        jdk.extended(dependencies.facts)
                            .with_own_output(own_output.clone()),
                    ),
                    sources,
                ),
                (None, _) => (
                    Arc::new(
                        TypeWorld::empty()
                            .extended(dependencies.facts)
                            .with_own_output(own_output.clone()),
                    ),
                    None,
                ),
            };
            send(
                &results,
                world,
                started,
                "dependencies",
                sources.clone(),
                report.clone(),
            );
            // Then where each owed JAR's names are written, and the world
            // again with them; then the same for what the build tool can
            // fetch, which may take a while and so comes second.
            if let Some(facts) = &mut regrafted {
                loop {
                    let grafting = progress.begin("Reading library sources");
                    if !owed.is_empty()
                        && crate::dependencies::graft(&config, &owed, facts, &cancel) > 0
                        && !cancel.is_cancelled()
                    {
                        let facts = if unsourced.is_some() {
                            facts.clone()
                        } else {
                            std::mem::take(facts)
                        };
                        let world = match &base {
                            Some(jdk) => jdk.extended(facts),
                            None => TypeWorld::empty().extended(facts),
                        }
                        .with_own_output(own_output.clone());
                        send(
                            &results,
                            Arc::new(world),
                            started,
                            "sources",
                            sources.clone(),
                            report.clone(),
                        );
                    }
                    drop(grafting);
                    let Some(asking) = unsourced.take() else {
                        break;
                    };
                    let _fetching = progress.begin("Downloading library sources");
                    owed = crate::dependencies::fetch_sources(asking, &cancel, &child);
                    if cancel.is_cancelled() {
                        break;
                    }
                }
            }
        }) {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, "classpath: could not start the reader thread");
            false
        }
    }
}

/// Discover-free so a test can drive the no-JDK path, and the wrong-JDK path,
/// without depending on what the machine running it has installed.
///
/// Answers what it published, for the dependencies to extend.
fn run(
    config: &Config,
    jdk: Option<Jdk>,
    cancel: &CancellationToken,
    results: &Sender<Classpath>,
    progress: &crate::progress::Reporter,
) -> Option<Published> {
    let started = Instant::now();
    // A configured home that discovery did not take is a typo or a moved
    // installation, and `discover` would silently fall through to `$JAVA_HOME`
    // and the built-in locations. Reading some other JDK is worse than reading
    // none: it feeds overload elimination from a JDK the user did not choose.
    if let Some(configured) = &config.jdk.home
        && jdk.as_ref().map(|jdk| &jdk.home) != Some(configured)
    {
        tracing::warn!(
            configured = %configured.display(),
            "classpath: jdk.home is not a JDK; not falling back to another one"
        );
        return None;
    }
    let Some(jdk) = jdk else {
        tracing::info!("classpath: no JDK found");
        return None;
    };
    let (home, version) = identity(&jdk);
    tracing::info!(jdk = %home, version = %version, "classpath: reading");

    // `persist` is the user saying "do not reuse an index from a previous
    // session", so it governs both directions: no read, and nothing written
    // for a later session to read either.
    let mut db = config.index.persist.then(|| open_cache(config)).flatten();
    if let Some(facts) = db.as_ref().and_then(|db| cached(db, &home, &version)) {
        let sources = jdk.sources.clone().map(|path| (path, version.clone()));
        return Some(send(
            results,
            Arc::new(TypeWorld::new(facts)),
            started,
            "cache",
            sources,
            None,
        ));
    }

    let Some(sources) = &jdk.sources else {
        tracing::info!(jdk = %home, "classpath: no lib/src.zip, so no type facts");
        return None;
    };
    let _reading = progress.begin("Reading the JDK");
    let facts =
        match javelin_classpath::read_type_facts(sources, config.index.max_file_size_bytes, cancel)
        {
            Ok(facts) => facts,
            // A corrupt archive fails the whole read by design; degrading to the
            // empty world is this caller's job, and it is never fatal.
            Err(err) if cancel.is_cancelled() => {
                tracing::debug!(%err, "classpath: read cancelled");
                return None;
            }
            Err(err) => {
                tracing::warn!(src_zip = %sources.display(), %err, "classpath: read failed");
                return None;
            }
        };

    // An empty read is not an empty classpath: publishing one would raise
    // readiness to `Dependencies` on the strength of a world that can answer
    // nothing, and claim every answer complete.
    if facts.is_empty() {
        tracing::warn!(src_zip = %sources.display(), "classpath: no type facts in the sources");
        return None;
    }
    if let Some(db) = &mut db {
        // A cache that could not be written costs the next session its warm
        // start and this one nothing.
        if let Err(err) = db.write_types(&home, &version, &facts) {
            tracing::warn!(%err, "classpath: could not cache the type facts");
        }
    }
    Some(send(
        results,
        Arc::new(TypeWorld::new(facts)),
        started,
        "src.zip",
        Some((sources.clone(), version.clone())),
        None,
    ))
}

/// A world the classpath thread sent, and the JDK sources beside it.
type Published = (Arc<TypeWorld>, Option<(PathBuf, String)>);

fn send(
    results: &Sender<Classpath>,
    world: Arc<TypeWorld>,
    started: Instant,
    source: &'static str,
    sources: Option<(PathBuf, String)>,
    dependencies: Option<DependencyReport>,
) -> Published {
    let classpath = Classpath {
        world: Arc::clone(&world),
        elapsed: started.elapsed(),
        source,
        sources: sources.clone(),
        dependencies,
    };
    // The loop is gone: the session ended while this was being read, and the
    // answer is no longer wanted by anyone.
    let _ = results.send(classpath);
    (world, sources)
}

fn open_cache(config: &Config) -> Option<CacheDb> {
    let path = cache_path(config)
        .inspect_err(|err| tracing::warn!(%err, "classpath: no cache directory"))
        .ok()?;
    CacheDb::open(&path)
        .inspect_err(
            |err| tracing::warn!(path = %path.display(), %err, "classpath: cache unusable"),
        )
        .ok()
}

/// The home and version a cache stores this JDK's facts under.
///
/// One function so the writer, the reader and `doctor` cannot disagree about
/// what identifies a JDK — a mismatch here silently re-reads `src.zip` on
/// every start, which is eleven seconds nobody would look for.
fn identity(jdk: &Jdk) -> (String, String) {
    (
        jdk.home.to_string_lossy().into_owned(),
        jdk.version
            .map_or_else(|| "unknown".to_owned(), |version| version.to_string()),
    )
}

fn stores(db: &CacheDb, home: &str, version: &str) -> bool {
    let stored = |key: &str| db.metadata(key).ok().flatten();
    stored("jdk.home").as_deref() == Some(home) && stored("jdk.version").as_deref() == Some(version)
}

/// The cached facts, but only when they describe the JDK that was just found.
///
/// An empty result counts as a miss rather than as an empty classpath: a JDK
/// whose read produced nothing is indistinguishable from one that was never
/// read, and re-reading is the honest answer to that.
pub(crate) fn cached(
    db: &CacheDb,
    home: &str,
    version: &str,
) -> Option<Vec<javelin_classpath::TypeFacts>> {
    if !stores(db, home, version) {
        return None;
    }
    let facts = db
        .read_types()
        .inspect_err(|err| tracing::warn!(%err, "classpath: cached facts unreadable"))
        .ok()?;
    (!facts.is_empty()).then_some(facts)
}

/// What `javelin doctor` prints about the classpath.
///
/// Resolved through the same discovery and the same cache path the background
/// job uses, so the two cannot drift and disagree about why a name does not
/// resolve.
#[derive(Debug)]
pub struct Report {
    pub jdk: Option<Jdk>,
    pub cache: Option<PathBuf>,
    /// Facts cached *for the JDK above*. `None` when nothing is cached, when
    /// what is cached describes another JDK, or when the cache is unreadable —
    /// in every one of those cases the next session reads `src.zip` again.
    pub cached_facts: Option<u64>,
}

pub fn report(config: &Config) -> Report {
    let jdk = javelin_classpath::discover(config.jdk.home.as_deref(), &config.jdk.search_paths);
    let cache = cache_path(config).ok();
    let cached_facts = cache
        .as_ref()
        .zip(jdk.as_ref())
        .and_then(|(path, jdk)| cached_count(path, jdk));
    Report {
        jdk,
        cache,
        cached_facts,
    }
}

fn cached_count(path: &std::path::Path, jdk: &Jdk) -> Option<u64> {
    // An absent cache is reported as absent rather than created here: asking
    // what is cached should not leave a file behind. A cache written by an
    // incompatible Javelin is still discarded, because `CacheDb::open`
    // discards it — the same thing the next session would do to it anyway.
    if !path.exists() {
        return None;
    }
    let db = CacheDb::open(path).ok()?;
    let (home, version) = identity(jdk);
    if !stores(&db, &home, &version) {
        return None;
    }
    db.metadata("jdk.facts")
        .ok()
        .flatten()
        .and_then(|count| count.parse().ok())
}

/// Whether a static import among `imports` may bring in a type spelled
/// `name`, which the JLS 7.5 tiers do not know: any on-demand one, or a
/// single one of `name`'s first segment — it imports that name alone (JLS
/// 7.5.3).
fn static_import_may_name(imports: &[ImportDecl], name: &str) -> bool {
    let first = name.split('.').next().unwrap_or(name);
    imports.iter().any(|import| {
        import.is_static && (import.is_on_demand || import.name.rsplit('.').next() == Some(first))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn externals(fqns: &[&str]) -> ClasspathExternals {
        ClasspathExternals::new(
            Classpath::hand_built(fqns).world,
            Arc::new(javelin_index::WorkspaceIndex::default()),
        )
    }

    fn over(facts: Vec<javelin_classpath::TypeFacts>) -> ClasspathExternals {
        ClasspathExternals::new(
            Classpath::holding(facts).world,
            Arc::new(javelin_index::WorkspaceIndex::default()),
        )
    }

    /// A window onto a JDK holding `world` and a workspace whose own sources
    /// declare `sources`.
    fn with_workspace(world: &[&str], sources: &[&str]) -> ClasspathExternals {
        let types = sources.iter().copied().map(source_type).collect();
        ClasspathExternals::new(
            Classpath::hand_built(world).world,
            Arc::new(WorkspaceIndex::new(types)),
        )
    }

    /// One indexed workspace type. Every span is zero: what these tests ask
    /// about is which name resolves, and where it is written is the
    /// definition handler's question rather than this door's.
    fn source_type(fqn: &str) -> javelin_index::WorkspaceType {
        javelin_index::WorkspaceType {
            doc_span: None,
            fqn: fqn.to_owned(),
            simple: fqn.rsplit('.').next().unwrap_or(fqn).to_owned(),
            kind: DeclKind::Class,
            file: PathBuf::from(format!("/w/{}.java", fqn.replace('.', "/"))),
            name_start: 0,
            name_end: 0,
            decl_start: 0,
            decl_end: 0,
            name_range: javelin_document::Range::new(
                javelin_document::Position::default(),
                javelin_document::Position::default(),
            ),
            supertypes: Vec::new(),
            synthetic: false,
            lombok: Default::default(),
            type_parameters: Vec::new(),
        }
    }

    #[test]
    fn a_name_a_static_import_or_an_inherited_member_type_may_mean_is_shadowed() {
        let externals = externals(&[
            "java.lang.Object",
            "q.Base",
            "q.Base.Column",
            "q.Mapping.Column",
        ]);
        let syntax = |text: &str| {
            let document = javelin_document::TextDocument::new(
                javelin_document::DocumentId::new("p/A.java"),
                0,
                text.to_owned(),
            );
            let tree = javelin_parser::JavaParser::new()
                .expect("parser")
                .parse(&document)
                .expect("tree");
            javelin_parser::FileSyntax::build(&document, &tree)
        };
        assert!(!externals.may_shadow_type("Column", &syntax("package p;\nclass A {}\n")));
        for text in [
            "package p;\nimport static q.Mapping.Column;\nclass A {}\n",
            "package p;\nimport q.Base;\nclass A extends Base {}\n",
        ] {
            assert!(externals.may_shadow_type("Column", &syntax(text)), "{text}");
        }
    }

    fn on_demand(name: &str) -> ImportDecl {
        ImportDecl {
            is_on_demand: true,
            ..import(name)
        }
    }

    #[test]
    fn a_name_only_the_workspaces_own_sources_declare_resolves() {
        let externals = with_workspace(&[], &["com.example.Greeter"]);
        assert_eq!(
            externals.resolve("Greeter", Some("com.example"), &[]),
            Some("com.example.Greeter".to_owned())
        );
        // And the JDK half still answers on its own, which is the whole point
        // of one table rather than two lookups with a precedence between them.
        let both = with_workspace(&["java.lang.String"], &["com.example.Greeter"]);
        assert_eq!(
            both.resolve("String", Some("com.example"), &[]),
            Some("java.lang.String".to_owned())
        );
    }

    /// JLS 7.5's tiers, which is why the two halves are one table: the file's
    /// own package outranks `java.lang`, so a workspace that declares its own
    /// `String` shadows the JDK's without anything here saying so.
    #[test]
    fn a_sibling_in_the_same_package_outranks_java_lang() {
        let externals = with_workspace(&["java.lang.String"], &["com.example.String"]);
        assert_eq!(
            externals.resolve("String", Some("com.example"), &[]),
            Some("com.example.String".to_owned())
        );
        // From another package the same name is the JDK's again.
        assert_eq!(
            externals.resolve("String", Some("com.other"), &[]),
            Some("java.lang.String".to_owned())
        );
    }

    #[test]
    fn a_workspace_name_and_a_jdk_name_in_one_tier_answer_nothing() {
        let externals = with_workspace(&["java.util.Thing"], &["com.example.Thing"]);
        let imports = [on_demand("com.example"), on_demand("java.util")];
        // Two on-demand imports, one candidate from each half of the table.
        // Ambiguity answers nothing whichever side each name came from.
        assert_eq!(
            externals.resolve("Thing", Some("com.other"), &imports),
            None
        );
    }

    /// The soundness decision the struct doc argues: `apply.rs` excludes on a
    /// world merely having heard of a type, and a fact about a file the user
    /// can edit must not license that. Teaching `is_subtype` the workspace
    /// half is what would. Members are the half that *is* answered, since the
    /// eighth slice: they make candidates visible, and nothing in `apply.rs`
    /// excludes on them.
    #[test]
    fn the_workspace_half_answers_names_and_members_and_never_subtyping() {
        let externals = with_workspace(&[], &["com.example.Greeter"]);
        assert_eq!(
            externals.resolve("Greeter", Some("com.example"), &[]),
            Some("com.example.Greeter".to_owned()),
            "the premise: the index holds it"
        );
        assert_eq!(
            externals.is_subtype("com.example.Greeter", "com.example.Greeter"),
            None,
            "`known` must stay false for a workspace type"
        );
        assert!(externals.members("com.example.Greeter").is_some());
    }

    use crate::workspace::testing::{Sources, indexed};

    /// A window onto a JDK holding `facts` and a workspace indexed from real
    /// files by the real walk.
    fn over_sources(
        facts: Vec<javelin_classpath::TypeFacts>,
        sources: &Sources,
    ) -> ClasspathExternals {
        ClasspathExternals::new(Classpath::holding(facts).world, Arc::clone(&sources.index))
    }

    fn object() -> javelin_classpath::TypeFacts {
        declaring(
            "java.lang.Object",
            DeclKind::Class,
            &[],
            vec![method(
                "toString",
                MemberType::Class("java.lang.String".to_owned()),
            )],
        )
    }

    #[test]
    fn a_workspace_types_own_members_are_answered_with_where_they_are_written() {
        let sources = indexed(
            "own-members",
            &[(
                "com/example/Greeter.java",
                "package com.example;\npublic class Greeter {\n    int count;\n    String greet(int times) { return null; }\n}\n",
            )],
        );
        let externals = over_sources(vec![object()], &sources);
        let found = externals
            .members("com.example.Greeter")
            .expect("the index holds it");
        let greet = found
            .members
            .iter()
            .find(|member| member.name == "greet")
            .expect("its own method");
        assert_eq!(greet.owner, "com.example.Greeter");
        assert_eq!(greet.signature(), "String greet(int times)");
        let site = greet
            .site
            .as_ref()
            .expect("a workspace member says where it is");
        assert_eq!(
            PathBuf::from(&site.source),
            sources.path("com/example/Greeter.java")
        );
        assert_eq!(
            site.name_range,
            Some(javelin_document::Range::new(
                javelin_document::Position::new(3, 11),
                javelin_document::Position::new(3, 16),
            ))
        );
        assert!(found.members.iter().any(|member| member.name == "toString"));
        assert!(
            found.complete,
            "Greeter's closure is all resolved: {found:?}"
        );
    }

    /// A JAR compiled against the workspace's own module names its type as a
    /// supertype; the source is newer than any installed copy of it, so the
    /// closure follows the source and not the stale JAR fact.
    #[test]
    fn a_dependencys_supertype_the_workspace_declares_is_the_workspaces() {
        let sources = indexed(
            "jar-extends-workspace",
            &[(
                "com/example/Base.java",
                "package com.example;\npublic class Base { void fresh() {} }\n",
            )],
        );
        let stale = declaring(
            "com.example.Base",
            DeclKind::Class,
            &[],
            vec![method("stale", MemberType::Void)],
        );
        let jar = declaring(
            "lib.Impl",
            DeclKind::Class,
            &["com.example.Base"],
            Vec::new(),
        );
        let world = TypeWorld::new(vec![object()]).extended(vec![stale, jar]);
        let externals = ClasspathExternals::new(Arc::new(world), Arc::clone(&sources.index));
        let names: Vec<String> = externals
            .members("lib.Impl")
            .expect("held")
            .members
            .into_iter()
            .map(|member| member.name.clone())
            .collect();
        assert!(names.contains(&"fresh".to_owned()), "{names:?}");
        assert!(!names.contains(&"stale".to_owned()), "{names:?}");
    }

    #[test]
    fn a_workspace_types_members_include_a_workspace_supertypes_and_a_jdk_supertypes() {
        let sources = indexed(
            "inherited-members",
            &[
                (
                    "com/example/Greeter.java",
                    "package com.example;\npublic class Greeter { void greet() {} }\n",
                ),
                (
                    "com/example/Loud.java",
                    "package com.example;\npublic class Loud extends Greeter { void shout() {} }\n",
                ),
            ],
        );
        let externals = over_sources(vec![object()], &sources);
        let found = externals.members("com.example.Loud").expect("held");
        let owners: Vec<(&str, &str)> = found
            .members
            .iter()
            .map(|member| (member.name.as_str(), member.owner.as_str()))
            .collect();
        assert!(
            owners.contains(&("shout", "com.example.Loud")),
            "{owners:?}"
        );
        assert!(
            owners.contains(&("greet", "com.example.Greeter")),
            "{owners:?}"
        );
        assert!(
            owners.contains(&("toString", "java.lang.Object")),
            "{owners:?}"
        );
        assert_eq!(
            owners
                .iter()
                .filter(|(name, _)| *name == "toString")
                .count(),
            1,
            "Object is reached twice and visited once"
        );
    }

    #[test]
    fn a_supertype_that_resolves_to_nothing_lowers_complete_and_keeps_the_rest() {
        let sources = indexed(
            "gap",
            &[(
                "p/A.java",
                "package p;\nclass A extends Missing { void own() {} }\n",
            )],
        );
        let found = over_sources(vec![object()], &sources)
            .members("p.A")
            .expect("held");
        assert!(!found.complete);
        assert!(found.members.iter().any(|member| member.name == "own"));
    }

    /// Recovery can drop a member a broken file declares, and a subtype in a
    /// clean file inherits the gap.
    #[test]
    fn a_type_in_a_file_that_parses_with_errors_has_no_whole_member_list() {
        let sources = indexed(
            "broken-members",
            &[
                ("p/A.java", "package p;\npublic class A { void a( }\n"),
                ("p/B.java", "package p;\npublic class B extends A {}\n"),
                ("p/C.java", "package p;\npublic class C { void c() {} }\n"),
            ],
        );
        let externals = over_sources(vec![object()], &sources);
        assert!(!externals.members("p.A").expect("held").complete);
        assert!(!externals.members("p.B").expect("held").complete);
        assert!(externals.members("p.C").expect("held").complete);
    }

    #[test]
    fn a_workspace_interfaces_static_method_is_not_inherited_by_an_implementer() {
        let sources = indexed(
            "interface-static",
            &[(
                "p/I.java",
                "package p;\ninterface I { static void s() {} void d(); }\nclass C implements I {}\n",
            )],
        );
        let externals = over_sources(vec![object()], &sources);
        let implementer = names(&externals, "p.C");
        assert!(implementer.contains(&"d".to_owned()), "{implementer:?}");
        assert!(!implementer.contains(&"s".to_owned()), "{implementer:?}");
        assert!(names(&externals, "p.I").contains(&"s".to_owned()));
    }

    #[test]
    fn two_files_declaring_one_fqn_have_no_members_at_all() {
        let sources = indexed(
            "duplicate-members",
            &[
                ("a/p/A.java", "package p;\nclass A { void one() {} }\n"),
                ("b/p/A.java", "package p;\nclass A { void two() {} }\n"),
            ],
        );
        assert!(
            over_sources(vec![object()], &sources)
                .members("p.A")
                .is_none()
        );
    }

    #[test]
    fn an_enum_inherits_from_java_lang_enum_and_lists_what_java_declares() {
        let sources = indexed(
            "enum-members",
            &[
                ("p/Color.java", "package p;\nenum Color { RED, GREEN }\n"),
                ("p/Entry.java", "package p;\nrecord Entry(String date) {}\n"),
            ],
        );
        let java_enum = declaring(
            "java.lang.Enum",
            DeclKind::Class,
            &["java.lang.Object"],
            vec![method(
                "name",
                MemberType::Class("java.lang.String".to_owned()),
            )],
        );
        let java_record = declaring(
            "java.lang.Record",
            DeclKind::Class,
            &["java.lang.Object"],
            vec![],
        );
        let string = declaring(
            "java.lang.String",
            DeclKind::Class,
            &["java.lang.Object"],
            vec![],
        );
        let externals = over_sources(vec![object(), java_enum, java_record, string], &sources);
        let found = externals.members("p.Color").expect("held");
        let red = found
            .members
            .iter()
            .find(|member| member.name == "RED")
            .expect("a constant");
        assert_eq!(red.declared, ExternalType::Class("p.Color".to_owned()));
        assert!(red.is_static);
        assert!(found.members.iter().any(|member| member.name == "name"));
        let value_of = found
            .members
            .iter()
            .find(|member| member.name == "valueOf")
            .expect("valueOf");
        assert!(value_of.is_static);
        assert_eq!(value_of.declared, ExternalType::Class("p.Color".to_owned()));
        assert!(found.complete, "values() and valueOf are members");
        let found = externals.members("p.Entry").expect("held");
        let date = found
            .members
            .iter()
            .find(|member| member.name == "date" && member.kind == DeclKind::Method)
            .expect("an accessor");
        assert_eq!(
            date.declared,
            ExternalType::Class("java.lang.String".to_owned())
        );
        assert!(found.complete, "and so are a record's accessors");
    }

    fn declared_type(externals: &ClasspathExternals, owner: &str, member: &str) -> ExternalType {
        externals
            .members(owner)
            .expect("held")
            .members
            .into_iter()
            .find(|found| found.name == member)
            .unwrap_or_else(|| panic!("{owner} has no {member}"))
            .declared
            .clone()
    }

    #[test]
    fn a_type_parameter_is_never_resolved_as_a_type() {
        // `Error` here is the class's own type parameter, which shadows
        // `java.lang.Error`; resolving it through the tiers would type the
        // call as a JDK class and let `apply.rs` exclude on it.
        let sources = indexed(
            "type-parameter",
            &[
                (
                    "p/Result.java",
                    "package p;\npublic class Result<Value, Error> {\n    public Error error() { return null; }\n    \
                     public <Item> Item first(Item i) { return i; }\n}\n",
                ),
                ("p/Item.java", "package p;\npublic class Item {}\n"),
            ],
        );
        let error = declaring("java.lang.Error", DeclKind::Class, &[], Vec::new());
        let externals = over_sources(vec![object(), error], &sources);
        assert_eq!(
            declared_type(&externals, "p.Result", "error"),
            ExternalType::Variable("Error".into()),
            "the owner's own variable, for a receiver's arguments to bind"
        );
        assert!(matches!(
            declared_type(&externals, "p.Result", "first"),
            ExternalType::Unresolved(_)
        ));
    }

    /// Tier 2's slice 5: a workspace type's members are read in its own
    /// variables, and its supertype edges bind what they wrote.
    #[test]
    fn a_workspace_types_variables_are_bound_through_its_edges_and_its_arguments() {
        let sources = indexed(
            "workspace-generics",
            &[
                (
                    "p/Repo.java",
                    "package p;\nimport java.util.*;\npublic interface Repo<T> {\n    \
                     Optional<T> find();\n    T get();\n    List<Entity> all();\n    \
                     <T> T pick(T t);\n    <Item> Item first(Item i);\n}\n",
                ),
                ("p/Entity.java", "package p;\npublic class Entity {}\n"),
                (
                    "p/EntityRepo.java",
                    "package p;\npublic interface EntityRepo extends Repo<Entity> {}\n",
                ),
                (
                    "p/RawRepo.java",
                    "package p;\npublic interface RawRepo extends Repo {}\n",
                ),
                (
                    "p/Both.java",
                    "package p;\nimport java.util.ArrayList;\n\
                     public abstract class Both extends ArrayList<String> implements Repo<Entity> {}\n",
                ),
                (
                    "p/Names.java",
                    "package p;\nimport java.util.ArrayList;\n\
                     public class Names extends ArrayList<String> {}\n",
                ),
            ],
        );
        let generic =
            |fqn: &str, parameter: &str, members: Vec<MemberFacts>| javelin_classpath::TypeFacts {
                type_parameters: vec![parameter.to_owned()],
                ..declaring(fqn, DeclKind::Class, &["java.lang.Object"], members)
            };
        let externals = over_sources(
            vec![
                object(),
                declaring("java.lang.String", DeclKind::Class, &[], Vec::new()),
                generic("java.util.Optional", "T", Vec::new()),
                generic("java.util.List", "E", Vec::new()),
                generic(
                    "java.util.ArrayList",
                    "E",
                    vec![method("get", MemberType::Variable("E".to_owned()))],
                ),
            ],
            &sources,
        );
        let variable = |name: &str| ExternalType::Variable(name.to_owned());
        let class = |fqn: &str| ExternalType::Class(fqn.to_owned());
        let repo = externals.members("p.Repo").expect("the workspace type");
        assert_eq!(repo.type_parameters, vec!["T"]);
        assert_eq!(declared_type(&externals, "p.Repo", "get"), variable("T"));
        assert_eq!(
            declared_type(&externals, "p.Repo", "find"),
            ExternalType::Parameterized("java.util.Optional".to_owned(), vec![variable("T")])
        );
        assert_eq!(
            declared_type(&externals, "p.Repo", "all"),
            ExternalType::Parameterized("java.util.List".to_owned(), vec![class("p.Entity")]),
            "an argument is resolved in the declaring file"
        );
        assert_eq!(
            declared_type(&externals, "p.Repo", "pick"),
            ExternalType::Unresolved("T".to_owned()),
            "a method's own variable shadows the owner's"
        );
        let first = repo
            .members
            .iter()
            .find(|member| member.name == "first")
            .expect("the generic method");
        assert_eq!(first.type_parameters, vec!["Item"]);
        assert_eq!(first.declared, ExternalType::Unresolved("Item".to_owned()));

        assert_eq!(
            declared_type(&externals, "p.EntityRepo", "get"),
            class("p.Entity"),
            "a workspace edge binds what it wrote"
        );
        assert_eq!(
            declared_type(&externals, "p.EntityRepo", "find"),
            ExternalType::Parameterized("java.util.Optional".to_owned(), vec![class("p.Entity")])
        );
        assert_eq!(
            declared_type(&externals, "p.Both", "find"),
            ExternalType::Parameterized("java.util.Optional".to_owned(), vec![class("p.Entity")]),
            "each edge binds its own arguments"
        );
        assert_eq!(
            declared_type(&externals, "p.Names", "get"),
            class("java.lang.String"),
            "and so does one into the JDK"
        );
        assert_eq!(
            declared_type(&externals, "p.RawRepo", "get"),
            ExternalType::Unresolved("T".to_owned()),
            "a raw edge binds nothing"
        );
        let raw_first = externals
            .members("p.RawRepo")
            .expect("the raw subtype")
            .members
            .into_iter()
            .find(|member| member.name == "first")
            .expect("inherited");
        assert!(raw_first.type_parameters.is_empty(), "and erases (JLS 4.8)");
    }

    /// A name written inside a workspace type that an enclosing type's
    /// variable, or a member type of a frame of its closures, may mean is
    /// never read through the tiers — and inside a type argument nor is any
    /// name, where such a closure is not whole.
    /// A workspace member's type is certain where the declaring type's
    /// closures are whole, so every name inheritance may shadow was refused,
    /// and its file parsed.
    #[test]
    fn a_workspace_members_type_is_certain_where_nothing_could_shadow_it() {
        let sources = indexed(
            "workspace-chains-certain",
            &[
                ("p/Entity.java", "package p;\npublic class Entity {}\n"),
                (
                    "p/Repo.java",
                    "package p;\npublic class Repo {\n    \
                     public Entity find() { return null; }\n    \
                     public static class Nested { public Entity one() { return null; } }\n}\n",
                ),
                (
                    "p/Loose.java",
                    "package p;\npublic class Loose extends Missing {\n    \
                     public Entity find() { return null; }\n    \
                     public static class Inner { public Entity one() { return null; } }\n}\n",
                ),
                (
                    "p/Broken.java",
                    "package p;\npublic class Broken {\n    \
                     public Entity find() { return null; }\n    void x( {\n}\n",
                ),
            ],
        );
        let externals = over_sources(vec![object()], &sources);
        let certain = |owner: &str| {
            externals
                .members(owner)
                .expect("held")
                .members
                .into_iter()
                .find(|found| found.owner == owner)
                .unwrap_or_else(|| panic!("{owner} has a member"))
                .certain
        };
        assert!(certain("p.Repo"));
        assert!(certain("p.Repo.Nested"), "the type around it is whole too");
        assert!(!certain("p.Loose"), "a supertype the world does not hold");
        assert!(
            !certain("p.Loose.Inner"),
            "the type around it may inherit a member type of any name"
        );
        assert!(!certain("p.Broken"), "a file that does not parse");
        assert!(
            !externals
                .members("java.lang.Object")
                .expect("held")
                .members
                .iter()
                .any(|member| member.certain),
            "nothing asks a JDK member"
        );
    }

    #[test]
    fn a_workspace_name_an_outer_variable_or_an_inherited_member_type_may_mean_is_refused() {
        let sources = indexed(
            "workspace-generics-scope",
            &[
                (
                    "p/Result.java",
                    "package p;\nimport java.util.*;\npublic class Result<Value, Error> {\n    \
                     public Error error() { return null; }\n    \
                     public class Failure extends Result<Value, Error> {}\n    \
                     public class Inner { List<Error> all() { return null; } }\n}\n",
                ),
                (
                    "p/Base.java",
                    "package p;\npublic class Base { public static class Error {} }\n",
                ),
                (
                    "p/Outer.java",
                    "package p;\nimport java.util.*;\nimport java.util.function.Supplier;\n\
                     public class Outer extends Base {\n    \
                     public static abstract class Inner implements Supplier<Error> {}\n    \
                     public List<Error> errors() { return null; }\n    \
                     public Error error() { return null; }\n}\n",
                ),
                (
                    "p/Shadow.java",
                    "package p;\npublic class Shadow extends Base {\n    \
                     public Error made() { return null; }\n}\nclass Error {}\n",
                ),
                (
                    "p/Holder.java",
                    "package p;\npublic class Holder {\n    public static class Error {}\n    \
                     public static class Inner extends Base { public Error find() { return null; } }\n    \
                     public static class Mine extends Base {\n        \
                     public static class Error {}\n        \
                     public Error mine() { return null; }\n    }\n}\n",
                ),
                ("p/Entity.java", "package p;\npublic class Entity {}\n"),
                (
                    "p/Loose.java",
                    "package p;\nimport java.util.*;\npublic class Loose extends Missing {\n    \
                     List<Entity> all() { return null; }\n    Entity one() { return null; }\n}\n",
                ),
            ],
        );
        let generic =
            |fqn: &str, parameter: &str, members: Vec<MemberFacts>| javelin_classpath::TypeFacts {
                type_parameters: vec![parameter.to_owned()],
                ..declaring(fqn, DeclKind::Class, &["java.lang.Object"], members)
            };
        let externals = over_sources(
            vec![
                object(),
                declaring(
                    "java.lang.Error",
                    DeclKind::Class,
                    &["java.lang.Object"],
                    Vec::new(),
                ),
                generic("java.util.List", "E", Vec::new()),
                generic(
                    "java.util.function.Supplier",
                    "T",
                    vec![method("get", MemberType::Variable("T".to_owned()))],
                ),
            ],
            &sources,
        );
        let class = |fqn: &str| ExternalType::Class(fqn.to_owned());
        let unresolved = |name: &str| ExternalType::Unresolved(name.to_owned());
        let list = |argument: ExternalType| {
            ExternalType::Parameterized("java.util.List".to_owned(), vec![argument])
        };
        assert_eq!(
            declared_type(&externals, "p.Result.Failure", "error"),
            unresolved("Error"),
            "an inner class's edge names its outer class's variable"
        );
        assert_eq!(
            declared_type(&externals, "p.Result.Inner", "all"),
            list(unresolved("Error"))
        );
        assert_eq!(
            declared_type(&externals, "p.Outer.Inner", "get"),
            unresolved("Error"),
            "a member type the enclosing type inherits"
        );
        assert_eq!(
            declared_type(&externals, "p.Outer", "errors"),
            list(unresolved("Error"))
        );
        assert_eq!(
            declared_type(&externals, "p.Outer", "error"),
            unresolved("Error")
        );
        assert_eq!(
            declared_type(&externals, "p.Shadow", "made"),
            unresolved("Error"),
            "even over a top-level type the file declares"
        );
        assert_eq!(
            declared_type(&externals, "p.Holder.Inner", "find"),
            unresolved("Error"),
            "an inherited member type outranks one the type around it declares"
        );
        assert_eq!(
            declared_type(&externals, "p.Holder.Mine", "mine"),
            class("p.Holder.Mine.Error"),
            "the type's own member type outranks an inherited one"
        );
        assert_eq!(
            declared_type(&externals, "p.Loose", "all"),
            list(unresolved("Entity")),
            "an argument, below a closure that is not whole"
        );
        assert_eq!(
            declared_type(&externals, "p.Loose", "one"),
            class("p.Entity"),
            "a top-level name keeps the tiers' reading"
        );
    }

    #[test]
    fn a_nested_type_the_file_declares_in_scope_wins_over_a_top_level_one() {
        let sources = indexed(
            "nested-in-scope",
            &[
                (
                    "p/Outer.java",
                    "package p;\npublic class Outer {\n    static class Node { void inner() {} }\n    \
                     static class Base { void a() {} }\n    static class Impl extends Base {}\n    \
                     Node head() { return null; }\n}\n",
                ),
                (
                    "p/Node.java",
                    "package p;\npublic class Node { void top() {} }\n",
                ),
                (
                    "p/Base.java",
                    "package p;\npublic class Base { void b() {} }\n",
                ),
            ],
        );
        let externals = over_sources(vec![object()], &sources);
        assert_eq!(
            declared_type(&externals, "p.Outer", "head"),
            ExternalType::Class("p.Outer.Node".to_owned())
        );
        let impl_members = names(&externals, "p.Outer.Impl");
        assert!(impl_members.contains(&"a".to_owned()), "{impl_members:?}");
        assert!(!impl_members.contains(&"b".to_owned()), "{impl_members:?}");
    }

    #[test]
    fn a_type_the_file_declares_out_of_scope_is_refused_not_guessed() {
        // `Other.Node` is not in scope inside `Outer`; Java would take the
        // package's `Node`, and the file-local walk refuses rather than decide
        // — the same veto, so the two walks agree.
        let sources = indexed(
            "nested-out-of-scope",
            &[
                (
                    "p/Outer.java",
                    "package p;\npublic class Outer { Node head() { return null; } }\n\
                     class Other { static class Node {} }\n",
                ),
                ("p/Node.java", "package p;\npublic class Node {}\n"),
            ],
        );
        let externals = over_sources(vec![object()], &sources);
        assert!(matches!(
            declared_type(&externals, "p.Outer", "head"),
            ExternalType::Unresolved(_)
        ));
    }

    #[test]
    fn a_private_member_is_not_selectable_from_another_file() {
        let sources = indexed(
            "private-member",
            &[(
                "p/A.java",
                "package p;\npublic class A { private void hidden() {} void shown() {} }\n",
            )],
        );
        let members = names(&over_sources(vec![object()], &sources), "p.A");
        assert!(members.contains(&"shown".to_owned()), "{members:?}");
        assert!(!members.contains(&"hidden".to_owned()), "{members:?}");
    }

    /// A JAR the workspace's own build wrote — Gradle hands one module's
    /// `build/libs` JAR to the next — is compiled from the source the index
    /// holds, so its fact is no rival copy of the type.
    #[test]
    fn a_workspace_type_only_its_own_build_output_also_holds_is_whole() {
        let sources = indexed(
            "own-output",
            &[("p/A.java", "package p;\npublic class A { void a() {} }\n")],
        );
        let copy = || declaring("p.A", DeclKind::Class, &["java.lang.Object"], Vec::new());
        let world = |own: &[&str]| {
            TypeWorld::new(vec![object()])
                .extended(vec![copy()])
                .with_own_output(own.iter().map(|fqn| (*fqn).to_owned()).collect())
        };
        let whole = |world: TypeWorld| {
            ClasspathExternals::new(Arc::new(world), Arc::clone(&sources.index))
                .members("p.A")
                .expect("held")
                .complete
        };
        assert!(whole(world(&["p.A"])));
        assert!(!whole(world(&[])), "a copy from elsewhere is a rival");
        let enhanced = TypeWorld::new(vec![object()])
            .extended(vec![declaring(
                "p.A",
                DeclKind::Class,
                &["java.lang.Object"],
                vec![
                    method("a", MemberType::Void),
                    method("woven", MemberType::Void),
                ],
            )])
            .with_own_output(["p.A".to_owned()].into());
        assert!(
            !whole(enhanced),
            "a class with a member its source does not write is not that source"
        );
    }

    /// A type Lombok writes into beyond what the walk generates — here
    /// a `@SuperBuilder` with a builder method of another name — has a list
    /// short by it; one whose every Lombok annotation is modelled does not.
    #[test]
    fn a_workspace_type_lombok_writes_more_into_is_not_whole() {
        let sources = indexed(
            "lombok-unmodelled",
            &[
                (
                    "p/A.java",
                    "package p;\nimport lombok.experimental.SuperBuilder;\n\
                     @SuperBuilder(builderMethodName = \"make\") public class A { int a; }\n",
                ),
                (
                    "p/B.java",
                    "package p;\nimport lombok.Data;\n@Data public class B { int b; }\n",
                ),
            ],
        );
        let externals = over_sources(vec![object()], &sources);
        let whole = |fqn: &str| externals.members(fqn).expect("held").complete;
        assert!(!whole("p.A"));
        assert!(whole("p.B"));
    }

    /// What Lombok wrote into the class file is what the walk generates for
    /// the source, `canEqual` included, so `@Data` is no enhancer.
    #[test]
    fn a_lombok_class_in_its_own_build_output_is_whole() {
        let sources = indexed(
            "own-output-lombok",
            &[(
                "p/L.java",
                "package p;\nimport lombok.Data;\n@Data public class L { private int n; }\n",
            )],
        );
        let compiled = TypeWorld::new(vec![object()])
            .extended(vec![declaring(
                "p.L",
                DeclKind::Class,
                &["java.lang.Object"],
                ["getN", "setN", "canEqual", "toString"]
                    .into_iter()
                    .map(|name| method(name, MemberType::Void))
                    .collect(),
            )])
            .with_own_output(["p.L".to_owned()].into());
        assert!(
            ClasspathExternals::new(Arc::new(compiled), Arc::clone(&sources.index))
                .members("p.L")
                .expect("held")
                .complete
        );
    }

    /// `import static q.Outer.Error` brings in a member type the tiers know
    /// nothing of, so a member type spelled `Error` is not `java.lang.Error`.
    #[test]
    fn a_member_type_a_static_import_may_name_is_unresolved() {
        let parameter = |import: &str| {
            let sources = indexed(
                "static-import-member-type",
                &[(
                    "p/A.java",
                    &format!(
                        "package p;\n{import}\npublic class A {{ public void log(Error e) {{}} }}\n"
                    ),
                )],
            );
            let error = declaring("java.lang.Error", DeclKind::Class, &[], Vec::new());
            over_sources(vec![object(), error], &sources)
                .members("p.A")
                .expect("held")
                .members
                .into_iter()
                .find(|member| member.name == "log")
                .expect("log")
                .parameters[0]
                .clone()
        };
        assert_eq!(
            parameter(""),
            ExternalType::Class("java.lang.Error".to_owned())
        );
        assert_eq!(
            parameter("import static q.Util.helper;"),
            ExternalType::Class("java.lang.Error".to_owned())
        );
        for import in ["import static q.Outer.Error;", "import static q.Outer.*;"] {
            assert_eq!(
                parameter(import),
                ExternalType::Unresolved("Error".to_owned()),
                "{import}"
            );
        }
    }

    /// A single static import brings in the member types of its own name
    /// and no other, so only a supertype spelled with that name may mean one.
    #[test]
    fn only_a_static_import_of_the_supertypes_name_or_on_demand_doubts_it() {
        let whole = |import: &str| {
            let sources = indexed(
                "static-import-supertype",
                &[
                    (
                        "p/Base.java",
                        "package p;\npublic class Base { void b() {} }\n",
                    ),
                    (
                        "p/A.java",
                        &format!("package p;\n{import}\npublic class A extends Base {{}}\n"),
                    ),
                ],
            );
            over_sources(vec![object()], &sources)
                .members("p.A")
                .expect("held")
                .complete
        };
        assert!(whole(""));
        assert!(whole("import static q.Util.helper;"));
        assert!(!whole("import static q.Util.Base;"));
        assert!(!whole("import static q.Util.*;"));
    }

    #[test]
    fn a_jdk_types_supertypes_are_the_jdks_even_when_the_workspace_declares_the_name() {
        // A vendored or checked-out copy of a JDK type in the workspace must
        // not take over the closure of a JDK receiver.
        let sources = indexed(
            "vendored-jdk",
            &[(
                "java/lang/Object.java",
                "package java.lang;\npublic class Object { void vendored() {} }\n",
            )],
        );
        let string = declaring(
            "java.lang.String",
            DeclKind::Class,
            &["java.lang.Object"],
            Vec::new(),
        );
        let members = names(
            &over_sources(vec![object(), string], &sources),
            "java.lang.String",
        );
        assert!(members.contains(&"toString".to_owned()), "{members:?}");
        assert!(!members.contains(&"vendored".to_owned()), "{members:?}");
    }

    #[test]
    fn a_workspace_members_types_resolve_against_its_own_files_imports() {
        let sources = indexed(
            "member-types",
            &[(
                "p/A.java",
                "package p;\nimport java.util.List;\nclass Box<T> {}\n\
                 class A { List<String> names() { return null; } Box<String> box() { return null; } }\n",
            )],
        );
        let list = declaring("java.util.List", DeclKind::Interface, &[], Vec::new());
        let found = over_sources(vec![object(), list], &sources)
            .members("p.A")
            .expect("held");
        let names = found
            .members
            .iter()
            .find(|member| member.name == "names")
            .expect("the method");
        // Written with an argument, so not the raw type, whether the tiers
        // or the file itself settle the name.
        let string = || vec![ExternalType::Unresolved("String".to_owned())];
        assert_eq!(
            names.declared,
            ExternalType::Parameterized("java.util.List".to_owned(), string())
        );
        let boxed = found
            .members
            .iter()
            .find(|member| member.name == "box")
            .expect("the method");
        assert_eq!(
            boxed.declared,
            ExternalType::Parameterized("p.Box".to_owned(), string())
        );
    }

    /// An instance method. A test that wants it static sets `is_static` on the
    /// returned value, whose fields are public.
    fn method(name: &str, declared: MemberType) -> MemberFacts {
        MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: name.to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared,
            parameters: Vec::new(),
            variadic: false,
            // Hand-built, so there is no entry text to slice; zero is the
            // honest value rather than a made-up span.
            name_start: 0,
            name_end: 0,
        }
    }

    /// A JDK type's constructors are its whole list only where its entry
    /// parsed and it wrote one; no type's are answered from its members.
    #[test]
    fn a_jdk_types_constructors_are_answered_only_where_they_are_whole() {
        let constructor = MemberFacts {
            kind: DeclKind::Constructor,
            ..method("Box", MemberType::Unresolved(String::new()))
        };
        let mut boxed = fact("p.Box");
        boxed.constructors = vec![constructor];
        let count = |facts: javelin_classpath::TypeFacts| {
            over(vec![facts])
                .constructors("p.Box")
                .map(|found| found.members.len())
        };
        assert_eq!(count(boxed.clone()), Some(1));
        assert_eq!(count(fact("p.Box")), None, "none written");
        boxed.members_complete = false;
        assert_eq!(count(boxed), None, "an entry with errors");
    }

    /// slf4j's shape: `info(String, Object, Object)` beside
    /// `info(Marker, String, Object)`. A `String` is no `Marker` — the JDK's
    /// whole closure above it names no JAR type — so the call picks one.
    #[test]
    fn a_jdk_argument_excludes_a_parameter_of_a_jar_type() {
        let class = |fqn: &str| MemberType::Class(fqn.to_owned());
        let info = |parameters: Vec<MemberType>| MemberFacts {
            parameters,
            ..method("info", MemberType::Void)
        };
        let mut logger = declaring(
            "org.slf4j.Logger",
            DeclKind::Interface,
            &["java.lang.Object"],
            vec![
                info(vec![
                    class("java.lang.String"),
                    class("java.lang.Object"),
                    class("java.lang.Object"),
                ]),
                info(vec![
                    class("org.slf4j.Marker"),
                    class("java.lang.String"),
                    class("java.lang.Object"),
                ]),
            ],
        );
        logger.members[1].name_start = 1;
        let world = TypeWorld::new(vec![
            object(),
            declaring(
                "java.lang.String",
                DeclKind::Class,
                &["java.lang.Object"],
                Vec::new(),
            ),
        ])
        .extended(vec![
            declaring("org.slf4j.Marker", DeclKind::Interface, &[], Vec::new()),
            logger,
        ]);
        let externals = ClasspathExternals::new(Arc::new(world), Arc::default());
        let picked = |call: &str| {
            let text = format!(
                "import org.slf4j.Logger;
class A {{ void f(Logger log, Object a, \
                 org.slf4j.Marker m) {{ log.{call}; }} }}
"
            );
            let offset = text.find("info").expect("the call") as u32;
            let document = javelin_document::TextDocument::new(
                javelin_document::DocumentId::new("file:///A.java"),
                0,
                text,
            );
            let tree = javelin_parser::JavaParser::new()
                .expect("parser")
                .parse(&document)
                .expect("tree");
            let syntax = javelin_parser::FileSyntax::build(&document, &tree);
            let scopes = javelin_parser::ScopeTree::build(&document, &tree);
            let files = javelin_semantic::Files::single(document.id().clone(), &syntax, &scopes);
            javelin_semantic::external_member_at(&document, &tree, &files, &externals, offset)
                .map(|found| found.signature)
        };
        assert_eq!(
            picked("info(\"x {} {}\", a, a)").as_deref(),
            Some("void info(String, Object, Object)")
        );
        assert_eq!(
            picked("info(m, \"x {}\", a)").as_deref(),
            None,
            "a JAR type proves nothing about the other"
        );
    }

    /// The signature definition lands on at `needle`'s first byte in `text`:
    /// a local member's, or an external one's.
    fn lands_on(text: &str, needle: &str, externals: &ClasspathExternals) -> Option<String> {
        let offset = text.find(needle).expect("the needle") as u32;
        let document = javelin_document::TextDocument::new(
            javelin_document::DocumentId::new("file:///A.java"),
            0,
            text.to_owned(),
        );
        let tree = javelin_parser::JavaParser::new()
            .expect("parser")
            .parse(&document)
            .expect("tree");
        let syntax = javelin_parser::FileSyntax::build(&document, &tree);
        let scopes = javelin_parser::ScopeTree::build(&document, &tree);
        let files = javelin_semantic::Files::single(document.id().clone(), &syntax, &scopes);
        javelin_semantic::member_definition_at(&document, &tree, &files, externals, offset)
            .map(|found| found.signature)
            .or_else(|| {
                javelin_semantic::external_member_at(&document, &tree, &files, externals, offset)
                    .map(|found| found.signature)
            })
    }

    /// `r.register(this)` inside `Foo`, against `register(Object)` and
    /// `register(Foo)` a JAR declares — the build's own output holding `Foo`
    /// too. The file's `Foo` is that `com.example.Foo`, so `Object` reaching
    /// no JAR type rules nothing out, and the call answers nothing rather
    /// than `register(Object)`.
    #[test]
    fn a_declared_argument_is_not_excluded_from_its_own_fqn_in_a_jar() {
        let class = |fqn: &str| MemberType::Class(fqn.to_owned());
        let register = |parameter: &str| MemberFacts {
            parameters: vec![class(parameter)],
            ..method("register", MemberType::Void)
        };
        let mut registry = declaring(
            "com.example.Registry",
            DeclKind::Class,
            &["java.lang.Object"],
            vec![register("java.lang.Object"), register("com.example.Foo")],
        );
        registry.members[1].name_start = 1;
        let world = TypeWorld::new(vec![object()]).extended(vec![
            registry,
            declaring(
                "com.example.Foo",
                DeclKind::Class,
                &["java.lang.Object"],
                Vec::new(),
            ),
        ]);
        let externals = ClasspathExternals::new(Arc::new(world), Arc::default());
        let text = "package com.example;\n\
                    class Foo { void run(Registry r) { r.register(this); r.register(new Foo()); } }\n";
        assert_eq!(lands_on(text, "register(this)", &externals), None);
        assert_eq!(lands_on(text, "register(new", &externals), None);
    }

    /// No JDK type is a workspace type, so `resolveCustomerData(list, c)`
    /// rules out `(CustomerRef, …)` for a `List` argument. A name neither the
    /// world nor the workspace holds may be a newer JDK's, and says nothing.
    #[test]
    fn a_jdk_type_is_no_workspace_type_and_says_nothing_of_an_unheld_one() {
        let externals = with_workspace(&["java.util.List"], &["com.example.CustomerRef"]);
        assert_eq!(
            externals.is_subtype("java.util.List", "com.example.CustomerRef"),
            Some(false)
        );
        assert_eq!(
            externals.is_subtype("java.util.List", "java.util.SequencedCollection"),
            None
        );
    }

    /// `Entry` in a class implementing `Map` is `Map.Entry`, whatever an
    /// on-demand import's JAR holds, so the parameter the tiers read as
    /// `x.Entry` excludes no `Map.Entry` argument.
    #[test]
    fn a_parameter_an_inherited_member_type_may_mean_excludes_nothing() {
        let world = TypeWorld::new(vec![
            object(),
            declaring("java.util.Map", DeclKind::Interface, &[], Vec::new()),
            declaring("java.util.Map.Entry", DeclKind::Interface, &[], Vec::new()),
        ])
        .extended(vec![declaring(
            "x.Entry",
            DeclKind::Class,
            &["java.lang.Object"],
            Vec::new(),
        )]);
        let externals = ClasspathExternals::new(Arc::new(world), Arc::default());
        let text = "import x.*;\n\
                    abstract class A implements java.util.Map {\n\
                    void put2(Object o) {}\n\
                    void put2(Entry e) {}\n\
                    void run(java.util.Map.Entry e) { this.put2(e); }\n}\n";
        assert_eq!(lands_on(text, "put2(e)", &externals), None);
    }

    fn field(name: &str, declared: MemberType) -> MemberFacts {
        MemberFacts {
            kind: DeclKind::Field,
            ..method(name, declared)
        }
    }

    /// The names one receiver can select, in the order the walk found them.
    fn names(externals: &ClasspathExternals, fqn: &str) -> Vec<String> {
        externals
            .members(fqn)
            .expect("a name the world holds")
            .members
            .into_iter()
            .map(|member| member.name.clone())
            .collect()
    }

    fn import(name: &str) -> ImportDecl {
        let span = javelin_parser::Span {
            start_byte: 0,
            end_byte: 0,
            range: javelin_document::Range::new(
                javelin_document::Position::default(),
                javelin_document::Position::default(),
            ),
        };
        ImportDecl {
            name: name.to_owned(),
            span,
            name_span: span,
            is_static: false,
            is_on_demand: false,
        }
    }

    #[test]
    fn a_name_the_world_holds_is_its_own_subtype() {
        let externals = externals(&["java.lang.String"]);
        assert_eq!(
            externals.is_subtype("java.lang.String", "java.lang.String"),
            Some(true)
        );
        assert_eq!(
            externals.is_subtype("java.util.List", "java.util.List"),
            None
        );
    }

    /// The obligation `apply.rs` excludes on the *absence* of: a box class the
    /// world holds that unboxes to nothing would reach no primitive at all.
    #[test]
    fn every_box_class_the_world_holds_unboxes_to_its_primitive() {
        let classes: Vec<&str> = BOXES.iter().map(|(_, class)| *class).collect();
        let externals = externals(&classes);
        for (primitive, class) in BOXES {
            assert_eq!(externals.unboxed(class), Some(primitive), "{class}");
            assert_eq!(
                externals.boxed(primitive),
                Some(class.to_owned()),
                "{class}"
            );
        }
    }

    #[test]
    fn a_world_without_the_box_classes_boxes_and_unboxes_to_nothing() {
        let externals = externals(&["java.lang.String"]);
        assert_eq!(externals.boxed(Primitive::Int), None);
        assert_eq!(externals.unboxed("java.lang.Integer"), None);
        assert_eq!(
            externals.unboxed("java.lang.String"),
            None,
            "not a box class"
        );
    }

    #[test]
    fn an_ambiguous_name_answers_nothing_exactly_like_an_unknown_one() {
        let externals = externals(&["a.Thing", "b.Thing"]);
        let imports = [import("a.Thing"), import("b.Thing")];
        assert_eq!(externals.resolve("Thing", None, &imports), None);
        assert_eq!(externals.resolve("Thing", None, &[]), None);
        assert_eq!(
            externals.resolve("Thing", None, &imports[..1]),
            Some("a.Thing".to_owned())
        );
    }

    /// The machine with no Java on it, and the machine where discovery fails:
    /// the server keeps answering exactly what it answered before a classpath
    /// existed, and says so — nothing is complete, because a JDK could still
    /// turn up in a later session.
    #[test]
    fn a_classpath_job_that_finds_no_jdk_leaves_the_server_answering_phase_two() {
        let mut state = crate::ServerState::new(Config::default()).expect("grammar loads");
        let id = javelin_document::DocumentId::new("file:///A.java");
        state.did_open(
            id,
            1,
            "class A {\n  int go(int n) { return n; }\n}\n".to_owned(),
        );

        let (sender, receiver) = crossbeam_channel::unbounded();
        run(
            &Config::default(),
            None,
            &CancellationToken::never(),
            &sender,
            &Default::default(),
        );
        assert!(
            receiver.try_recv().is_err(),
            "nothing was read, so nothing arrives"
        );

        assert_eq!(state.engine.readiness, javelin_protocol::Readiness::File);
        assert!(!state.is_complete(), "a JDK may still turn up");
        assert!(
            state.externals().world.is_empty(),
            "the world stays empty rather than half-built"
        );
        let params = serde_json::from_value(serde_json::json!({
            "textDocument": { "uri": "file:///A.java" },
            "position": { "line": 1, "character": 25 },
        }))
        .expect("well-formed params");
        let found =
            crate::handlers::definition::handle(&mut state, &params, &CancellationToken::never())
                .expect("not cancelled");
        assert!(found.is_some(), "the local still resolves from the buffer");
    }

    #[test]
    fn background_indexing_off_starts_no_reader_at_all() {
        let mut config = Config::default();
        config.index.background_indexing = false;
        let (sender, _receiver) = crossbeam_channel::unbounded();
        // Not "no world arrived within n milliseconds": a cold read takes
        // eleven seconds, so that would pass with the check deleted.
        assert!(!spawn(
            &config,
            Vec::new(),
            CancellationToken::never(),
            Default::default(),
            sender,
            Default::default()
        ));
    }

    /// `discover`'s fallback order is a feature everywhere except here: a
    /// configured home that does not resolve must not silently become
    /// whichever JDK `$JAVA_HOME` happens to point at.
    #[test]
    fn a_configured_jdk_home_that_is_not_a_jdk_reads_nothing_at_all() {
        let mut config = Config::default();
        config.jdk.home = Some(PathBuf::from("/nope"));
        let found = Jdk {
            home: PathBuf::from("/somewhere/else"),
            version: Some(25),
            sources: Some(PathBuf::from("/somewhere/else/lib/src.zip")),
        };
        let (sender, receiver) = crossbeam_channel::unbounded();
        run(
            &config,
            Some(found),
            &CancellationToken::never(),
            &sender,
            &crate::progress::Reporter::default(),
        );
        assert!(receiver.try_recv().is_err(), "another JDK was read instead");
    }

    #[test]
    fn a_types_own_members_are_returned_for_a_name_the_world_holds() {
        let externals = over(vec![declaring(
            "java.lang.String",
            DeclKind::Class,
            &[],
            vec![
                method("length", MemberType::Primitive("int".to_owned())),
                field(
                    "CASE_INSENSITIVE_ORDER",
                    MemberType::Class("java.util.Comparator".to_owned()),
                ),
            ],
        )]);

        let found = externals.members("java.lang.String").expect("held");
        assert!(found.complete, "nothing above it was unreachable");
        let rendered: Vec<String> = found
            .members
            .iter()
            .map(|member| member.signature())
            .collect();
        assert_eq!(
            rendered,
            ["int length()", "Comparator CASE_INSENSITIVE_ORDER"]
        );
        assert!(
            found
                .members
                .iter()
                .all(|member| member.owner == "java.lang.String")
        );
    }

    #[test]
    fn an_inherited_members_site_names_the_supertypes_source_not_the_receivers() {
        // `site` must match `owner`, never the type the question was asked
        // about — an inherited member is written in the supertype's entry,
        // and a site naming the receiver's would send a caller that opens it
        // to the wrong file.
        let mut base = declaring(
            "a.Base",
            DeclKind::Class,
            &[],
            vec![MemberFacts {
                name_start: 10,
                name_end: 16,
                ..method("shared", MemberType::Void)
            }],
        );
        base.source = "a/Base.java".to_owned();
        let mut sub = declaring("a.Sub", DeclKind::Class, &["a.Base"], Vec::new());
        sub.source = "a/Sub.java".to_owned();

        let externals = over(vec![base, sub]);
        let found = externals.members("a.Sub").expect("held");
        let shared = found
            .members
            .iter()
            .find(|member| member.name == "shared")
            .expect("the inherited member");
        let site = shared.site.as_ref().expect("a real frame always has one");
        assert_eq!(site.source, "a/Base.java");
        assert_eq!(site.name_start, 10);
        assert_eq!(site.name_end, 16);
    }

    #[test]
    fn a_name_the_world_does_not_hold_answers_cannot_say() {
        let externals = externals(&["java.lang.String"]);

        // Held and declaring nothing is a different answer from not held, and
        // the difference is what stops a `.` on an unknown type looking like a
        // type with no members.
        let held = externals.members("java.lang.String").expect("held");
        assert!(held.members.is_empty());
        assert!(externals.members("java.util.List").is_none());
    }

    #[test]
    fn an_inherited_member_is_returned_with_the_supertype_as_its_owner() {
        let externals = over(vec![
            declaring(
                "a.Base",
                DeclKind::Class,
                &[],
                vec![method("shared", MemberType::Void)],
            ),
            declaring(
                "a.Sub",
                DeclKind::Class,
                &["a.Base"],
                vec![method("own", MemberType::Void)],
            ),
        ]);

        let found = externals.members("a.Sub").expect("held");
        assert!(found.complete);
        let owned: Vec<(&str, &str)> = found
            .members
            .iter()
            .map(|member| (member.name.as_str(), member.owner.as_str()))
            .collect();
        // `a.Sub` on the inherited one would make it look like the receiver's
        // own, which is what own-shadows-inherited compares.
        assert_eq!(owned, [("own", "a.Sub"), ("shared", "a.Base")]);
    }

    fn generic(
        fqn: &str,
        parameters: &[&str],
        supertypes: &[(&str, Vec<MemberType>)],
        members: Vec<MemberFacts>,
    ) -> javelin_classpath::TypeFacts {
        javelin_classpath::TypeFacts {
            name_start: 0,
            name_end: 0,
            type_parameters: parameters.iter().map(|name| (*name).to_owned()).collect(),
            supertype_arguments: supertypes
                .iter()
                .filter(|(_, arguments)| !arguments.is_empty())
                .map(|(fqn, arguments)| ((*fqn).to_owned(), arguments.clone()))
                .collect(),
            ..declaring(
                fqn,
                DeclKind::Interface,
                &supertypes.iter().map(|(fqn, _)| *fqn).collect::<Vec<_>>(),
                members,
            )
        }
    }

    fn variable(name: &str) -> MemberType {
        MemberType::Variable(name.to_owned())
    }

    fn declared(found: &ExternalMembers, name: &str) -> ExternalType {
        found
            .members
            .iter()
            .find(|member| member.name == name)
            .unwrap_or_else(|| panic!("no member called {name}"))
            .declared
            .clone()
    }

    /// `Arr<T> extends Lst<T>`, `Lst<E> extends Coll<E>`: a member two edges
    /// up is expressed in `Arr`'s own `T`, which is what a receiver's
    /// argument binds — whatever each supertype called its parameter.
    #[test]
    fn an_inherited_member_is_expressed_in_the_asked_types_own_variables() {
        let externals = over(vec![
            generic(
                "a.Coll",
                &["C"],
                &[],
                vec![
                    method("first", variable("C")),
                    method(
                        "all",
                        MemberType::Parameterized("a.Coll".into(), vec![variable("C")]),
                    ),
                ],
            ),
            generic("a.Lst", &["E"], &[("a.Coll", vec![variable("E")])], vec![]),
            generic(
                "a.Arr",
                &["T"],
                &[("a.Lst", vec![variable("T")])],
                vec![method("own", variable("T"))],
            ),
            generic(
                "a.Names",
                &[],
                &[("a.Lst", vec![MemberType::Class("java.lang.String".into())])],
                vec![],
            ),
        ]);
        let arr = externals.members("a.Arr").expect("held");
        assert_eq!(arr.type_parameters, vec!["T"]);
        assert_eq!(declared(&arr, "own"), ExternalType::Variable("T".into()));
        assert_eq!(declared(&arr, "first"), ExternalType::Variable("T".into()));
        assert_eq!(
            declared(&arr, "all"),
            ExternalType::Parameterized("a.Coll".into(), vec![ExternalType::Variable("T".into())])
        );
        let names = externals.members("a.Names").expect("held");
        assert!(names.type_parameters.is_empty());
        assert_eq!(
            declared(&names, "first"),
            ExternalType::Class("java.lang.String".into())
        );
    }

    /// A raw receiver's closure binds no edge: `Wrap<T> extends Lst<String>`
    /// used raw has an erased `first`, which javac types `Object` — unknown
    /// here, and never the `String` the edge wrote.
    #[test]
    fn a_raw_closure_binds_no_edge() {
        let externals = over(vec![
            generic("a.Coll", &["C"], &[], vec![method("first", variable("C"))]),
            generic(
                "a.Wrap",
                &["T"],
                &[("a.Coll", vec![MemberType::Class("java.lang.String".into())])],
                vec![method("own", variable("T"))],
            ),
        ]);
        assert_eq!(
            declared(&externals.members("a.Wrap").expect("held"), "first"),
            ExternalType::Class("java.lang.String".into())
        );
        let raw = externals.raw_members("a.Wrap").expect("held");
        assert_eq!(
            declared(&raw, "first"),
            ExternalType::Unresolved("C".into())
        );
        assert_eq!(declared(&raw, "own"), ExternalType::Variable("T".into()));
    }

    /// A raw edge, or one with the wrong number of arguments, binds the
    /// supertype's parameters to nothing — never to a variable of the asked
    /// type that happens to share the name.
    /// A raw edge erases what is inherited through it (JLS 4.8), so an
    /// instance method's own variables are not a call's to bind; a static's
    /// are, and one reached through a bound edge keeps them.
    #[test]
    fn a_raw_edge_erases_an_inherited_methods_own_variables() {
        let own = |mut member: MemberFacts, is_static| {
            member.type_parameters = vec!["V".into()];
            member.is_static = is_static;
            member
        };
        let externals = over(vec![
            generic(
                "a.Fn",
                &["T"],
                &[],
                vec![
                    own(method("then", MemberType::Unresolved("V".into())), false),
                    own(method("make", MemberType::Unresolved("V".into())), true),
                ],
            ),
            declaring("a.Legacy", DeclKind::Class, &["a.Fn"], vec![]),
            generic(
                "a.Typed",
                &[],
                &[("a.Fn", vec![MemberType::Class("java.lang.String".into())])],
                vec![],
            ),
        ]);
        let params = |fqn: &str, name: &str| {
            externals
                .members(fqn)
                .expect("held")
                .members
                .into_iter()
                .find(|member| member.name == name)
                .map(|member| member.type_parameters.clone())
        };
        assert_eq!(params("a.Legacy", "then"), Some(Vec::new()));
        assert_eq!(params("a.Typed", "then"), Some(vec!["V".to_owned()]));
        assert_eq!(params("a.Fn", "then"), Some(vec!["V".to_owned()]));
    }

    #[test]
    fn a_raw_or_misshapen_edge_binds_nothing() {
        let externals = over(vec![
            generic("a.Coll", &["E"], &[], vec![method("first", variable("E"))]),
            generic("a.Raw", &["E"], &[("a.Coll", vec![])], vec![]),
            generic(
                "a.Pair",
                &["K", "V"],
                &[],
                vec![method("key", variable("K"))],
            ),
            generic(
                "a.Short",
                &["E"],
                &[("a.Pair", vec![variable("E")])],
                vec![],
            ),
        ]);
        assert_eq!(
            declared(&externals.members("a.Raw").expect("held"), "first"),
            ExternalType::Unresolved("E".into())
        );
        assert_eq!(
            declared(&externals.members("a.Short").expect("held"), "key"),
            ExternalType::Unresolved("K".into())
        );
    }

    #[test]
    fn a_diamond_contributes_a_shared_supertype_once_and_an_override_twice() {
        let externals = over(vec![
            declaring(
                "a.Top",
                DeclKind::Class,
                &[],
                vec![method("go", MemberType::Void)],
            ),
            declaring("a.Left", DeclKind::Class, &["a.Top"], Vec::new()),
            declaring("a.Right", DeclKind::Class, &["a.Top"], Vec::new()),
            declaring(
                "a.Bottom",
                DeclKind::Class,
                &["a.Left", "a.Right"],
                vec![method("go", MemberType::Void)],
            ),
        ]);

        let found = externals.members("a.Bottom").expect("held");
        assert!(found.complete);
        let owners: Vec<&str> = found
            .members
            .iter()
            .map(|member| member.owner.as_str())
            .collect();
        assert_eq!(
            owners.len(),
            2,
            "the override and the overridden are two members, as they are in a file"
        );
        assert_eq!(
            owners.iter().filter(|owner| **owner == "a.Top").count(),
            1,
            "the same type reached twice contributes once"
        );
    }

    #[test]
    fn a_supertype_the_world_lacks_a_fact_for_makes_the_closure_incomplete() {
        // `supertypes_complete` is true — the name resolved — but no fact was
        // read for it, so whatever it declares is unreachable.
        let externals = over(vec![declaring(
            "a.Sub",
            DeclKind::Class,
            &["a.Unread"],
            vec![method("own", MemberType::Void)],
        )]);

        let found = externals.members("a.Sub").expect("held");
        assert!(!found.complete);
        assert_eq!(names(&externals, "a.Sub"), ["own"], "and still returned");
    }

    #[test]
    fn an_incomplete_supertype_list_makes_the_closure_incomplete_and_still_returns_what_it_has() {
        let mut sub = declaring(
            "a.Sub",
            DeclKind::Class,
            &["a.Base"],
            vec![method("own", MemberType::Void)],
        );
        // A second supertype resolved to nothing and was dropped at read time.
        sub.supertypes_complete = false;
        let externals = over(vec![
            sub,
            declaring(
                "a.Base",
                DeclKind::Class,
                &[],
                vec![method("shared", MemberType::Void)],
            ),
        ]);

        assert!(!externals.members("a.Sub").expect("held").complete);
        assert_eq!(
            names(&externals, "a.Sub"),
            ["own", "shared"],
            "the supertype that did resolve is still walked"
        );
    }

    /// A supertype's member list that is short — its source parsed with
    /// errors, or a newer overlay was not read — leaves the closure short.
    #[test]
    fn a_short_member_list_anywhere_in_the_closure_makes_it_incomplete() {
        let mut base = declaring(
            "a.Base",
            DeclKind::Class,
            &[],
            vec![method("shared", MemberType::Void)],
        );
        base.members_complete = false;
        let externals = over(vec![
            declaring("a.Sub", DeclKind::Class, &["a.Base"], Vec::new()),
            base,
        ]);
        assert!(!externals.members("a.Sub").expect("held").complete);
    }

    #[test]
    fn a_cycle_in_the_supertype_graph_terminates_rather_than_looping() {
        let externals = over(vec![
            declaring(
                "a.A",
                DeclKind::Class,
                &["a.B"],
                vec![method("a", MemberType::Void)],
            ),
            declaring(
                "a.B",
                DeclKind::Class,
                &["a.A"],
                vec![method("b", MemberType::Void)],
            ),
        ]);

        let found = externals.members("a.A").expect("held");
        assert!(found.complete, "every node was held and whole");
        assert_eq!(found.members.len(), 2, "each contributing once");
    }

    #[test]
    fn an_interfaces_static_method_is_not_inherited_by_an_implementer() {
        let mut of = method("of", MemberType::Class("a.Iface".to_owned()));
        of.is_static = true;
        let externals = over(vec![
            declaring(
                "a.Iface",
                DeclKind::Interface,
                &[],
                vec![of, method("run", MemberType::Void)],
            ),
            declaring("a.Impl", DeclKind::Class, &["a.Iface"], Vec::new()),
            declaring(TypeWorld::OBJECT, DeclKind::Class, &[], Vec::new()),
        ]);

        // The interface is not a supertype of itself: `Iface.of()` compiles,
        // so a receiver naming it keeps its own statics.
        assert_eq!(names(&externals, "a.Iface"), ["of", "run"]);
        // JLS 8.4.8: `Impl.of()` does not.
        assert_eq!(names(&externals, "a.Impl"), ["run"]);
    }

    #[test]
    fn an_interfaces_constant_is_inherited() {
        // A field of an interface is implicitly static, and inherited all the
        // same — which is why only methods are skipped above.
        let mut max = field("MAX", MemberType::Primitive("int".to_owned()));
        max.is_static = true;
        let externals = over(vec![
            declaring("a.Limits", DeclKind::Interface, &[], vec![max]),
            declaring("a.Impl", DeclKind::Class, &["a.Limits"], Vec::new()),
            declaring(TypeWorld::OBJECT, DeclKind::Class, &[], Vec::new()),
        ]);

        let found = externals.members("a.Impl").expect("held");
        assert_eq!(names(&externals, "a.Impl"), ["MAX"]);
        assert!(found.members[0].is_static);
        assert_eq!(found.members[0].owner, "a.Limits");
    }

    /// No supertype list names `java.lang.Object` for an interface — only a
    /// class gets the `extends` synthesised — so reaching it is this walk's
    /// job, and a world with no fact for it says so rather than claiming a
    /// list that is missing every one of its members.
    #[test]
    fn an_interface_reaches_objects_members_that_no_supertype_list_names() {
        let object = declaring(
            TypeWorld::OBJECT,
            DeclKind::Class,
            &[],
            vec![method(
                "toString",
                MemberType::Class("java.lang.String".to_owned()),
            )],
        );
        let iface = declaring("a.Iface", DeclKind::Interface, &[], Vec::new());

        let externals = over(vec![iface.clone(), object]);
        let found = externals.members("a.Iface").expect("held");
        assert!(found.complete);
        assert_eq!(names(&externals, "a.Iface"), ["toString"]);

        let alone = over(vec![iface]);
        assert!(!alone.members("a.Iface").expect("held").complete);
    }

    #[test]
    fn a_closure_wider_than_the_visit_budget_answers_incomplete() {
        let depth = TypeWorld::MAX_VISITS + 10;
        let facts: Vec<javelin_classpath::TypeFacts> = (0..depth)
            .map(|link| {
                let supertypes = if link + 1 < depth {
                    vec![format!("a.Link{}", link + 1)]
                } else {
                    Vec::new()
                };
                javelin_classpath::TypeFacts {
                    name_start: 0,
                    name_end: 0,
                    supertypes,
                    members: vec![method(&format!("m{link}"), MemberType::Void)],
                    ..fact(&format!("a.Link{link}"))
                }
            })
            .collect();

        let found = over(facts).members("a.Link0").expect("held");
        assert!(!found.complete);
        assert_eq!(
            found.members.len(),
            TypeWorld::MAX_VISITS,
            "one member per frame, and no more frames than the budget"
        );
    }

    #[test]
    fn an_empty_world_answers_cannot_say_to_every_name() {
        let externals = externals(&[]);

        assert!(externals.members("java.lang.String").is_none());
        assert!(externals.members(TypeWorld::OBJECT).is_none());
        assert!(externals.members("").is_none());
    }

    #[test]
    fn an_unknown_primitive_keyword_is_unresolved_rather_than_a_guess() {
        for (primitive, _) in BOXES {
            assert_eq!(
                member_type(&MemberType::Primitive(primitive.keyword().to_owned())),
                ExternalType::Primitive(primitive),
                "{}",
                primitive.keyword()
            );
        }
        assert_eq!(
            member_type(&MemberType::Primitive("int128".to_owned())),
            ExternalType::Unresolved("int128".to_owned())
        );
        assert_eq!(member_type(&MemberType::Void), ExternalType::Void);
        // An empty payload is a member that writes no type at all, which is a
        // value and not a missing one.
        assert_eq!(
            member_type(&MemberType::Unresolved(String::new())),
            ExternalType::Unresolved(String::new())
        );
    }
}
