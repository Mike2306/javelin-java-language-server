//! The one window this crate has onto what no source file it can read
//! declares.
//!
//! A trait rather than a dependency on `javelin-classpath`: `javelin-server`
//! owns the classpath and implements this over it, so the engine below it stays
//! testable against a hand-written answer and the crate graph stays acyclic.
//! [`NoExternals`] answers "cannot say" to everything, which is exactly what
//! this crate answered before a classpath existed.

use std::sync::Arc;

use javelin_parser::{DeclKind, ImportDecl};

use crate::types::Primitive;

/// What no readable source file declares. The engine's one window onto the
/// classpath.
pub trait Externals {
    /// The fully-qualified name `name` can only mean, resolved the way a
    /// compilation unit in `package` with these `imports` resolves it.
    ///
    /// `name` is as the author wrote it, dots included: `String`, but also
    /// `Map.Entry` and an already-qualified `java.util.List`. Resolving those
    /// is the implementation's job, not the caller's — the caller hands over
    /// what it read out of the buffer.
    ///
    /// `Some` only when the name can mean exactly one thing. Ambiguous and
    /// unknown both answer `None`, and deliberately cannot be told apart:
    /// both mean *answer nothing*, and a caller able to distinguish them
    /// would eventually branch on the difference and guess.
    fn resolve(&self, name: &str, package: Option<&str>, imports: &[ImportDecl]) -> Option<String>;

    /// Whether `from` is a subtype of `to`, both fully qualified.
    ///
    /// `None` is *cannot say* rather than "no": a name this world does not
    /// hold, or a supertype chain it could not follow to the end. A `to` the
    /// world holds only as a dependency's may still answer `Some(false)`,
    /// since no platform type's closure reaches one — which is why `apply.rs`
    /// never reads `is_subtype(Object, to)` as a declaration's answer for
    /// such a `to`.
    ///
    /// Reflexive on exactly the names it holds: `is_subtype(x, x)` is
    /// `Some(true)` for a known `x` whatever the state of the closure above it,
    /// and `None` for one it has never heard of. `apply.rs` reads that as the
    /// membership test this trait deliberately does not expose, so an
    /// implementation owes it.
    ///
    /// It owes `java.lang.Object` too, and for a soundness reason rather than
    /// a convenience one: `is_subtype(x, "java.lang.Object")` must never be
    /// `Some(false)` for an `x` this world holds. JLS 4.10.2 puts every
    /// reference type under `Object` and no source writes the `extends` that
    /// says so, so an implementation that answers from a closure alone will
    /// report an interface, an enum, a record or an annotation type as
    /// demonstrably not an `Object` — and `apply.rs` will exclude `f(Object)`
    /// and hand the call to a sibling overload.
    ///
    /// The same holds of the box mapping, and for a sharper reason:
    /// [`Externals::unboxed`] must be *total* over the box classes this world
    /// holds — if `is_subtype(fqn, fqn)` is `Some(true)` and `fqn` is one of
    /// the eight box classes, `unboxed(fqn)` must answer `Some`. It is the one
    /// fact `apply.rs` excludes on the *absence* of: a reference type the world
    /// holds that unboxes to nothing reaches no primitive at all, so a world
    /// whose fact set and whose box mapping disagree picks a wrong overload
    /// rather than refusing. Deriving that negative instead would mean calling
    /// [`Externals::boxed`] over all eight primitives per candidate, which is
    /// eight allocations on the request path.
    fn is_subtype(&self, from: &str, to: &str) -> Option<bool>;

    /// The fully-qualified name of the class `primitive` boxes to.
    ///
    /// On the trait rather than a free function because the mapping is only
    /// usable when the world actually holds the type: one built from a JDK
    /// with no `src.zip` knows no `java.lang.Integer`, and must not be used to
    /// exclude anything on the strength of a name it cannot see.
    fn boxed(&self, primitive: Primitive) -> Option<String>;

    /// The primitive `fqn` unboxes to, the inverse of [`Externals::boxed`] and
    /// total over the box classes this world holds — see
    /// [`Externals::is_subtype`] for why the totality is an obligation rather
    /// than a nicety.
    fn unboxed(&self, fqn: &str) -> Option<Primitive>;

