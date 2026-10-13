//! The members Lombok writes into a type, read off the annotations the type
//! and its fields carry — so a reader can follow `account.getIban()` into a
//! class that declares only `@Data` and a field.
//!
//! An annotation counts only when it is Lombok's: written `@lombok.Getter`,
//! or `@Getter` in a file that imports `lombok.Getter` or `lombok.*`. What is
//! generated follows Lombok's own rules — names, which fields, which methods
//! an existing one suppresses — for the subset modelled here, and nothing is
//! generated where a rule outside the subset could change a name:
//! `@Accessors` on the type or one around it, a `@Builder` with arguments other than
//! `toBuilder = true`, a `@Singular` on a field type or with an argument outside
//! Lombok's own recipes, a generic type's builder or `@With`, and a
//! `@SuperBuilder` on a generic type or one extending a generic type.
//!
//! `@SuperBuilder`'s `builder()` is typed as the private `FooBuilderImpl` it
//! returns, where Lombok writes `FooBuilder<?, ?>`: the members are the same,
//! and the edges bind each setter's `B` to the implementation and `build()`'s
//! `C` to the type, which a wildcard receiver would leave unknown.
//!
//! Each generated member's name span is its field's name — or the type's for
//! `builder()`, and an empty one just after and just before it for `build()`
//! and `toBuilder()` — so a jump lands on what the author wrote, and a member is navigated as its property: a getter
//! and its field are one site, which is the key references and the call
//! hierarchy use. Its declaration span is empty, so no use is ever inside it.
//!
//! What this module generates is stored by the workspace walk, which skips a
//! file whose time and size it has stored — so a change to any rule here is a
//! `javelin_index::SCHEMA_VERSION` bump, or a cache keeps the old answer for
//! every file nobody edits.
//!
//! A log annotation's field is sited where the type's annotations start, and
//! named `log` here: the index renames it where a `lombok.config` says.
//!
//! ponytail: what is not generated is a missing member — `@CustomLog`'s
//! field, a static constructor, `@Delegate`, `@ExtensionMethod`,
//! `@SuperBuilder`'s `$` methods, Guava's `@Singular` recipes,
//! `@FieldNameConstants`, a `@Builder` or a `@Singular` on a constructor or a
//! method, and the
//! `lombok.config` keys that rename what is generated, which the walk detects
//! and the index drops these members for. The upgrade is one rule at a time,
//! each with Lombok's own tests as its oracle.

use javelin_parser::{Decl, DeclKind, ImportDecl, Span};

use crate::sources::{WrittenMember, split_arguments, trailing_arguments, type_parameter_names};

/// What Lombok generates for one type.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Generated {
    /// Members of the type itself.
    pub members: Vec<WrittenMember>,
    /// The builder classes nested in the type: `@Builder`'s one, or
    /// `@SuperBuilder`'s abstract builder and, for a concrete type, its
    /// implementation.
    pub builders: Vec<GeneratedType>,
    /// What the type's Lombok annotations say about `members`.
    pub lombok: LombokUse,
}

/// A class Lombok nests in the type it generates for.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GeneratedType {
    pub simple: String,
    /// Its own type parameters' names, `@SuperBuilder`'s `C` and `B`.
    pub type_parameters: Vec<String>,
    /// Its supertypes as written in the annotated type's file.
    pub supertypes: Vec<String>,
    pub members: Vec<WrittenMember>,
}

/// Whether Lombok touches a type, and whether what it writes there is all
/// in [`Generated`] — which is what a list claiming to be whole must know.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct LombokUse {
    /// The type, a field, a method or a constructor of it carries a Lombok
    /// annotation.
    pub annotated: bool,
    /// One of them may write a member [`lombok_members`] does not.
    pub unmodelled: bool,
    /// `@ExtensionMethod`: inside the type, any receiver may have methods
    /// no list holds.
    pub extensions: bool,
}

impl LombokUse {
    /// The three flags as bits, for a store.
    pub fn bits(self) -> i64 {
        i64::from(self.annotated)
            | i64::from(self.unmodelled) << 1
            | i64::from(self.extensions) << 2
    }

    /// Each flag of either.
    pub fn or(self, other: Self) -> Self {
        Self {
            annotated: self.annotated || other.annotated,
            unmodelled: self.unmodelled || other.unmodelled,
            extensions: self.extensions || other.extensions,
        }
    }

    pub fn from_bits(bits: i64) -> Self {
        Self {
            annotated: bits & 1 != 0,
            unmodelled: bits & 2 != 0,
            extensions: bits & 4 != 0,
        }
    }
}

/// Lombok annotations that write no member another type can name: they
/// write a method `Object` declares, a constructor, a `private` field, or
/// nothing at all.
const ADDS_NO_NAME: [&str; 26] = [
    "lombok.ToString",
    "lombok.ToString.Include",
    "lombok.ToString.Exclude",
    "lombok.EqualsAndHashCode.Include",
    "lombok.EqualsAndHashCode.Exclude",
    "lombok.NonNull",
    "lombok.Generated",
    "lombok.SneakyThrows",
    "lombok.Synchronized",
    "lombok.Locked",
    "lombok.Locked.Read",
    "lombok.Locked.Write",
    "lombok.Builder.Default",
    "lombok.Builder.ObtainVia",
    "lombok.extern.jackson.Jacksonized",
    "lombok.experimental.NonFinal",
    "lombok.experimental.PackagePrivate",
    "lombok.CustomLog",
    "lombok.extern.java.Log",
    "lombok.extern.slf4j.Slf4j",
    "lombok.extern.slf4j.XSlf4j",
    "lombok.extern.log4j.Log4j",
    "lombok.extern.log4j.Log4j2",
    "lombok.extern.apachecommons.CommonsLog",
    "lombok.extern.jbosslog.JBossLog",
    "lombok.extern.flogger.Flogger",
];

/// The constructor annotations, which write a static factory method when
/// given a `staticName`.
const CONSTRUCTORS: [&str; 3] = [
    "lombok.NoArgsConstructor",
    "lombok.AllArgsConstructor",
    "lombok.RequiredArgsConstructor",
];

/// What this module generates, on a class's or an enum's own declaration —
/// `@Data` and `@Value` short of a `staticConstructor` — and on a field.
const ON_TYPE: [&str; 10] = [
    "lombok.Getter",
    "lombok.Setter",
    "lombok.Data",
    "lombok.Value",
    "lombok.EqualsAndHashCode",
    "lombok.experimental.FieldDefaults",
    "lombok.With",
    "lombok.experimental.Wither",
    "lombok.Builder",
    "lombok.experimental.SuperBuilder",
];
const ON_FIELD: [&str; 5] = [
    "lombok.Getter",
    "lombok.Setter",
    "lombok.With",
    "lombok.experimental.Wither",
    "lombok.Singular",
];

/// The flags every annotation on `owner` and its members sets by its name
/// alone; [`lombok_members`] adds `unmodelled` where a modelled one is used
/// outside the rules it follows.
fn lombok_use(owner: &Decl, imports: &[ImportDecl]) -> LombokUse {
    let mut found = LombokUse::default();
    let modelled_kind = matches!(owner.kind, DeclKind::Class | DeclKind::Enum);
    // Both on demand, an annotation neither import names singly may be
    // either package's — `lombok_name` reads it as neither.
    let ambiguous = ["lombok", "lombok.experimental"].iter().all(|package| {
        imports
            .iter()
            .any(|import| !import.is_static && import.is_on_demand && import.name == *package)
    });
    let parts = std::iter::once(owner).chain(owner.children.iter().filter(|child| {
        matches!(
            child.kind,
            DeclKind::Field
                | DeclKind::Method
                | DeclKind::Constructor
                | DeclKind::EnumConstant
                | DeclKind::RecordComponent
        )
    }));
    for part in parts {
        let either = part.modifiers.iter().filter(|modifier| {
            ambiguous && modifier.starts_with('@') && unclaimed(modifier, imports)
        });
        for modifier in either {
            found.annotated = true;
            found.unmodelled = true;
            found.extensions |= modifier[1..].split(['(', ' ']).next() == Some("ExtensionMethod");
        }
        let is_type = std::ptr::eq(part, owner);
        for annotation in annotations(part, imports) {
            found.annotated = true;
            let name = annotation.name.as_str();
            found.extensions |= name == "lombok.experimental.ExtensionMethod";
            let modelled = if ADDS_NO_NAME.contains(&name) {
                true
            } else if CONSTRUCTORS.contains(&name) {
                !annotation.args.contains("staticName")
            } else if is_type {
                modelled_kind
                    && ON_TYPE.contains(&name)
                    && !(matches!(name, "lombok.Data" | "lombok.Value")
                        && annotation.args.contains("staticConstructor"))
            } else {
                modelled_kind && part.kind == DeclKind::Field && ON_FIELD.contains(&name)
            };
            found.unmodelled |= !modelled;
        }
    }
    found
}

/// Whether an annotation is one no single import and no `java.lang` name
/// accounts for.
fn unclaimed(modifier: &str, imports: &[ImportDecl]) -> bool {
    let written = modifier.trim_start_matches('@');
    let name = written.split(['(', ' ']).next().unwrap_or(written);
    let first = name.split('.').next().unwrap_or(name);
    !name.contains('.')
        && !matches!(
            first,
            "Override" | "Deprecated" | "SuppressWarnings" | "FunctionalInterface" | "SafeVarargs"
        )
        && !imports.iter().any(|import| {
            !import.is_static
                && !import.is_on_demand
                && import.name.rsplit('.').next() == Some(first)
        })
}