    /// The members a receiver of type `fqn` can select: the ones `fqn`
    /// declares and every one it inherits, already closed over the supertype
    /// chain.
    ///
    /// `None` is *cannot say* — this world holds no fact for `fqn` — and is
    /// what an empty world answers to every name, which is what keeps
    /// [`crate::members_of`] giving Phase 2's empty list with no classpath
    /// behind it. `Some` with an empty list is a different answer: the type is
    /// held, and it declares and inherits nothing a `.` can reach.
    ///
    /// The closure is walked on this side of the door because everything it
    /// needs is on this side: the kinds, the supertype edges and the
    /// completeness flags are the world's, and [`crate::members_of`] walks
    /// only the files a query can reach. Two obligations come with that, and
    /// both are things this crate cannot check for itself.
    ///
    /// Java does not inherit an interface's static method, so a frame that is
    /// a supertype rather than `fqn` itself and is a [`DeclKind::Interface`]
    /// must contribute no static *method* — its constants are inherited, so
    /// only methods are skipped. It is the same rule `members_of` applies to
    /// the file-local walk, stated in the two places that have what it needs
    /// to apply it: there a `Decl` and its modifiers, here a fact and its
    /// kind.
    ///
    /// [`ExternalMember::owner`] must name the type that *declares* each
    /// member rather than `fqn` throughout. It is what own-shadows-inherited
    /// compares, so an owner filled in with the type asked about would make
    /// every inherited member look like the receiver's own — which turns
    /// `sole_candidate`'s honest refusal into a pick, the one direction this
    /// crate never trades in.
    ///
    /// And a member's [`ExternalMember::kind`] must be one a `.` can select: a
    /// field, a method, an enum constant, a record component or an annotation
    /// element. Not a constructor and not an initialiser, neither of which is
    /// reachable through a `.` at all, and not a *nested type* either — that is
    /// a type of its own, reached by its own name, and reporting it here would
    /// offer it twice. It is the same list `members::is_member` admits from a
    /// file, minus the type kinds. `members_of` drops anything else rather than
    /// render it, but the obligation is stated because a dropped member is a
    /// missing answer with nothing to show why.
    ///
    /// [`ExternalMember::site`] must name the source that *declares* the
    /// member when it names one at all — the same frame [`ExternalMember::owner`]
    /// names, never the type the question was asked about. `None` is always a
    /// legal answer for a site (this world cannot say where the member is
    /// written), but naming the wrong frame is not: a caller that opens it
    /// would land in the receiver's file for a member the receiver only
    /// inherited.
    fn members(&self, fqn: &str) -> Option<ExternalMembers>;

    /// [`Externals::members`] for a *raw* receiver: JLS 4.8 erases a raw
    /// type's supertypes, so no supertype edge binds anything and an
    /// inherited member's type variable stays unresolved rather than taking
    /// an argument the edge wrote. The caller erases what is left.
    ///
    /// The default is `members` itself, which is right for a world whose
    /// closures bind no edge — a hand-built one.
    fn raw_members(&self, fqn: &str) -> Option<ExternalMembers> {
        self.members(fqn)
    }

    /// The members a source generator writes into `fqn` as `file` declares
    /// it — Lombok's getters, setters and `builder()` — as facts, because no
    /// file holds their declarations: the file-local walk reads what is
    /// written and asks this for the rest. Each one's
    /// [`ExternalMember::owner`] is `fqn`, and none is another file's.
    ///
    /// The default is none, which is right for a world that runs no
    /// generator — a hand-built one.
    fn generated_members(&self, _fqn: &str, _file: &crate::FileId) -> Vec<ExternalMember> {
        Vec::new()
    }

    /// The constructors `fqn` declares itself — what `new fqn(…)` chooses
    /// among, never an inherited one (JLS 8.8) — with their parameter types
    /// in `fqn`'s own variables, as [`ExternalMembers::type_parameters`]
    /// names them.
    ///
    /// `None` unless the list is whole: a type with no written constructor,
    /// one a generator may add a constructor to, or one read from a file that
    /// did not parse. A lone constructor of the right arity is picked, so a
    /// short list is a wrong jump rather than a missing one.
    ///
    /// The default is none, which is right for a world that reads no
    /// constructor — a hand-built one.
    fn constructors(&self, _fqn: &str) -> Option<ExternalMembers> {
        None
    }

    /// Whether `name`, written in the file `syntax` describes, may mean a
    /// type [`Externals::resolve`]'s tiers do not see: a member type one of
    /// the file's types inherits, or one a static import brings in.
    ///
    /// The default is yes, so a world that cannot say refuses.
    fn may_shadow_type(&self, _name: &str, _syntax: &javelin_parser::FileSyntax) -> bool {
        true
    }
}

/// What a receiver of one external type can select, and whether the answer is
/// the whole of it.
#[derive(Debug, Clone)]
pub struct ExternalMembers {
    /// The members, with every [`ExternalType::Variable`] among their types
    /// naming one of [`ExternalMembers::type_parameters`]: a member inherited
    /// from a supertype is expressed in the asked type's own variables, so a
    /// receiver's type arguments bind all of them at once.
    ///
    /// Shared, because a resolver asks for one type's list once per call on
    /// it: cloning the list is a pointer bump per member, not a deep copy.
    pub members: Vec<Arc<ExternalMember>>,
    /// The asked type's own type parameters, in order — what a receiver's
    /// type arguments bind to. Empty for a type that declares none, or one
    /// whose parameters this world does not know.
    pub type_parameters: Vec<String>,
    /// Whether the closure walked was whole: every type it had to visit was
    /// held, every one of their supertype lists was complete, and the walk did
    /// not run out of budget.
    ///
    /// `false` means members may be missing, which is not the same as the
    /// answer being wrong — what is here is still true. It is what stops a
    /// completion list being marked complete, so a client keeps asking rather
    /// than filtering a short list locally.
    pub complete: bool,
}

/// One member a receiver of an external type can select, as facts rather than
/// as a declaration: there is no file to read it out of.
#[derive(Debug, Clone)]
pub struct ExternalMember {
    pub name: String,
    pub kind: DeclKind,
    pub is_static: bool,
    /// A method with no body: what a functional interface has exactly one of.
    pub is_abstract: bool,
    /// Callable from any package. `false` where the world cannot say, which
    /// keeps this member from ever being the most specific one.
    pub is_public: bool,
    /// A field's type or a method's return type.
    pub declared: ExternalType,
    /// A method's formal parameters, in order. Empty for anything else.
    pub parameters: Vec<ExternalType>,
    /// Their names, in the same order, or empty where the world cannot say.
    pub parameter_names: Vec<String>,
    /// A generic method's own type parameters — each an
    /// [`ExternalType::Unresolved`] of that name in the types above — which a
    /// call's arguments can bind. Empty where the world cannot say.
    pub type_parameters: Vec<String>,
    /// Whether the last of those parameters is a `...`.
    pub variadic: bool,
    /// Whether `declared` was read where nothing could have meant another
    /// type by a name in it — what a chain hop through a member written in
    /// another workspace file needs. A JDK or JAR member is read where it is
    /// written, and nothing asks.
    pub certain: bool,
    /// The fully-qualified name of the type that declares this member, which
    /// is not always the type asked about — see [`Externals::members`] for why
    /// it may not be filled in with the latter.
    pub owner: String,
    /// Its place among `owner`'s members in the facts it was read from, which
    /// with `owner` names a member that has no site to name it by — a JAR
    /// member read without sources, a JDK member's archive entry. Kept when a
    /// closure substitutes the member into a receiver's variables, so two
    /// receivers of one member agree on it.
    pub ordinal: u32,
    /// Where this member is written down, for a caller that can go and open
    /// it.
    ///
    /// `None` is *this world cannot say where* — a hand-built test world, for
    /// instance. A caller holding `None` answers nothing, the same refusal an
    /// unheld fqn gets.
    pub site: Option<ExternalSite>,
}