/// What Lombok generates for `owner`, a type declared inside `enclosing` —
/// outermost first, empty for a top-level type — in a file with these
/// `imports`. Empty for a type that carries no Lombok annotation, and for
/// one whose annotations are outside what this module models.
pub fn lombok_members(owner: &Decl, enclosing: &[&Decl], imports: &[ImportDecl]) -> Generated {
    let mut generated = Generated {
        lombok: lombok_use(owner, imports),
        ..Generated::default()
    };
    // Lombok writes its setters into a builder class the type around it
    // writes by hand, under whatever name `builderClassName` gives it, so
    // a class nested in a type that builds is never known whole.
    let builds_around = enclosing.last().is_some_and(|outer| {
        std::iter::once(*outer)
            .chain(
                outer
                    .children
                    .iter()
                    .filter(|child| matches!(child.kind, DeclKind::Method | DeclKind::Constructor)),
            )
            .flat_map(|part| annotations(part, imports))
            .any(|found| {
                matches!(
                    found.name.as_str(),
                    "lombok.Builder" | "lombok.experimental.SuperBuilder"
                )
            })
    });
    generated.lombok.unmodelled |= owner.kind == DeclKind::Class && builds_around;
    if !matches!(owner.kind, DeclKind::Class | DeclKind::Enum) {
        return generated;
    }
    let on_type = annotations(owner, imports);
    let log = on_type
        .iter()
        .find_map(|found| Some((logger(&found.name)?, found.args)));
    // An `access` makes the field another level's, or none at all with
    // `NONE`: what is generated is not known to be private.
    let leveled = log.is_some_and(|(_, args)| args.contains("access"));
    generated.lombok.unmodelled |= leveled;
    if let Some((logger, _)) = log
        && !leveled
        && !owner
            .children
            .iter()
            .any(|child| child.kind == DeclKind::Field && child.name == "log")
    {
        // Sited where the annotations start, a byte no name starts at.
        let at = empty_at(owner.span);
        generated.members.push(WrittenMember {
            doc_span: None,
            parameter_names: Vec::new(),
            name: "log".to_owned(),
            kind: DeclKind::Field,
            is_static: true,
            is_abstract: false,
            is_public: false,
            declared: Some(logger.to_owned()),
            parameters: Vec::new(),
            variadic: false,
            name_span: at,
            span: at,
            is_private: true,
            type_parameters: Vec::new(),
        });
    }
    let fields: Vec<&Decl> = owner
        .children
        .iter()
        .filter(|child| child.kind == DeclKind::Field && !child.name.is_empty())
        .filter(|field| !field.name.starts_with('$'))
        .collect();
    let per_field: Vec<Vec<Annotation<'_>>> = fields
        .iter()
        .map(|field| annotations(field, imports))
        .collect();
    let anywhere = |name: &str| {
        on_type.iter().any(|found| found.name == name)
            || per_field.iter().flatten().any(|found| found.name == name)
    };
    // Lombok reads `@Accessors` off the field and every type around it.
    let accessors = "lombok.experimental.Accessors";
    if anywhere(accessors)
        || enclosing
            .iter()
            .any(|outer| find(&annotations(outer, imports), accessors).is_some())
    {
        generated.lombok.unmodelled |= generated.lombok.annotated;
        return generated;
    }
    let is_class = owner.kind == DeclKind::Class;
    let data = is_class && find(&on_type, "lombok.Data").is_some();
    let value = is_class && find(&on_type, "lombok.Value").is_some();
    // `@Value` makes every field final, and so does `@FieldDefaults` asked
    // to — and a final field has no setter.
    let defaults_final = find(&on_type, "lombok.experimental.FieldDefaults").is_some_and(|args| {
        args.chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .contains("makeFinal=true")
    });
    let class_getter = find(&on_type, "lombok.Getter")
        .map(access)
        .or((data || value).then_some(Access::Public));
    // `@Setter` and `@With` on an enum are errors; on its field they are not.
    let class_setter = find(&on_type, "lombok.Setter")
        .map(access)
        .or(data.then_some(Access::Public))
        .filter(|_| is_class);
    let generic = owner.type_parameters.is_some();
    let class_with = find(&on_type, "lombok.With")
        .or(find(&on_type, "lombok.experimental.Wither"))
        .map(access)
        .filter(|_| is_class);

    let methods: Vec<(&str, usize, bool)> = owner
        .children
        .iter()
        .filter(|child| child.kind == DeclKind::Method)
        .map(|method| {
            let variadic = method.parameters.last().is_some_and(|last| last.varargs);
            (method.name.as_str(), method.parameters.len(), variadic)
        })
        .collect();
    // Lombok writes no method where the type already has one of any of the
    // accessor's names, ignoring case, taking as many parameters — or a
    // variadic one, which Lombok's own example says suppresses it too.
    let exists = |names: &[String], arity: usize| {
        methods.iter().any(|(name, count, variadic)| {
            (*count == arity || *variadic) && names.iter().any(|it| it.eq_ignore_ascii_case(name))
        })
    };
    let owned_parameters = type_parameter_names(owner.type_parameters.as_deref());
    let accessor =
        |field: &Decl, name: String, declared: &str, parameters: Vec<String>| WrittenMember {
            // Lombok copies the field's Javadoc onto what it generates.
            doc_span: field.doc_span,
            // A setter's, a wither's and a builder method's one parameter is
            // named for its field, as Lombok writes it.
            parameter_names: parameters.iter().map(|_| field.name.clone()).collect(),
            name,
            kind: DeclKind::Method,
            is_static: has(field, "static"),
            is_abstract: false,
            // `access` reads `PROTECTED` and `PACKAGE` as public, so nothing
            // generated claims to be.
            is_public: false,
            declared: Some(declared.to_owned()),
            parameters,
            variadic: false,
            name_span: field.name_span,
            span: empty_at(field.name_span),
            is_private: false,
            type_parameters: owned_parameters.clone(),
        };

    // Final as written, or made final by `@Value` or `@FieldDefaults` before
    // any other handler runs — which is what Lombok's own handlers read.
    let finals: Vec<bool> = fields
        .iter()
        .zip(&per_field)
        .map(|(field, own)| {
            has(field, "final")
                || ((value || defaults_final)
                    && !own
                        .iter()
                        .any(|found| found.name == "lombok.experimental.NonFinal"))
        })
        .collect();
    for ((field, own), &is_final) in fields.iter().zip(&per_field).zip(&finals) {
        let Some(declared) = field.declared_type.as_deref() else {
            generated.lombok.unmodelled |= generated.lombok.annotated;
            continue;
        };
        let is_static = has(field, "static");
        let names = Names::of(&field.name, declared.trim() == "boolean");
        // A field's own annotation wins over its type's, which never reaches
        // a static field.
        let getter = find(own, "lombok.Getter")
            .map(access)
            .or(class_getter.filter(|_| !is_static));
        if let Some(access) = getter.filter(|access| *access != Access::None)
            && !exists(&names.getters, 0)
        {
            let mut member = accessor(field, names.getters[0].clone(), declared, Vec::new());
            member.is_private = access == Access::Private;
            generated.members.push(member);
        }
        let setter = find(own, "lombok.Setter")
            .map(access)
            .or(class_setter.filter(|_| !is_static));
        if let Some(access) = setter.filter(|access| *access != Access::None)
            && !is_final
            && !exists(&names.setters, 1)
        {
            let mut member = accessor(
                field,
                names.setters[0].clone(),
                "void",
                vec![declared.to_owned()],
            );
            member.is_private = access == Access::Private;
            generated.members.push(member);
        }
        let with = find(own, "lombok.With")
            .or(find(own, "lombok.experimental.Wither"))
            .map(access)
            .or(class_with);
        let with = with.filter(|access| *access != Access::None);
        // Lombok writes a generic type's wither; this module does not.
        generated.lombok.unmodelled |= with.is_some() && generic;
        if let Some(access) = with
            && !is_static
            && !generic
            && !(is_final && field.has_initializer)
            && !exists(&names.withers, 1)
        {
            let mut member = accessor(
                field,
                names.withers[0].clone(),
                &owner.name,
                vec![declared.to_owned()],
            );
            member.is_private = access == Access::Private;
            generated.members.push(member);
        }
    }

    // `equals` and `hashCode` override `Object`'s and add no name; the
    // protected `canEqual` does, and a compiled class holds it. Lombok skips
    // it for a final class extending `Object` — written here anyway, since a
    // member too many is a missing answer and one too few a rival class file.
    if (data || value || find(&on_type, "lombok.EqualsAndHashCode").is_some())
        && is_class
        && !methods.iter().any(|(name, _, _)| *name == "canEqual")
        && let Some(brace) = just_before_end(owner.span)
    {
        generated.members.push(WrittenMember {
            doc_span: None,
            parameter_names: vec!["other".to_owned()],
            name: "canEqual".to_owned(),
            kind: DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: false,
            declared: Some("boolean".to_owned()),
            parameters: vec!["Object".to_owned()],
            variadic: false,
            name_span: brace,
            span: brace,
            is_private: false,
            type_parameters: Vec::new(),
        });
    }

    // Lombok skips a method the type writes, by its name alone.
    let writes = |name: &str| methods.iter().any(|(written, _, _)| *written == name);
    // Sites for the methods on the builder and the type — every one an
    // offset no other written name can start at: the type's name, the byte
    // after it, the byte before it and, for `@SuperBuilder`'s `self()`, the
    // byte inside it — so a call of one is never a use of another.
    let method = |name: &str, declared: &str, is_static: bool, at: Span| WrittenMember {
        doc_span: None,
        parameter_names: Vec::new(),
        name: name.to_owned(),
        kind: DeclKind::Method,
        is_static,
        is_abstract: false,
        is_public: false,
        declared: Some(declared.to_owned()),
        parameters: Vec::new(),
        variadic: false,
        name_span: at,
        span: empty_at(at),
        is_private: false,
        type_parameters: Vec::new(),
    };
    // What the builder sets: each field a constructor would take, by a
    // setter returning `returns` or, under `@Singular`, its three adders —
    // `None` where a `@Singular` is outside what is modelled, since Lombok's
    // builder then holds methods this one does not.
    let setters = |returns: &str, scope: &[String]| -> Option<Vec<WrittenMember>> {
        let mut out = Vec::new();
        for ((field, own), &is_final) in fields.iter().zip(&per_field).zip(&finals) {
            let defaulted = own
                .iter()
                .any(|found| found.name == "lombok.Builder.Default");
            if has(field, "static") || (is_final && field.has_initializer && !defaulted) {
                continue;
            }
            let Some(declared) = field.declared_type.as_deref() else {
                continue;
            };
            let setter = |name: String, parameters: Vec<String>, names: Vec<String>| {
                let mut member = accessor(field, name, returns, parameters);
                member.parameter_names = names;
                member.is_static = false;
                member.type_parameters = scope.to_vec();
                member
            };
            let Some(args) = find(own, "lombok.Singular") else {
                out.push(setter(
                    field.name.clone(),
                    vec![declared.to_owned()],
                    vec![field.name.clone()],
                ));
                continue;
            };
            let parts = (!defaulted).then(|| singular(&field.name, declared, args))??;
            out.push(setter(parts.name, parts.parameters, parts.parameter_names));
            out.push(setter(
                field.name.clone(),
                vec![parts.plural],
                vec![field.name.clone()],
            ));
            out.push(setter(
                format!("clear{}", capitalized(&field.name)),
                Vec::new(),
                Vec::new(),
            ));
        }
        Some(out)
    };

    let super_builder = find(&on_type, "lombok.experimental.SuperBuilder");
    // Lombok refuses either builder on a non-static nested class; one in an
    // interface is static whether or not it says so.
    let static_allowed = enclosing.last().is_none_or(|outer| {
        has(owner, "static") || matches!(outer.kind, DeclKind::Interface | DeclKind::AnnotationType)
    });
    if let Some(args) = find(&on_type, "lombok.Builder")
        && is_class
        && static_allowed
        && !has(owner, "abstract")
        && super_builder.is_none()
    {
        let builder = format!("{}Builder", owner.name);
        // A generic type's builder is generic in the type's own variables,
        // and so is what builds one.
        let applied = |name: &str| match owned_parameters.is_empty() {
            true => name.to_owned(),
            false => format!("{name}<{}>", owned_parameters.join(", ")),
        };
        let returns = applied(&builder);
        let scoped = |mut member: WrittenMember, scope: Vec<String>| {
            member.type_parameters = scope;
            member
        };
        let args: String = args.chars().filter(|c| !c.is_whitespace()).collect();
        let declares_builder = owner.children.iter().any(|child| child.name == builder);
        if (args.is_empty() || args == "toBuilder=true")
            && !declares_builder
            && let Some(mut members) = setters(&returns, &owned_parameters)
        {
            if !writes("builder") {
                // `static <K, V> PairBuilder<K, V> builder()`: the variables
                // are the method's own, listed after the owner's they shadow.
                generated.members.push(scoped(
                    method("builder", &returns, true, owner.name_span),
                    [owned_parameters.clone(), owned_parameters.clone()].concat(),
                ));
            }
            if args == "toBuilder=true" && !writes("toBuilder") {
                match just_before(owner.name_span) {
                    Some(before) => generated.members.push(scoped(
                        method("toBuilder", &returns, false, before),
                        owned_parameters.clone(),
                    )),
                    // A name at the start of its line has no site to spare.
                    None => generated.lombok.unmodelled = true,
                }
            }
            members.push(scoped(
                method(
                    "build",
                    &applied(&owner.name),
                    false,
                    just_after(owner.name_span),
                ),
                owned_parameters.clone(),
            ));
            generated.builders.push(GeneratedType {
                simple: builder,
                type_parameters: owned_parameters.clone(),
                members,
                ..GeneratedType::default()
            });
        }
    }
    if let Some(args) = super_builder {
        let args: String = args.chars().filter(|c| !c.is_whitespace()).collect();
        if is_class
            && !generic
            && static_allowed
            && find(&on_type, "lombok.Builder").is_none()
            && (args.is_empty() || args == "toBuilder=true")
            && !owner
                .extends
                .as_deref()
                .is_some_and(|written| written.contains('<'))
        {
            let toward = args == "toBuilder=true";
            if let Some(types) = super_builders(owner, toward, &setters, &method, &writes) {
                generated.builders.extend(types.builders);
                generated.members.extend(types.on_type);
                generated.lombok.unmodelled |= types.short;
            }
        }
    }
    // A builder written where this module writes none — on a generic or
    // abstract type, with other arguments, beside a `@Singular` it does not
    // model — is Lombok's builder all the same, and so is a `@Singular`
    // where no builder is.
    let builds = find(&on_type, "lombok.Builder").is_some() || super_builder.is_some();
    generated.lombok.unmodelled |=
        (builds || anywhere("lombok.Singular")) && generated.builders.is_empty();
    generated
}