/// Where an external member is written down, for a caller that can go and
/// open it.
///
/// `source` is opaque to this crate: it is whatever the [`Externals`]
/// implementation needs to find the text again — a `src.zip` entry name, for
/// the only implementation there is. Nothing here interprets it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalSite {
    pub source: String,
    /// Byte offsets of the member's name within that source.
    pub name_start: u32,
    pub name_end: u32,
    /// The name's line and UTF-16 characters, when whatever recorded the
    /// member recorded those too.
    ///
    /// `Some` is a contract, not a convenience: it says `source` is the path
    /// of a file on this machine and this range is where the name was when it
    /// was read, so a caller can point at it without opening anything. The
    /// workspace's own members carry one; a JDK member does not, because its
    /// `source` is an entry inside an archive and its offsets have to be
    /// converted against the text once that entry is extracted.
    pub name_range: Option<javelin_document::Range>,
    /// Byte offsets of the declaration's `/** … */` in `source`, where it
    /// has one. Read only after the name has been found where `name_start`
    /// says, so a stale offset reads nothing rather than another comment.
    pub doc_span: Option<(u32, u32)>,
}

/// A member's type, as far as whatever built the fact could resolve it.
///
/// Deliberately not a [`crate::Type`]: nothing here is addressable in a file,
/// so `Type::Declared` cannot arise, and the world that fills these in has no
/// [`crate::Files`] to resolve a name against. `members::member_type` maps one
/// onto a `Type` on demand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExternalType {
    Void,
    Primitive(Primitive),
    /// A name that resolved to exactly one type, fully qualified, with any
    /// type arguments dropped — a fact describes a declaration and not an
    /// instantiation.
    ///
    /// Resolved is not the same as *held*: the resolver's single-type-import
    /// tier answers from the import alone without consulting the fact set, so
    /// this can name an fqn the world has no fact for — one skipped for size,
    /// or never read. Nothing needs to check: asking for such a name's members
    /// answers `None`, which is an empty list and a withheld completeness
    /// flag, and probing per member would only turn a rendered `Foo` into a
    /// `com.example.Foo`.
    Class(String),
    /// A name that resolved to exactly one type, with the type arguments it
    /// was written with: `List<E>`.
    Parameterized(String, Vec<ExternalType>),
    /// A type parameter of the type the members were asked about — see
    /// [`ExternalMembers::members`] — which a receiver's argument replaces.
    Variable(String),
    /// `? extends bound`, as a type argument.
    Extends(Box<ExternalType>),
    /// `? super bound`, as a type argument.
    Super(Box<ExternalType>),
    /// Nothing settled it: a method's own type variable, an array, an
    /// unbounded `?`, an ambiguous name, one
    /// nothing reachable declares, or a member that writes no type at all —
    /// which carries an empty string, a legitimate value and not a missing
    /// one. The text as written, claiming exactly as much as a
    /// [`crate::Type::Unknown`] claims, which is nothing.
    Unresolved(String),
}

impl ExternalMember {
    /// The member as one signature line: `int length()`,
    /// `String substring(int beginIndex, int endIndex)`,
    /// `String CASE_INSENSITIVE_ORDER`.
    ///
    /// Simple names throughout and never the fully-qualified ones the facts
    /// carry. A completion item's detail and a hover both sit next to the name
    /// the author wrote, and `java.lang.String substring(int, int)` is noise
    /// in that position.
    ///
    /// ponytail: the signature is the fact's as declared, `E get(int)`, and
    /// not the receiver's substitution of it, because a fact does not know
    /// its receiver; the upgrade is rendering through `members::member_type`.
    ///
    /// A parameter's name follows its type wherever the facts hold every
    /// one — `substring(int beginIndex, int endIndex)` — and no parameter is
    /// named where any is not, which is a class file read without its
    /// sources.
    pub fn signature(&self) -> String {
        let mut out = String::new();
        let declared = rendered(&self.declared);
        // A member with nothing written renders as its bare name rather than
        // with a leading space where a type would have been.
        if !declared.is_empty() {
            out.push_str(&declared);
            out.push(' ');
        }
        out.push_str(&self.name);
        if !matches!(self.kind, DeclKind::Method | DeclKind::Constructor) {
            return out;
        }
        out.push('(');
        let last = self.parameters.len().saturating_sub(1);
        for (index, parameter) in self.parameters.iter().enumerate() {
            if index > 0 {
                out.push_str(", ");
            }
            out.push_str(&rendered(parameter));
            // A variadic parameter's fact holds its *element* type, so
            // rendering it as written would name a type the call does not
            // take.
            if self.variadic && index == last {
                out.push_str("...");
            }
            if self.parameter_names.len() == self.parameters.len() {
                out.push(' ');
                out.push_str(&self.parameter_names[index]);
            }
        }
        out.push(')');
        out
    }
}