/// What `@SuperBuilder` writes into and beside one type.
struct SuperBuilt {
    builders: Vec<GeneratedType>,
    on_type: Vec<WrittenMember>,
    /// Lombok writes a member this module has no site for.
    short: bool,
}

/// `@SuperBuilder`'s `FooBuilder<C, B>`, which extends the superclass's
/// builder, and for a concrete type `FooBuilderImpl` with `builder()` and
/// `toBuilder()` — `None` where Lombok's builder holds what this one cannot.
///
/// `builder()` is typed as `FooBuilderImpl`, the class its value is: Lombok
/// writes `FooBuilder<?, ?>`, whose members are the same, and the engine
/// reads no member off a wildcard receiver.
fn super_builders(
    owner: &Decl,
    to_builder: bool,
    setters: &dyn Fn(&str, &[String]) -> Option<Vec<WrittenMember>>,
    method: &dyn Fn(&str, &str, bool, Span) -> WrittenMember,
    writes: &dyn Fn(&str) -> bool,
) -> Option<SuperBuilt> {
    let builder = format!("{}Builder", owner.name);
    let implementation = format!("{builder}Impl");
    if owner
        .children
        .iter()
        .any(|child| child.name == builder || child.name == implementation)
    {
        return None;
    }
    let used = used_type_names(owner);
    let class = nonclashing("C", &used);
    let own = nonclashing("B", &used);
    let scope = vec![class.clone(), own.clone()];
    let mut members = setters(&own, &scope)?;
    let after = just_after(owner.name_span);
    // `self()` and `build()` are two sites, or references on one list the
    // other's calls — save under a one-letter name, which has no byte to
    // spare, where they share one.
    let own_site = just_inside(owner.name_span).unwrap_or(after);
    for (name, declared, at) in [("self", &own, own_site), ("build", &class, after)] {
        let mut member = method(name, declared, false, at);
        member.is_abstract = true;
        member.type_parameters = scope.clone();
        members.push(member);
    }
    let supertypes = owner
        .extends
        .as_deref()
        .map(|written| {
            let written = written.trim();
            let simple = written.rsplit('.').next().unwrap_or(written);
            format!("{written}.{simple}Builder<{class}, {own}>")
        })
        .into_iter()
        .collect();
    // `toBuilder` has the builder fill itself from an instance through a
    // method of its own, which shares `toBuilder()`'s site: a call to it
    // is written in nothing but a hand-written builder.
    let fill = to_builder.then(|| just_before(owner.name_span));
    if let Some(Some(before)) = fill {
        let mut member = method("$fillValuesFrom", &own, false, before);
        member.parameters = vec![class.clone()];
        member.parameter_names = vec!["instance".to_owned()];
        member.type_parameters = scope.clone();
        members.push(member);
    }
    let mut built = SuperBuilt {
        builders: vec![GeneratedType {
            simple: builder.clone(),
            type_parameters: scope,
            supertypes,
            members,
        }],
        on_type: Vec::new(),
        short: false,
    };
    // An abstract type gets neither the implementation nor a way to make one.
    if has(owner, "abstract") {
        return Some(built);
    }
    built.builders.push(GeneratedType {
        simple: implementation.clone(),
        type_parameters: Vec::new(),
        supertypes: vec![format!("{builder}<{}, {implementation}>", owner.name)],
        members: vec![
            method("self", &implementation, false, own_site),
            method("build", &owner.name, false, after),
        ],
    });
    if !writes("builder") {
        built
            .on_type
            .push(method("builder", &implementation, true, owner.name_span));
    }
    if to_builder && !writes("toBuilder") {
        match just_before(owner.name_span) {
            Some(before) => built
                .on_type
                .push(method("toBuilder", &implementation, false, before)),
            None => built.short = true,
        }
    }
    Some(built)
}

/// The names `@SuperBuilder`'s `C` and `B` must not take: the type's, each
/// field's type written as one plain name, and the first segment of each
/// supertype — Lombok's own list.
fn used_type_names(owner: &Decl) -> Vec<String> {
    let first = |written: &str| {
        written
            .trim()
            .split(['.', '<'])
            .next()
            .unwrap_or("")
            .trim()
            .to_owned()
    };
    let mut used = vec![owner.name.clone()];
    used.extend(
        owner
            .children
            .iter()
            .filter(|child| child.kind == DeclKind::Field)
            .filter_map(|field| field.declared_type.as_deref())
            .map(str::trim)
            .filter(|written| {
                written
                    .chars()
                    .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
            })
            .map(str::to_owned),
    );
    used.extend(owner.extends.as_deref().map(first));
    used.extend(owner.implements.iter().map(|written| first(written)));
    used
}

/// `name`, or `name2`, `name3` … — the first `used` does not hold.
fn nonclashing(name: &str, used: &[String]) -> String {
    if !used.iter().any(|it| it == name) {
        return name.to_owned();
    }
    (2..)
        .map(|counter| format!("{name}{counter}"))
        .find(|candidate| !used.contains(candidate))
        .unwrap_or_default()
}

/// The singular adder `@Singular` writes for a collection field, and the
/// plural adder's one parameter type.
struct Singular {
    name: String,
    parameters: Vec<String>,
    parameter_names: Vec<String>,
    plural: String,
}

/// What `@Singular(args)` writes for a field `plural` of type `declared` —
/// `None` where Lombok reports an error or uses a recipe not modelled here:
/// Guava's collections, a raw type or a wildcard argument.
fn singular(plural: &str, declared: &str, args: &str) -> Option<Singular> {
    let mut explicit = None;
    for part in split_arguments(args) {
        let part = part.trim();
        let value = match part.split_once('=') {
            Some((key, value)) => match key.trim() {
                "value" => value.trim(),
                "ignoreNullCollections" => continue,
                _ => return None,
            },
            None => part,
        };
        let name = value.strip_prefix('"')?.strip_suffix('"')?;
        // An empty value is no value: Lombok singularizes the field's name.
        if !name.is_empty() {
            let identifier = name
                .chars()
                .all(|c| c.is_alphanumeric() || c == '_' || c == '$')
                && !name.starts_with(|c: char| c.is_ascii_digit());
            explicit = Some(identifier.then(|| name.to_owned())?);
        }
    }
    let name = match explicit {
        Some(name) => name,
        None => auto_singular(plural)?,
    };
    // Lombok matches the type by what is written, not by what it resolves to.
    let (base, inner) = trailing_arguments(declared.trim())?;
    let arguments: Vec<&str> = split_arguments(inner).into_iter().map(str::trim).collect();
    if arguments.iter().any(|argument| argument.starts_with('?')) {
        return None;
    }
    let base = base.trim();
    let simple = base
        .strip_prefix("java.util.")
        .or_else(|| {
            base.strip_prefix("java.lang.")
                .filter(|rest| *rest == "Iterable")
        })
        .unwrap_or(base);
    let one = matches!(
        simple,
        "List" | "Collection" | "Iterable" | "Set" | "SortedSet" | "NavigableSet"
    );
    let two = matches!(simple, "Map" | "SortedMap" | "NavigableMap");
    match arguments.as_slice() {
        [element] if one => Some(Singular {
            parameters: vec![(*element).to_owned()],
            parameter_names: vec![name.clone()],
            plural: format!("java.util.Collection<? extends {element}>"),
            name,
        }),
        [key, value] if two => Some(Singular {
            parameters: vec![(*key).to_owned(), (*value).to_owned()],
            parameter_names: vec![format!("{name}Key"), format!("{name}Value")],
            plural: format!("java.util.Map<? extends {key}, ? extends {value}>"),
            name,
        }),
        _ => None,
    }
}

/// Lombok's `singulars.txt`, in its order: an ending and its singular, `!`
/// for none. Upper case starts a whole word, `-` an ending only.
const SINGULARS: &[(&str, &str)] = &[
    ("quizzes", "quiz"),
    ("matrices", "matrix"),
    ("indices", "index"),
    ("vertices", "vertex"),
    ("statuses", "status"),
    ("aliases", "alias"),
    ("alias", "!"),
    ("species", "!"),
    ("Axes", "!"),
    ("-axes", "axe"),
    ("sexes", "sex"),
    ("Testes", "testis"),
    ("movies", "movie"),
    ("octopodes", "octopus"),
    ("buses", "bus"),
    ("Mice", "mouse"),
    ("Lice", "louse"),
    ("News", "!"),
    ("Men", "man"),
    ("Women", "woman"),
    ("minutiae", "minutia"),
    ("shoes", "shoe"),
    ("synopses", "synopsis"),
    ("prognoses", "prognosis"),
    ("theses", "thesis"),
    ("diagnoses", "diagnosis"),
    ("bases", "base"),
    ("analyses", "analysis"),
    ("Crises", "crisis"),
    ("children", "child"),
    ("moves", "move"),
    ("zombies", "zombie"),
    ("-quies", "quy"),
    ("-us", "!"),
    ("-is", "!"),
    ("series", "!"),
    ("-ies", "y"),
    ("-oes", "o"),
    ("hives", "hive"),
    ("-tives", "tive"),
    ("-sses", "ss"),
    ("-ches", "ch"),
    ("-xes", "x"),
    ("-shes", "sh"),
    ("-lves", "lf"),
    ("-rves", "rf"),
    ("saves", "save"),
    ("Leaves", "leaf"),
    ("-ves", "!"),
    ("-ss", "!"),
    ("-us", "!"),
    ("-s", ""),
];

/// Lombok's `Singulars.autoSingularize`: the first rule whose ending
/// `plural` has, compared without case, where a whole word's must start at
/// an upper-case letter or the name's start.
fn auto_singular(plural: &str) -> Option<String> {
    // Every rule is ASCII; slicing anything else could split a character.
    if !plural.is_ascii() {
        return None;
    }
    let bytes = plural.as_bytes();
    for (last, replacement) in SINGULARS {
        let whole_word = last.as_bytes()[0].is_ascii_uppercase();
        let ending_only = usize::from(last.starts_with('-'));
        if plural.len() < last.len() {
            continue;
        }
        let start = plural.len() - last.len() + ending_only;
        if !plural[start..].eq_ignore_ascii_case(&last[ending_only..]) {
            continue;
        }
        if whole_word
            && plural.len() != last.len()
            && !bytes[plural.len() - last.len()].is_ascii_uppercase()
        {
            continue;
        }
        if *replacement == "!" {
            return None;
        }
        let post = if !replacement.is_empty() && bytes[start].is_ascii_uppercase() {
            capitalized(replacement)
        } else {
            (*replacement).to_owned()
        };
        return Some(format!("{}{post}", &plural[..start]));
    }
    None
}

/// The type of the field a log annotation called `name` writes. `@CustomLog`'s
/// is whatever `lombok.log.custom.declaration` says, so it writes none here.
fn logger(name: &str) -> Option<&'static str> {
    Some(match name {
        "lombok.extern.slf4j.Slf4j" => "org.slf4j.Logger",
        "lombok.extern.slf4j.XSlf4j" => "org.slf4j.ext.XLogger",
        "lombok.extern.java.Log" => "java.util.logging.Logger",
        "lombok.extern.log4j.Log4j" => "org.apache.log4j.Logger",
        "lombok.extern.log4j.Log4j2" => "org.apache.logging.log4j.Logger",
        "lombok.extern.apachecommons.CommonsLog" => "org.apache.commons.logging.Log",
        "lombok.extern.jbosslog.JBossLog" => "org.jboss.logging.Logger",
        "lombok.extern.flogger.Flogger" => "com.google.common.flogger.FluentLogger",
        _ => return None,
    })
}

/// The arguments of the annotation called `name` among `found`.
fn find<'a>(found: &[Annotation<'a>], name: &str) -> Option<&'a str> {
    found
        .iter()
        .find(|annotation| annotation.name == name)
        .map(|annotation| annotation.args)
}

/// What an accessor annotation's `AccessLevel` says, by the word alone — so
/// `AccessLevel.NONE` and a statically imported `NONE` read alike, and an
/// annotation naming neither is public.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Access {
    Public,
    Private,
    None,
}

fn access(args: &str) -> Access {
    // An annotation inside the arguments — `onMethod_ = @Json(use = NONE)` —
    // is the generated method's, and its words are not the access level.
    let mut own = String::with_capacity(args.len());
    let mut chars = args.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '@' {
            own.push(c);
            continue;
        }
        while chars
            .next_if(|c| c.is_alphanumeric() || matches!(c, '_' | '.' | ' '))
            .is_some()
        {}
        if chars.next_if_eq(&'(').is_some() {
            let mut depth = 1;
            while depth > 0 {
                match chars.next() {
                    Some('(') => depth += 1,
                    Some(')') => depth -= 1,
                    Some(_) => {}
                    None => break,
                }
            }
        }
    }
    let words = || own.split(|c: char| !(c.is_alphanumeric() || c == '_'));
    if words().any(|word| word == "NONE") {
        Access::None
    } else if words().any(|word| word == "PRIVATE") {
        Access::Private
    } else {
        Access::Public
    }
}

/// The names Lombok gives a field's accessors, the one it writes first and
/// the rest it also counts as already written — JLS-style `is` for a
/// primitive `boolean`, whose `isFoo` field is `isFoo()` and `setFoo`.
struct Names {
    getters: Vec<String>,
    setters: Vec<String>,
    withers: Vec<String>,
}