/// One member type as the author would have written it.
pub(crate) fn rendered(ty: &ExternalType) -> String {
    match ty {
        ExternalType::Void => "void".to_owned(),
        ExternalType::Primitive(primitive) => primitive.keyword().to_owned(),
        ExternalType::Class(fqn) => last_segment(fqn).to_owned(),
        ExternalType::Parameterized(fqn, arguments) => {
            let arguments: Vec<String> = arguments.iter().map(rendered).collect();
            format!("{}<{}>", last_segment(fqn), arguments.join(", "))
        }
        ExternalType::Variable(name) => name.clone(),
        ExternalType::Extends(bound) => format!("? extends {}", rendered(bound)),
        ExternalType::Super(bound) => format!("? super {}", rendered(bound)),
        ExternalType::Unresolved(written) => written.clone(),
    }
}

/// The simple name inside a source-form fully-qualified one: `Entry` for
/// `java.util.Map.Entry`.
///
/// Source form is what makes this a `rsplit` rather than a parse — a nested
/// type is joined with `.` and never with the classfile's `$`, which is
/// `javelin-classpath`'s recorded choice.
pub(crate) fn last_segment(fqn: &str) -> &str {
    fqn.rsplit('.').next().unwrap_or(fqn)
}

/// The Phase 2 engine: every question answers "cannot say".
///
/// Not a stub waiting to be filled in. It is the honest answer for a server
/// that found no JDK, and it is what lets every test written before the
/// classpath existed still describe the behaviour it was written to describe.
///
/// `Debug` because the workspace warns on a public type without it. No
/// supertrait bound of the same kind on [`Externals`]: nothing stores one, so
/// the lint never fires on a struct holding it, and requiring `Debug` of an
/// implementation would demand it of whatever the server wraps for no reader.
#[derive(Debug)]
pub struct NoExternals;

impl Externals for NoExternals {
    fn resolve(
        &self,
        _name: &str,
        _package: Option<&str>,
        _imports: &[ImportDecl],
    ) -> Option<String> {
        None
    }

    fn is_subtype(&self, _from: &str, _to: &str) -> Option<bool> {
        None
    }

    fn boxed(&self, _primitive: Primitive) -> Option<String> {
        None
    }

    fn unboxed(&self, _fqn: &str) -> Option<Primitive> {
        None
    }

    fn members(&self, _fqn: &str) -> Option<ExternalMembers> {
        None
    }

    /// No type outside the file, so none to inherit or import a member type
    /// from; the file's own are the resolver's.
    fn may_shadow_type(&self, _name: &str, _syntax: &javelin_parser::FileSyntax) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::testing::{class, field, method};
    use super::*;

    const STRING: &str = "java.lang.String";