impl Names {
    fn of(field: &str, boolean: bool) -> Self {
        let stripped = field
            .strip_prefix("is")
            .filter(|rest| boolean && rest.chars().next().is_some_and(|c| !c.is_lowercase()));
        let mut suffixes = vec![capitalized(stripped.unwrap_or(field))];
        if stripped.is_some() {
            suffixes.push(capitalized(field));
        }
        let with = |prefix: &str| -> Vec<String> {
            suffixes
                .iter()
                .map(|suffix| format!("{prefix}{suffix}"))
                .collect()
        };
        let getters = if boolean {
            let mut names = with("is");
            names.extend(with("get"));
            names
        } else {
            with("get")
        };
        Self {
            getters,
            setters: with("set"),
            withers: with("with"),
        }
    }
}

fn capitalized(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// An empty span at the byte after `span`, where no identifier starts.
fn just_after(span: Span) -> Span {
    Span {
        start_byte: span.end_byte,
        end_byte: span.end_byte,
        range: javelin_document::Range::new(span.range.end, span.range.end),
    }
}

/// An empty span one byte into `span`, where no identifier starts — `None`
/// where that byte is its end.
fn just_inside(span: Span) -> Option<Span> {
    let start = span.range.start;
    (span.end_byte - span.start_byte > 1).then(|| {
        let at = javelin_document::Position::new(start.line, start.character + 1);
        Span {
            start_byte: span.start_byte + 1,
            end_byte: span.start_byte + 1,
            range: javelin_document::Range::new(at, at),
        }
    })
}

/// An empty span at the byte before `span`, where no identifier starts —
/// `None` where that byte would be on the line before.
fn just_before(span: Span) -> Option<Span> {
    let start = span.range.start;
    (start.character > 0 && span.start_byte > 0).then(|| {
        let at = javelin_document::Position::new(start.line, start.character - 1);
        Span {
            start_byte: span.start_byte - 1,
            end_byte: span.start_byte - 1,
            range: javelin_document::Range::new(at, at),
        }
    })
}

/// An empty span at the last byte of `span` — a type's closing brace, where
/// no name starts and no other generated member is sited.
fn just_before_end(span: Span) -> Option<Span> {
    let end = span.range.end;
    (end.character > 0 && span.end_byte > 0).then(|| {
        let at = javelin_document::Position::new(end.line, end.character - 1);
        Span {
            start_byte: span.end_byte - 1,
            end_byte: span.end_byte - 1,
            range: javelin_document::Range::new(at, at),
        }
    })
}

/// A span with no extent at `span`'s start: nothing is inside it.
fn empty_at(span: Span) -> Span {
    Span {
        start_byte: span.start_byte,
        end_byte: span.start_byte,
        range: javelin_document::Range::new(span.range.start, span.range.start),
    }
}

fn has(decl: &Decl, keyword: &str) -> bool {
    decl.modifiers.iter().any(|modifier| modifier == keyword)
}

/// One of a declaration's annotations that is Lombok's: its name in full,
/// `lombok.Getter`, and the text between its parentheses.
struct Annotation<'a> {
    name: String,
    args: &'a str,
}

fn annotations<'a>(decl: &'a Decl, imports: &[ImportDecl]) -> Vec<Annotation<'a>> {
    decl.modifiers
        .iter()
        .filter_map(|modifier| {
            let written = modifier.trim().strip_prefix('@')?;
            let (name, args) = match written.find('(') {
                Some(open) => (
                    &written[..open],
                    written[open + 1..]
                        .trim_end()
                        .strip_suffix(')')
                        .unwrap_or(""),
                ),
                None => (written, ""),
            };
            let name: String = name.chars().filter(|c| !c.is_whitespace()).collect();
            Some(Annotation {
                name: lombok_name(&name, imports)?,
                args,
            })
        })
        .collect()
}