    #[test]
    fn a_signature_renders_simple_names_and_a_methods_parameter_types() {
        assert_eq!(
            method(
                STRING,
                "length",
                ExternalType::Primitive(Primitive::Int),
                vec![]
            )
            .signature(),
            "int length()"
        );
        // The fqn is what the fact carries and the simple name is what a
        // reader wants beside the name they typed.
        assert_eq!(
            method(
                STRING,
                "substring",
                class(STRING),
                vec![
                    ExternalType::Primitive(Primitive::Int),
                    ExternalType::Primitive(Primitive::Int)
                ]
            )
            .signature(),
            "String substring(int, int)"
        );
        // Named only where every parameter is.
        let mut named = method(
            STRING,
            "substring",
            class(STRING),
            vec![
                ExternalType::Primitive(Primitive::Int),
                ExternalType::Primitive(Primitive::Int),
            ],
        );
        named.parameter_names = vec!["beginIndex".to_owned(), "endIndex".to_owned()];
        assert_eq!(
            named.signature(),
            "String substring(int beginIndex, int endIndex)"
        );
        named.parameter_names.pop();
        assert_eq!(named.signature(), "String substring(int, int)");
        assert_eq!(
            field(STRING, "CASE_INSENSITIVE_ORDER", class(STRING)).signature(),
            "String CASE_INSENSITIVE_ORDER"
        );
        // A nested type's fqn is joined with `.`, so its simple name is the
        // last segment of that and not of a binary `Map$Entry`.
        assert_eq!(
            field(STRING, "entry", class("java.util.Map.Entry")).signature(),
            "Entry entry"
        );
        assert_eq!(
            method(STRING, "wait", ExternalType::Void, vec![]).signature(),
            "void wait()"
        );
    }

    #[test]
    fn a_signature_says_only_what_the_facts_say() {
        // A variadic parameter's fact holds its element type, so rendering it
        // as written would name a type the call does not take.
        let mut format = method(
            STRING,
            "format",
            class(STRING),
            vec![class(STRING), class("java.lang.Object")],
        );
        format.variadic = true;
        assert_eq!(format.signature(), "String format(String, Object...)");

        // An unresolved type renders as the text nothing settled, which is all
        // there is to say about it — and an empty one, from a member that
        // writes no type at all, renders as no type rather than as a space.
        assert_eq!(
            method(
                STRING,
                "chars",
                ExternalType::Unresolved("IntStream".into()),
                vec![]
            )
            .signature(),
            "IntStream chars()"
        );
        assert_eq!(
            field(STRING, "RED", ExternalType::Unresolved(String::new())).signature(),
            "RED"
        );
    }

    #[test]
    fn a_signature_renders_type_arguments_and_wildcards_as_declared() {
        let variable = |name: &str| ExternalType::Variable(name.into());
        let entries = ExternalType::Parameterized(
            "java.util.Set".into(),
            vec![ExternalType::Parameterized(
                "java.util.Map.Entry".into(),
                vec![variable("K"), variable("V")],
            )],
        );
        assert_eq!(
            method("java.util.Map", "entrySet", entries, vec![]).signature(),
            "Set<Entry<K, V>> entrySet()"
        );
        let consumer = ExternalType::Parameterized(
            "java.util.function.Consumer".into(),
            vec![ExternalType::Super(Box::new(variable("T")))],
        );
        assert_eq!(
            method(
                "java.lang.Iterable",
                "forEach",
                ExternalType::Void,
                vec![consumer]
            )
            .signature(),
            "void forEach(Consumer<? super T>)"
        );
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A world the tests in this crate hand-build, for the ones that need it
    //! to hold members.
    //!
    //! Never a real JDK, for the reason [`super::NoExternals`] is not enough:
    //! a test that read one would depend on what is installed on the machine
    //! and cost eleven seconds. Hand-built also keeps what is under test on
    //! this side of the door — reading a `TypeWorld` here would test
    //! `javelin-classpath`.
    //!
    //! [`World::members`] deliberately does not walk anything. Closing the
    //! supertype chain is the *implementation's* obligation under
    //! [`Externals::members`], so a fixture states the closed list it wants a
    //! receiver to see and this door hands it over unchanged — which is
    //! exactly the input `javelin-semantic` has to cope with either way.

    use javelin_parser::{DeclKind, ImportDecl};

    use super::{ExternalMember, ExternalMembers, ExternalType, Externals};
    use crate::types::Primitive;

    /// One type this world holds.
    pub(crate) struct Held {
        fqn: String,
        supertypes: Vec<String>,
        members: Vec<ExternalMember>,
        complete: bool,
        type_parameters: Vec<String>,
    }

    #[derive(Default)]
    pub(crate) struct World {
        types: Vec<Held>,
        generated: Vec<(String, ExternalMember)>,
        constructors: Vec<(String, Vec<ExternalMember>)>,
        shadowed: Vec<String>,
    }

    impl World {
        /// A world holding these names and nothing about them: no supertype,
        /// no member. Enough for a name to resolve.
        /// A world in which `name` may mean a type its tiers do not see.
        pub(crate) fn shadowing(mut self, name: &str) -> Self {
            self.shadowed.push(name.to_owned());
            self
        }

        pub(crate) fn holding(fqns: &[&str]) -> Self {
            fqns.iter().fold(Self::default(), |world, fqn| {
                world.with(fqn, &[], Vec::new())
            })
        }

        pub(crate) fn with(
            mut self,
            fqn: &str,
            supertypes: &[&str],
            members: Vec<ExternalMember>,
        ) -> Self {
            self.types.push(Held {
                fqn: fqn.to_owned(),
                supertypes: supertypes.iter().map(|name| (*name).to_owned()).collect(),
                members,
                complete: true,
                type_parameters: Vec::new(),
            });
            self
        }

        /// The type just added declares these type parameters, which its
        /// members' [`ExternalType::Variable`]s name.
        pub(crate) fn generic(mut self, names: &[&str]) -> Self {
            self.types
                .last_mut()
                .expect("a type to make generic")
                .type_parameters = names.iter().map(|name| (*name).to_owned()).collect();
            self
        }

        /// The same, holding a closure the world says it could not finish —
        /// [`ExternalMembers::complete`] is `false`, so the members are true
        /// and may not be all of them.
        ///
        /// A builder rather than a public field because a `Held` is opaque:
        /// what a test wants to say is "this list is short", and the world is
        /// the only thing that can say it.
        pub(crate) fn with_partial(
            mut self,
            fqn: &str,
            supertypes: &[&str],
            members: Vec<ExternalMember>,
        ) -> Self {
            self = self.with(fqn, supertypes, members);
            self.types.last_mut().expect("just pushed").complete = false;
            self
        }

        /// A member a generator writes into the file-local type `fqn`, which
        /// [`Externals::generated_members`] answers in any file.
        pub(crate) fn generating(mut self, fqn: &str, member: ExternalMember) -> Self {
            self.generated.push((fqn.to_owned(), member));
            self
        }

        /// The whole list of constructors `fqn` declares, which
        /// [`Externals::constructors`] answers in its own variables.
        pub(crate) fn constructing(mut self, fqn: &str, constructors: Vec<ExternalMember>) -> Self {
            self.constructors.push((fqn.to_owned(), constructors));
            self
        }

        fn held(&self, fqn: &str) -> Option<&Held> {
            self.types.iter().find(|held| held.fqn == fqn)
        }
    }

    /// A method fact, public. Its `is_static` and `variadic` are `false`; a test that
    /// wants either sets it on the returned value, whose fields are public.
    pub(crate) fn method(
        owner: &str,
        name: &str,
        declared: ExternalType,
        parameters: Vec<ExternalType>,
    ) -> ExternalMember {
        ExternalMember {
            type_parameters: Vec::new(),
            parameter_names: Vec::new(),
            name: name.to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared,
            parameters,
            variadic: false,
            certain: false,
            owner: owner.to_owned(),
            ordinal: 0,
            // Hand-built, so there is no source to name; `None` is the
            // honest value rather than a made-up site.
            site: None,
        }
    }

    pub(crate) fn field(owner: &str, name: &str, declared: ExternalType) -> ExternalMember {
        ExternalMember {
            type_parameters: Vec::new(),
            parameter_names: Vec::new(),
            name: name.to_owned(),
            kind: DeclKind::Field,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared,
            parameters: Vec::new(),
            variadic: false,
            certain: false,
            owner: owner.to_owned(),
            ordinal: 0,
            site: None,
        }
    }

    /// `ExternalType::Class(fqn)`, short enough to write inline in a fixture.
    pub(crate) fn class(fqn: &str) -> ExternalType {
        ExternalType::Class(fqn.to_owned())
    }

    impl Externals for World {
        /// By last segment, and `None` when two names share one — a real
        /// ambiguity rather than a missing entry, and nothing downstream can
        /// tell which of the two it would have got.
        fn resolve(
            &self,
            name: &str,
            _package: Option<&str>,
            _imports: &[ImportDecl],
        ) -> Option<String> {
            // A dotted name is held as written, or is its first segment's
            // type with the rest nested in it — `simple_name`'s rule. A
            // simple name ignores package and imports here.
            if name.contains('.') {
                if self.held(name).is_some() {
                    return Some(name.to_owned());
                }
                let (first, rest) = name.split_once('.')?;
                let nested = format!("{}.{rest}", self.resolve(first, None, &[])?);
                return self.held(&nested).map(|_| nested);
            }
            let mut hits = self
                .types
                .iter()
                .filter(|held| super::last_segment(&held.fqn) == name);
            let first = hits.next()?;
            hits.next().is_none().then(|| first.fqn.clone())
        }

        /// Reflexive on exactly the names it holds, and `java.lang.Object`
        /// answered without a fact, both per the trait's obligations.
        fn is_subtype(&self, from: &str, to: &str) -> Option<bool> {
            self.held(from)?;
            self.held(to)?;
            if to == "java.lang.Object" {
                return Some(true);
            }
            let mut seen: Vec<&str> = Vec::new();
            let mut stack = vec![from];
            while let Some(current) = stack.pop() {
                if current == to {
                    return Some(true);
                }
                if seen.contains(&current) {
                    continue;
                }
                seen.push(current);
                // `?`: a supertype this world does not hold leaves the closure
                // unshowable rather than absent.
                let held = self.held(current)?;
                stack.extend(held.supertypes.iter().map(String::as_str));
            }
            Some(false)
        }

        fn boxed(&self, primitive: Primitive) -> Option<String> {
            let fqn = box_class(primitive)?;
            self.held(fqn).map(|held| held.fqn.clone())
        }

        /// Total over the box classes this world holds, per the trait's
        /// obligation.
        fn unboxed(&self, fqn: &str) -> Option<Primitive> {
            let primitive = [Primitive::Int, Primitive::Long]
                .into_iter()
                .find(|primitive| box_class(*primitive) == Some(fqn))?;
            self.held(fqn).map(|_| primitive)
        }

        fn generated_members(&self, fqn: &str, _file: &crate::FileId) -> Vec<ExternalMember> {
            self.generated
                .iter()
                .filter(|(owner, _)| owner == fqn)
                .map(|(_, member)| member.clone())
                .collect()
        }

        fn members(&self, fqn: &str) -> Option<ExternalMembers> {
            let held = self.held(fqn)?;
            Some(ExternalMembers {
                members: held
                    .members
                    .iter()
                    .cloned()
                    .map(std::sync::Arc::new)
                    .collect(),
                type_parameters: held.type_parameters.clone(),
                complete: held.complete,
            })
        }

        fn constructors(&self, fqn: &str) -> Option<ExternalMembers> {
            let (_, constructors) = self.constructors.iter().find(|(owner, _)| owner == fqn)?;
            Some(ExternalMembers {
                members: constructors
                    .iter()
                    .cloned()
                    .map(std::sync::Arc::new)
                    .collect(),
                type_parameters: self
                    .held(fqn)
                    .map(|held| held.type_parameters.clone())
                    .unwrap_or_default(),
                complete: true,
            })
        }

        fn may_shadow_type(&self, name: &str, _syntax: &javelin_parser::FileSyntax) -> bool {
            self.shadowed.iter().any(|shadowed| shadowed == name)
        }
    }

    /// The two boxes the tests need. Not a table of `java.lang`: the real
    /// mapping lives with the classpath, and this one only has to be
    /// consistent with itself.
    fn box_class(primitive: Primitive) -> Option<&'static str> {
        match primitive {
            Primitive::Int => Some("java.lang.Integer"),
            Primitive::Long => Some("java.lang.Long"),
            _ => None,
        }
    }
}