/// `written` in full when it is Lombok's: already qualified, or its first
/// segment imported from `lombok` or `lombok.experimental`, singly or on
/// demand.
fn lombok_name(written: &str, imports: &[ImportDecl]) -> Option<String> {
    if written.starts_with("lombok.") {
        return Some(written.to_owned());
    }
    let first = written.split('.').next()?;
    let rest = &written[first.len()..];
    let single = imports.iter().find(|import| {
        !import.is_static && !import.is_on_demand && import.name.rsplit('.').next() == Some(first)
    });
    if let Some(import) = single {
        return import
            .name
            .starts_with("lombok.")
            .then(|| format!("{}{rest}", import.name));
    }
    let mut packages = imports
        .iter()
        .filter(|import| !import.is_static && import.is_on_demand)
        .filter(|import| matches!(import.name.as_str(), "lombok" | "lombok.experimental"));
    let package = packages.next()?;
    // Both on demand, and only one of them holds the name — which, this
    // module cannot say without the JAR, so the name is Lombok's only where
    // just one is imported.
    packages
        .next()
        .is_none()
        .then(|| format!("{}.{written}", package.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use javelin_document::{DocumentId, TextDocument};
    use javelin_parser::{FileSyntax, JavaParser};

    /// What Lombok generates for the first type `source` declares, as
    /// `name(parameters): declared` lines, the builder's after a `|`.
    fn parsed(source: &str) -> FileSyntax {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        FileSyntax::build(&document, &tree)
    }

    fn generated(source: &str) -> Vec<String> {
        let syntax = parsed(source);
        let owner = &syntax.declarations[0];
        let found = lombok_members(owner, &[], &syntax.imports);
        let line = |member: &WrittenMember| {
            format!(
                "{}{}{}({}): {}",
                if member.is_static { "static " } else { "" },
                if member.is_private { "private " } else { "" },
                member.name,
                member.parameters.join(", "),
                member.declared.as_deref().unwrap_or("")
            )
        };
        // `canEqual` rides along with every `@Data`; its own test says when.
        let mut lines: Vec<String> = found
            .members
            .iter()
            .filter(|member| member.name != "canEqual")
            .map(line)
            .collect();
        for builder in &found.builders {
            let mut header = format!("| {}", builder.simple);
            if !builder.type_parameters.is_empty() {
                header += &format!("<{}>", builder.type_parameters.join(", "));
            }
            if !builder.supertypes.is_empty() {
                header += &format!(" extends {}", builder.supertypes.join(", "));
            }
            lines.push(header);
            lines.extend(builder.members.iter().map(line));
        }
        lines
    }

    #[test]
    fn data_generates_a_getter_for_every_field_and_a_setter_for_every_mutable_one() {
        assert_eq!(
            generated(
                "import lombok.Data;\n@Data class A {\n    private String iban;\n    \
                 private final int id;\n    private boolean active;\n    \
                 private boolean isOpen;\n    private Boolean boxed;\n    \
                 private Boolean isReady;\n    \
                 private static int count;\n}\n"
            ),
            vec![
                "getIban(): String",
                "setIban(String): void",
                "getId(): int",
                "isActive(): boolean",
                "setActive(boolean): void",
                "isOpen(): boolean",
                "setOpen(boolean): void",
                "getBoxed(): Boolean",
                "setBoxed(Boolean): void",
                "getIsReady(): Boolean",
                "setIsReady(Boolean): void",
            ]
        );
    }

    /// `@EqualsAndHashCode`, and the `@Data` and `@Value` that imply it,
    /// write a protected `canEqual(Object)` the compiled class holds — unless
    /// the type writes one. Generated even where Lombok skips it, for a final
    /// class extending `Object`: a member too many is a missing answer.
    #[test]
    fn equals_and_hash_code_writes_can_equal() {
        let can_equal = |source: &str| {
            let document =
                TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
            let tree = JavaParser::new()
                .expect("grammar")
                .parse(&document)
                .expect("parses");
            let syntax = FileSyntax::build(&document, &tree);
            let owner = &syntax.declarations[0];
            lombok_members(owner, &[], &syntax.imports)
                .members
                .into_iter()
                .find(|member| member.name == "canEqual")
                .map(|member| {
                    // Its own site: the closing brace, where no name starts.
                    assert_eq!(&source[member.name_span.start_byte as usize..][..1], "}");
                    format!("{}({})", member.name, member.parameters.join(", "))
                })
        };
        for annotation in ["Data", "Value", "EqualsAndHashCode"] {
            assert_eq!(
                can_equal(&format!(
                    "import lombok.*;\n@{annotation} class A {{ int a; }}\n"
                )),
                Some("canEqual(Object)".to_owned()),
                "{annotation}"
            );
        }
        assert_eq!(
            can_equal("import lombok.*;\n@Getter class A { int a; }\n"),
            None
        );
        assert_eq!(
            can_equal(
                "import lombok.*;\n@Data class A { boolean canEqual(Object o) { return true; } }\n"
            ),
            None
        );
    }

    /// What a type's Lombok annotations say about the list the walk builds:
    /// `a` annotated, `u` unmodelled — Lombok may write a member nothing
    /// here generates — and `x` `@ExtensionMethod`.
    #[test]
    fn a_types_lombok_use_says_whether_its_generated_members_are_all_known() {
        let flags = |source: &str| {
            let document =
                TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
            let tree = JavaParser::new()
                .expect("grammar")
                .parse(&document)
                .expect("parses");
            let syntax = FileSyntax::build(&document, &tree);
            let found = lombok_members(&syntax.declarations[0], &[], &syntax.imports).lombok;
            [
                (found.annotated, 'a'),
                (found.unmodelled, 'u'),
                (found.extensions, 'x'),
            ]
            .into_iter()
            .filter_map(|(set, flag)| set.then_some(flag))
            .collect::<String>()
        };
        for (source, expected) in [
            ("class A { int a; }", ""),
            ("@Getter class A { int a; }", ""),
            ("@Override @Deprecated class A { int a; }", ""),
            ("import lombok.*;\n@Data class A { int a; }", "a"),
            ("import lombok.*;\n@Getter @Setter class A { int a; }", "a"),
            ("import lombok.*;\nclass A { @Getter @NonNull int a; }", "a"),
            (
                "import lombok.*;\n@ToString @EqualsAndHashCode class A { int a; }",
                "a",
            ),
            ("import lombok.extern.slf4j.Slf4j;\n@Slf4j class A {}", "a"),
            (
                "import lombok.*;\n@NoArgsConstructor @AllArgsConstructor class A { int a; }",
                "a",
            ),
            (
                "import lombok.*;\n@Builder(toBuilder = true) class A { int a; }",
                "a",
            ),
            ("import lombok.*;\n@Value class A { int a; }", "a"),
            (
                "import lombok.*;\nclass A { @SneakyThrows void a() {} }",
                "a",
            ),
            ("import lombok.*;\n@Getter enum A { X; int a; }", "a"),
            (
                "import lombok.experimental.SuperBuilder;\n@SuperBuilder class A {}",
                "a",
            ),
            (
                "import lombok.*;\n@Builder class A { @Singular java.util.List<String> names; }",
                "a",
            ),
            (
                "import lombok.*;\n@Builder class A { @Singular java.util.List<String> series; }",
                "au",
            ),
            (
                "import lombok.*;\nclass A { @Singular java.util.List<String> names; }",
                "au",
            ),
            (
                "import lombok.experimental.SuperBuilder;\n@SuperBuilder class A<T> {}",
                "au",
            ),
            (
                "import lombok.experimental.Delegate;\nclass A { @Delegate Runnable r; }",
                "au",
            ),
            (
                "import lombok.experimental.Accessors;\n@Accessors(fluent = true) class A {}",
                "au",
            ),
            (
                "import lombok.*;\n@Builder(builderMethodName = \"make\") class A {}",
                "au",
            ),
            (
                "import lombok.*;\n@AllArgsConstructor(staticName = \"of\") class A {}",
                "au",
            ),
            (
                "import lombok.*;\n@Data(staticConstructor = \"of\") class A {}",
                "au",
            ),
            ("import lombok.*;\n@With class A<T> { final int a; }", "au"),
            ("import lombok.*;\n@Builder class A<T> { int a; }", "a"),
            (
                "import lombok.*;\nabstract @Builder class A { int a; }",
                "au",
            ),
            (
                "import lombok.*;\n@Builder class A { int a; static class ABuilder {} }",
                "au",
            ),
            (
                "import lombok.*;\nclass A { @Builder static A of(int a) { return null; } }",
                "au",
            ),
            (
                "import lombok.experimental.Tolerate;\nclass A { @Tolerate void setA(int a) {} }",
                "au",
            ),
            ("import lombok.*;\n@Builder record A(int a) {}", "au"),
            ("import lombok.*;\n@Getter record A(int a) {}", "au"),
            ("import lombok.*;\ninterface A { @Getter int X = 0; }", "au"),
            ("import lombok.Foo;\n@Foo class A {}", "au"),
            ("@lombok.experimental.UtilityClass class A {}", "au"),
            (
                "import lombok.experimental.Accessors;\nimport lombok.Getter;\n\
                 @Accessors(chain = true) @Getter class A { int a; }",
                "au",
            ),
            (
                "import lombok.*;\nimport lombok.experimental.*;\n@Getter class A { int a; }",
                "au",
            ),
            (
                "import lombok.experimental.ExtensionMethod;\n\
                 @ExtensionMethod(Util.class) class A {}",
                "aux",
            ),
            (
                "import lombok.*;\nimport lombok.experimental.*;\n\
                 @ExtensionMethod(Util.class) class A {}",
                "aux",
            ),
        ] {
            assert_eq!(flags(source), expected, "{source}");
        }
    }

    /// `@Accessors` on a type around this one renames what this one's
    /// `@Getter` writes, so nothing generated here is all of it.
    #[test]
    fn accessors_around_a_type_leave_its_lombok_use_unmodelled() {
        let source = "import lombok.Getter;\nimport lombok.experimental.Accessors;\n\
                      @Accessors(fluent = true) class O { @Getter static class I { int a; } }\n";
        let document = TextDocument::new(DocumentId::new("file:///O.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let outer = &syntax.declarations[0];
        let inner = outer
            .children
            .iter()
            .find(|child| child.name == "I")
            .expect("nested");
        assert!(
            lombok_members(inner, &[outer], &syntax.imports)
                .lombok
                .unmodelled
        );
    }

    /// A setter's parameter is named for its field, as Lombok writes it.
    #[test]
    fn a_setters_parameter_is_named_for_its_field() {
        let source = "import lombok.Setter;\nclass A { @Setter int count; }\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let found = lombok_members(&syntax.declarations[0], &[], &syntax.imports);
        assert_eq!(found.members[0].parameter_names, vec!["count"]);
    }

    #[test]
    fn value_makes_every_field_final_unless_it_is_non_final() {
        assert_eq!(
            generated(
                "import lombok.*;\nimport lombok.experimental.NonFinal;\n\
                 @Value class A { String a; @NonFinal int b; }\n"
            ),
            vec!["getA(): String", "getB(): int"]
        );
    }

    /// A field's own annotation decides over its type's; `NONE` writes
    /// nothing, `PRIVATE` a private accessor, and a static field is reached
    /// only by its own annotation.
    #[test]
    fn a_fields_annotation_and_its_access_level_win() {
        assert_eq!(
            generated(
                "import lombok.Getter;\nimport lombok.Setter;\nimport lombok.AccessLevel;\n\
                 @Getter class A {\n    @Getter(AccessLevel.NONE) int hidden;\n    \
                 @Setter(value = AccessLevel.PRIVATE) int mine;\n    \
                 @Getter static int shared;\n    static int other;\n    \
                 @Getter(onMethod_ = @Json(use = Id.NONE)) int shown;\n}\n"
            ),
            vec![
                "getMine(): int",
                "private setMine(int): void",
                "static getShared(): int",
                "getShown(): int"
            ]
        );
    }

    /// Lombok's rule: no accessor where a method of any of its names, in any
    /// case, takes as many parameters — or is variadic.
    #[test]
    fn a_method_the_type_writes_suppresses_the_accessor_it_would_be() {
        assert_eq!(
            generated(
                "@lombok.Data class A {\n    int a; int b; boolean c; int d;\n    \
                 int GETA() { return 0; }\n    void setB(String b) {}\n    \
                 boolean getC() { return true; }\n    int getB(int x) { return 0; }\n    \
                 int getD(String... x) { return 0; }\n}\n"
            ),
            vec![
                "setA(int): void",
                "getB(): int",
                "setC(boolean): void",
                "setD(int): void"
            ]
        );
    }

    /// Not Lombok's `@Getter`, or not one this module models: nothing.
    #[test]
    fn an_annotation_that_is_not_lombok_or_renames_generates_nothing() {
        for source in [
            "@Getter class A { int a; }\n",
            "import other.Getter;\nimport lombok.*;\n@Getter class A { int a; }\n",
            "import lombok.*;\nimport lombok.experimental.*;\n@Getter class A { int a; }\n",
            "import lombok.*;\nimport lombok.experimental.Accessors;\n\
             @Getter @Accessors(fluent = true) class A { int a; }\n",
            "import lombok.Getter;\n@Getter interface A { int a = 0; }\n",
        ] {
            assert_eq!(generated(source), Vec::<String>::new(), "{source}");
        }
    }

    #[test]
    fn a_builder_sets_every_field_a_constructor_would_take() {
        assert_eq!(
            generated(
                "import lombok.Builder;\n@Builder(toBuilder = true) class Car {\n    \
                 private String name;\n    private final int wheels;\n    \
                 private final int doors = 4;\n    @Builder.Default private final int seats = 5;\n    \
                 private static int made;\n}\n"
            ),
            vec![
                "static builder(): CarBuilder",
                "toBuilder(): CarBuilder",
                "| CarBuilder",
                "name(String): CarBuilder",
                "wheels(int): CarBuilder",
                "seats(int): CarBuilder",
                "build(): Car",
            ]
        );
    }

    /// Lombok's builder of a generic type is generic in the type's own
    /// variables, and `builder()` takes them as its own: `static <K, V>
    /// PairBuilder<K, V> builder()`.
    #[test]
    fn a_generic_types_builder_carries_its_variables() {
        let source = "import lombok.Builder;\n@Builder(toBuilder = true) \
                      class Pair<K, V extends Comparable<V>> {\n    K key;\n    \
                      java.util.List<V> values;\n}\n";
        assert_eq!(
            generated(source),
            vec![
                "static builder(): PairBuilder<K, V>",
                "toBuilder(): PairBuilder<K, V>",
                "| PairBuilder<K, V>",
                "key(K): PairBuilder<K, V>",
                "values(java.util.List<V>): PairBuilder<K, V>",
                "build(): Pair<K, V>",
            ]
        );
        let syntax = parsed(source);
        let found = lombok_members(&syntax.declarations[0], &[], &syntax.imports);
        let scope = |name: &str, members: &[WrittenMember]| {
            members
                .iter()
                .find(|member| member.name == name)
                .map(|member| member.type_parameters.join(" "))
        };
        assert_eq!(
            scope("builder", &found.members).as_deref(),
            Some("K V K V"),
            "the owner's, then its own of the same names"
        );
        assert_eq!(scope("toBuilder", &found.members).as_deref(), Some("K V"));
        assert_eq!(
            scope("build", &found.builders[0].members).as_deref(),
            Some("K V")
        );
    }

    #[test]
    fn a_builder_outside_the_modelled_rules_is_not_generated() {
        for source in [
            "import lombok.Builder;\n@Builder(builderMethodName = \"make\") class A { int a; }\n",
            "import lombok.*;\n@Builder class A { @Singular java.util.List<String> series; }\n",
            "import lombok.*;\n@Builder class A { @Singular java.util.List names; }\n",
            "import lombok.*;\n@Builder class A { @Singular java.util.List<?> names; }\n",
            "import lombok.*;\n@Builder class A { @Singular ImmutableList<String> names; }\n",
            "import lombok.*;\n@Builder class A { @Singular(\"x\") String names; }\n",
            "import lombok.*;\n@Builder class A { @Singular @Builder.Default java.util.List<String> names = null; }\n",
            "import lombok.Builder;\n@Builder class A { int a; static class ABuilder {} }\n",
        ] {
            assert_eq!(generated(source), Vec::<String>::new(), "{source}");
        }
    }

    #[test]
    fn field_defaults_making_fields_final_leaves_them_no_setter() {
        assert_eq!(
            generated(
                "import lombok.*;\nimport lombok.experimental.FieldDefaults;\n\
                 @Data @FieldDefaults(makeFinal = true, level = AccessLevel.PRIVATE)\n\
                 class A { String a; }\n"
            ),
            vec!["getA(): String"]
        );
    }

    /// Lombok strips `is` from a boolean whose next character is not
    /// lower-case, an underscore or a digit included.
    #[test]
    fn an_is_prefix_is_stripped_before_anything_but_a_lower_case_letter() {
        assert_eq!(
            generated("import lombok.Data;\n@Data class A { boolean is_on; boolean island; }\n"),
            vec![
                "is_on(): boolean",
                "set_on(boolean): void",
                "isIsland(): boolean",
                "setIsland(boolean): void",
            ]
        );
    }

    /// On an enum only `@Getter` is Lombok's to write.
    #[test]
    fn an_enum_takes_getters_and_neither_setters_nor_withers() {
        assert_eq!(
            generated("import lombok.*;\n@Getter @Setter @With enum E { A; private int code; }\n"),
            vec!["getCode(): int"]
        );
    }

    /// A field `@Value` or `@FieldDefaults` made final, with an initialiser,
    /// is no builder's to set and no `@With`'s to copy.
    #[test]
    fn a_field_made_final_with_an_initialiser_is_neither_built_nor_withered() {
        assert_eq!(
            generated(
                "import lombok.*;\n@Value @Builder @With class C { int retries = 3; String name; }\n"
            ),
            vec![
                "getRetries(): int",
                "getName(): String",
                "withName(String): C",
                "static builder(): CBuilder",
                "| CBuilder",
                "name(String): CBuilder",
                "build(): C",
            ]
        );
    }

    /// `builder()` the type writes, at any arity, is the one there is.
    #[test]
    fn a_builder_method_the_type_writes_is_not_generated_again() {
        assert_eq!(
            generated(
                "import lombok.Builder;\n@Builder(toBuilder = true) class C {\n    String name;\n    \
                 static CBuilder builder(String name) { return null; }\n}\n"
            ),
            vec![
                "toBuilder(): CBuilder",
                "| CBuilder",
                "name(String): CBuilder",
                "build(): C",
            ]
        );
    }

    /// `@Accessors` on an enclosing type renames the nested type's
    /// accessors, so nothing is generated for it.
    #[test]
    fn accessors_on_an_enclosing_type_generate_nothing() {
        let source = "import lombok.*;\nimport lombok.experimental.Accessors;\n\
                      @Accessors(fluent = true) class Api {\n    @Getter static class Dto { String id; }\n}\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let api = &syntax.declarations[0];
        let dto = api
            .children
            .iter()
            .find(|child| child.name == "Dto")
            .expect("Dto");
        assert!(
            lombok_members(dto, &[api], &syntax.imports)
                .members
                .is_empty()
        );
        assert_eq!(
            lombok_members(dto, &[], &syntax.imports).members.len(),
            1,
            "the premise"
        );
    }

    /// Lombok's own `TestSingulars`, case for case.
    #[test]
    fn a_plural_is_singularized_as_lombok_does_it() {
        for (plural, singular) in [
            ("axes", None),
            ("adjectives", Some("adjective")),
            ("buses", Some("bus")),
            ("octopodes", Some("octopus")),
            ("octopi", None),
            ("elves", Some("elf")),
            ("jacks", Some("jack")),
            ("colloquies", Some("colloquy")),
            ("series", None),
            ("men", Some("man")),
            ("highwaymen", None),
            ("caveMen", Some("caveMan")),
            ("jackss", None),
            ("virus", None),
            ("quizzes", Some("quiz")),
            ("databases", Some("database")),
            ("dataBases", Some("dataBase")),
            ("Queries", Some("Query")),
            ("Movies", Some("Movie")),
            ("cafes", Some("cafe")),
            ("caves", None),
            ("leaves", Some("leaf")),
            ("autosaves", Some("autosave")),
            ("größen", None),
        ] {
            assert_eq!(auto_singular(plural).as_deref(), singular, "{plural}");
        }
    }

    /// `@Singular` swaps a collection field's setter for an adder of one, an
    /// adder of many and a clear, by Lombok's recipe for the written type.
    #[test]
    fn singular_writes_an_adder_of_one_an_adder_of_many_and_a_clear() {
        assert_eq!(
            generated(
                "import lombok.*;\nimport java.util.*;\n@Builder class A {\n    \
                 @Singular List<String> names;\n    \
                 @Singular(\"filter\") java.util.Set<Long> filters;\n    \
                 @Singular(value = \"label\", ignoreNullCollections = true) \
                 Map<String, List<Integer>> labels;\n    int size;\n}\n"
            ),
            vec![
                "static builder(): ABuilder",
                "| ABuilder",
                "name(String): ABuilder",
                "names(java.util.Collection<? extends String>): ABuilder",
                "clearNames(): ABuilder",
                "filter(Long): ABuilder",
                "filters(java.util.Collection<? extends Long>): ABuilder",
                "clearFilters(): ABuilder",
                "label(String, List<Integer>): ABuilder",
                "labels(java.util.Map<? extends String, ? extends List<Integer>>): ABuilder",
                "clearLabels(): ABuilder",
                "size(int): ABuilder",
                "build(): A",
            ]
        );
    }

    /// A map's singular adder names its two parameters for the singular, and
    /// every adder is sited at the field, as its setter would be.
    #[test]
    fn singulars_adders_are_sited_at_the_field_and_name_their_parameters() {
        let source = "import lombok.*;\n@Builder class A { @Singular java.util.Map<String, Integer> labels; }\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let owner = &syntax.declarations[0];
        let field = &owner.children[0];
        let found = lombok_members(owner, &[], &syntax.imports);
        let inside = &found.builders[0].members;
        assert_eq!(inside[0].parameter_names, ["labelKey", "labelValue"]);
        assert_eq!(inside[1].parameter_names, ["labels"]);
        assert!(
            inside[..3]
                .iter()
                .all(|member| member.name_span == field.name_span)
        );
    }

    /// `@SuperBuilder`'s builder extends the superclass's, its setters return
    /// the builder's own `B`, and a concrete type's implementation binds `B`
    /// to itself and `C` to the type.
    #[test]
    fn super_builder_writes_a_builder_that_extends_the_superclasss() {
        assert_eq!(
            generated(
                "import lombok.experimental.SuperBuilder;\n\
                 @SuperBuilder(toBuilder = true) class Child extends Parent {\n    \
                 private String name;\n    private final int fixed = 1;\n}\n"
            ),
            vec![
                "static builder(): ChildBuilderImpl",
                "toBuilder(): ChildBuilderImpl",
                "| ChildBuilder<C, B> extends Parent.ParentBuilder<C, B>",
                "name(String): B",
                "self(): B",
                "build(): C",
                "$fillValuesFrom(C): B",
                "| ChildBuilderImpl extends ChildBuilder<Child, ChildBuilderImpl>",
                "self(): ChildBuilderImpl",
                "build(): Child",
            ]
        );
        assert_eq!(
            generated(
                "import lombok.experimental.SuperBuilder;\n\
                 @SuperBuilder abstract class Base extends com.acme.Root { int id; }\n"
            ),
            vec![
                "| BaseBuilder<C, B> extends com.acme.Root.RootBuilder<C, B>",
                "id(int): B",
                "self(): B",
                "build(): C",
            ]
        );
    }

    /// `C` and `B` move aside, as Lombok moves them, for a name the type, a
    /// field's plain type or a supertype's first segment already takes.
    #[test]
    fn super_builders_variables_do_not_take_a_name_the_type_uses() {
        assert_eq!(
            generated(
                "import lombok.experimental.SuperBuilder;\n\
                 @SuperBuilder class C implements B.Marker { B b; }\n"
            ),
            vec![
                "static builder(): CBuilderImpl",
                "| CBuilder<C2, B2>",
                "b(B): B2",
                "self(): B2",
                "build(): C2",
                "| CBuilderImpl extends CBuilder<C, CBuilderImpl>",
                "self(): CBuilderImpl",
                "build(): C",
            ]
        );
    }

    #[test]
    fn a_super_builder_outside_the_modelled_rules_is_not_generated() {
        for source in [
            "import lombok.experimental.SuperBuilder;\n@SuperBuilder class A<T> { T a; }\n",
            "import lombok.experimental.SuperBuilder;\n@SuperBuilder class A extends B<String> {}\n",
            "import lombok.experimental.SuperBuilder;\n@SuperBuilder(builderMethodName = \"make\") class A {}\n",
            "import lombok.experimental.SuperBuilder;\n@SuperBuilder class A { static class ABuilder {} }\n",
            "import lombok.experimental.SuperBuilder;\n@SuperBuilder enum A { X }\n",
            "import lombok.*;\nimport lombok.experimental.SuperBuilder;\n@Builder @SuperBuilder class A { int a; }\n",
            "import lombok.*;\nimport lombok.experimental.SuperBuilder;\n@SuperBuilder class A { @Singular java.util.List<String> series; }\n",
        ] {
            assert_eq!(generated(source), Vec::<String>::new(), "{source}");
        }
    }

    /// Lombok refuses `@Builder` on an inner class too.
    #[test]
    fn a_builder_on_an_inner_class_is_not_generated() {
        let built = |source: &str| {
            let syntax = parsed(source);
            let outer = &syntax.declarations[0];
            let inner = outer
                .children
                .iter()
                .find(|child| child.name == "In")
                .expect("In");
            let found = lombok_members(inner, &[outer], &syntax.imports);
            (found.builders.len(), found.lombok.unmodelled)
        };
        let import = "import lombok.Builder;\n";
        assert_eq!(
            built(&format!("{import}class O {{ @Builder class In {{}} }}")),
            (0, true)
        );
        assert_eq!(
            built(&format!(
                "{import}class O {{ @Builder static class In {{}} }}"
            )),
            (1, false)
        );
        assert_eq!(
            built(&format!("{import}interface O {{ @Builder class In {{}} }}")),
            (1, false)
        );
    }

    /// Lombok refuses `@SuperBuilder` on an inner class; a nested one must be
    /// static, or inside an interface.
    #[test]
    fn a_super_builder_on_an_inner_class_is_not_generated() {
        let built = |source: &str| {
            let document =
                TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
            let tree = JavaParser::new()
                .expect("grammar")
                .parse(&document)
                .expect("parses");
            let syntax = FileSyntax::build(&document, &tree);
            let outer = &syntax.declarations[0];
            let inner = outer
                .children
                .iter()
                .find(|child| child.name == "In")
                .expect("In");
            let found = lombok_members(inner, &[outer], &syntax.imports);
            (found.builders.len(), found.lombok.unmodelled)
        };
        let import = "import lombok.experimental.SuperBuilder;\n";
        assert_eq!(
            built(&format!(
                "{import}class O {{ @SuperBuilder class In {{}} }}"
            )),
            (0, true)
        );
        assert_eq!(
            built(&format!(
                "{import}class O {{ @SuperBuilder static class In {{}} }}"
            )),
            (2, false)
        );
        assert_eq!(
            built(&format!(
                "{import}interface O {{ @SuperBuilder class In {{}} }}"
            )),
            (2, false)
        );
    }

    /// `builder()`, `toBuilder()` and `build()` are three methods at three
    /// sites, none of them a name.
    #[test]
    fn the_builders_three_methods_have_three_sites() {
        let source = "import lombok.Builder;\n@Builder(toBuilder = true) class Car { int a; }\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let owner = &syntax.declarations[0];
        let found = lombok_members(owner, &[], &syntax.imports);
        let inside = &found.builders.first().expect("a builder").members;
        let starts = [
            found.members[0].name_span.start_byte,
            found.members[1].name_span.start_byte,
            inside.last().expect("build").name_span.start_byte,
        ];
        assert_eq!(
            starts,
            [
                owner.name_span.start_byte,
                owner.name_span.start_byte - 1,
                owner.name_span.end_byte
            ]
        );
    }

    /// `@SuperBuilder`'s `self()` is not a site `build()` or a name shares.
    #[test]
    fn super_builders_self_and_build_have_two_sites() {
        let source = "import lombok.experimental.SuperBuilder;\n@SuperBuilder class Car {}\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let owner = &syntax.declarations[0];
        let found = lombok_members(owner, &[], &syntax.imports);
        for builder in &found.builders {
            let site = |name: &str| {
                builder
                    .members
                    .iter()
                    .find(|member| member.name == name)
                    .expect(name)
                    .name_span
                    .start_byte
            };
            assert_eq!(site("self"), owner.name_span.start_byte + 1);
            assert_eq!(site("build"), owner.name_span.end_byte);
        }
    }

    /// Lombok writes the setters into a builder class written by hand, so a
    /// class nested in a type that builds is not known whole; one in a type
    /// that does not build is.
    #[test]
    fn a_class_written_inside_a_type_that_builds_is_lomboks_to_add_to() {
        let unmodelled = |source: &str| {
            let document =
                TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
            let tree = JavaParser::new()
                .expect("grammar")
                .parse(&document)
                .expect("parses");
            let syntax = FileSyntax::build(&document, &tree);
            let outer = &syntax.declarations[0];
            let inner = outer
                .children
                .iter()
                .find(|child| child.kind == DeclKind::Class)
                .expect("a nested class");
            lombok_members(inner, &[outer], &syntax.imports)
                .lombok
                .unmodelled
        };
        for (source, expected) in [
            (
                "import lombok.Builder;\n@Builder class Plain { String name;\n\
                 static class PlainBuilder { PlainBuilder upper() { return this; } } }",
                true,
            ),
            (
                "import lombok.experimental.SuperBuilder;\n@SuperBuilder class Parent {\n\
                 static abstract class ParentBuilder<C, B> {} }",
                true,
            ),
            (
                "import lombok.Builder;\nclass Plain { String name;\n\
                 @Builder(builderClassName = \"Maker\") Plain(String name) {}\n\
                 static class Maker {} }",
                true,
            ),
            (
                "import lombok.Getter;\n@Getter class Plain { String name;\n\
                 static class PlainBuilder {} }",
                false,
            ),
        ] {
            assert_eq!(unmodelled(source), expected, "{source}");
        }
    }

    #[test]
    fn with_returns_a_copy_of_the_type() {
        assert_eq!(
            generated("import lombok.With;\nclass A { @With final int a; final boolean isOn; }\n"),
            vec!["withA(int): A"]
        );
        assert_eq!(
            generated("import lombok.With;\n@With class A { final boolean isOn; }\n"),
            vec!["withOn(boolean): A"]
        );
    }

    /// A generated member is navigated as what the author wrote: the field's
    /// name, and an empty declaration nothing is inside.
    #[test]
    fn a_generated_member_is_sited_at_its_field() {
        let source = "import lombok.Getter;\nclass A { @Getter int count; }\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let owner = &syntax.declarations[0];
        let getter = &lombok_members(owner, &[], &syntax.imports).members[0];
        let field = &owner.children[0];
        assert_eq!(getter.name_span, field.name_span);
        assert_eq!(getter.span.start_byte, getter.span.end_byte);
    }

    /// A log annotation writes a `private static final` logger called `log`
    /// of its framework's type — on a class or an enum, not where the type
    /// writes a field of the name — sited where the type's annotations start.
    #[test]
    fn a_log_annotation_writes_its_logger() {
        for (annotation, logger) in [
            ("extern.slf4j.Slf4j", "org.slf4j.Logger"),
            ("extern.slf4j.XSlf4j", "org.slf4j.ext.XLogger"),
            ("extern.java.Log", "java.util.logging.Logger"),
            ("extern.log4j.Log4j", "org.apache.log4j.Logger"),
            ("extern.log4j.Log4j2", "org.apache.logging.log4j.Logger"),
            (
                "extern.apachecommons.CommonsLog",
                "org.apache.commons.logging.Log",
            ),
            ("extern.jbosslog.JBossLog", "org.jboss.logging.Logger"),
            (
                "extern.flogger.Flogger",
                "com.google.common.flogger.FluentLogger",
            ),
        ] {
            let simple = annotation.rsplit('.').next().unwrap();
            assert_eq!(
                generated(&format!(
                    "import lombok.{annotation};\n@{simple}(topic = \"x\") class A {{}}\n"
                )),
                vec![format!("static private log(): {logger}")],
                "{annotation}"
            );
        }
        assert_eq!(
            generated("@lombok.extern.slf4j.Slf4j enum A { ONE }\n"),
            vec!["static private log(): org.slf4j.Logger"]
        );
        for source in [
            // Lombok warns and writes nothing over a field of the name.
            "import lombok.extern.slf4j.Slf4j;\n@Slf4j class A { int log; }\n",
            // Its declaration is configured, not known.
            "import lombok.CustomLog;\n@CustomLog class A {}\n",
            "import lombok.extern.slf4j.Slf4j;\n@Slf4j interface A {}\n",
            "@Slf4j class A {}\n",
            "import lombok.extern.slf4j.Slf4j;\n@Slf4j(access = AccessLevel.NONE) class A {}\n",
        ] {
            assert_eq!(generated(source), Vec::<String>::new(), "{source}");
        }
        let source = "import lombok.extern.slf4j.Slf4j;\n@Slf4j(access = PUBLIC) class A {}\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let access = lombok_members(&syntax.declarations[0], &[], &syntax.imports);
        assert!(access.members.is_empty() && access.lombok.unmodelled);

        let source = "import lombok.extern.slf4j.Slf4j;\n@Service @Slf4j class A {}\n";
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 0, source.to_owned());
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let owner = &syntax.declarations[0];
        let log = &lombok_members(owner, &[], &syntax.imports).members[0];
        assert_eq!(
            log.name_span.start_byte,
            source.find("@Service").unwrap() as u32
        );
        assert_eq!(log.name_span.start_byte, log.name_span.end_byte);
        assert_eq!(log.span, log.name_span);
    }
}
