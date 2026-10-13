//! What a Java type is, in the two forms this crate needs it.
//!
//! [`TypeRef`] is a type as the author wrote it: structured, but not looked up
//! against anything. [`Type`] is that reference after resolution — against the
//! file the name was written in first, and then against whatever
//! [`crate::Externals`] can see.
//! A name that file declares becomes [`Type::Declared`], which says *which*
//! file as well as where in it; a name only the classpath knows becomes
//! [`Type::External`]; and a name neither can settle
//! lands in [`Type::Unknown`], which carries the source text for when a hover
//! renders a type. Nothing renders one today: hover shows the declaration's
//! source text, not a `Type`.

use crate::files::DeclSite;

/// How deep nested type arguments are followed. Beyond this the argument list
/// is dropped rather than recursed into: a buffer mid-keystroke can nest
/// angle brackets as far as the typist has got.
const MAX_ARGUMENT_DEPTH: u32 = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Primitive {
    Boolean,
    Byte,
    Short,
    Int,
    Long,
    Char,
    Float,
    Double,
}

impl Primitive {
    /// The keyword the author would have written.
    ///
    /// Needed because a primitive can sit inside a type whose container does
    /// not resolve — `List<int[]>` is legal Java — and the text an
    /// unresolvable type carries is what a hover would render, for when one
    /// renders a type.
    pub fn keyword(self) -> &'static str {
        match self {
            Primitive::Boolean => "boolean",
            Primitive::Byte => "byte",
            Primitive::Short => "short",
            Primitive::Int => "int",
            Primitive::Long => "long",
            Primitive::Char => "char",
            Primitive::Float => "float",
            Primitive::Double => "double",
        }
    }
}

/// A type as written, parsed into shape but resolved against nothing.
///
/// `pub(crate)`: `parse_type_ref` is the only constructor, and its depth cap
/// only holds if nothing outside this crate can build one directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TypeRef {
    Primitive(Primitive),
    Void,
    Array(Box<TypeRef>),
    /// `List`, `Map.Entry<K, V>`, `T`. The name is as written, dots included.
    Named {
        name: String,
        arguments: Vec<TypeRef>,
    },
    /// `?`, `? extends Number`, and anything unparseable. Nothing downstream
    /// can say more about either than "unknown".
    Wildcard,
}

/// A type, resolved as far as one file and the classpath together allow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Type {
    Primitive(Primitive),
    Void,
    /// The type of `null`, and of a member lookup that found nothing.
    Null,
    Array(Box<Type>),
    /// A type a file declares. `decl.span` is the declaration's name span,
    /// which `FileSyntax::declaration_at` takes, and `decl.file` says whose
    /// `FileSyntax` that is.
    Declared {
        name: String,
        decl: DeclSite,
        arguments: Vec<Type>,
    },
    /// A type parameter in scope, e.g. the `T` of `class Box<T>`.
    Variable(String),
    /// A type no file this crate can read declares: the JDK today, the
    /// workspace and its dependencies later. `fqn` is source-form and fully qualified; `name`
    /// is what the author wrote, which is the text a hover would render.
    External {
        name: String,
        fqn: String,
        arguments: Vec<Type>,
    },
    /// A name nothing available settles: an import with no classpath behind it,
    /// a sibling source file, a buffer mid-keystroke. Carries the source text
    /// for when a hover renders a type.
    Unknown(String),
}

impl Type {
    /// Whether resolution reached a type at all. Not an error test — an unknown
    /// receiver is the ordinary answer, not a failure.
    ///
    /// `External` is known: it names exactly one classpath type, and
    /// [`crate::members_of`] reads members off one — from the facts the world
    /// holds rather than from a file.
    pub fn is_known(&self) -> bool {
        !matches!(self, Type::Unknown(_))
    }
}

/// Parse a type as the author wrote it.
///
/// Never fails. The input comes from `Decl::declared_type`, which is source
/// text from a buffer that is invalid Java most of the time it is read, so
/// anything unparseable becomes [`TypeRef::Wildcard`] — the same answer as an
/// explicit `?`, because neither can be resolved any further.
///
/// `pub(crate)`: the only constructor of `TypeRef`, so its depth cap is the
/// only depth cap that needs to exist.
pub(crate) fn parse_type_ref(text: &str) -> TypeRef {
    parse_at_depth(text.trim(), 0)
}

fn parse_at_depth(text: &str, depth: u32) -> TypeRef {
    let text = strip_annotations(text.trim());
    if text.is_empty() || text.starts_with('?') {
        return TypeRef::Wildcard;
    }
    if let Some(element) = text.strip_suffix("[]") {
        // Capped and incremented like the argument branch below: `int` followed
        // by a million `[]` is one recursion per dimension otherwise, and this
        // runs on the request loop. Java allows 255 dimensions at most, so
        // giving up past the cap costs nothing a real file would notice.
        if depth >= MAX_ARGUMENT_DEPTH {
            return TypeRef::Wildcard;
        }
        return TypeRef::Array(Box::new(parse_at_depth(element, depth + 1)));
    }
    let open = text.find('<');
    // `Outer<A>.Inner`: the arguments close before the name ends, so this is
    // a member type of a parameterized outer — not `Outer`, whatever the text
    // before the `<` says.
    if let Some(open) = open
        && closing(text, open).is_some_and(|close| close + 1 != text.len())
    {
        return TypeRef::Wildcard;
    }
    let (name, arguments) = match open {
        Some(open) if text.ends_with('>') && depth < MAX_ARGUMENT_DEPTH => {
            let inner = &text[open + 1..text.len() - 1];
            (
                &text[..open],
                split_arguments(inner)
                    .into_iter()
                    .map(|argument| parse_at_depth(argument, depth + 1))
                    .collect(),
            )
        }
        // An unterminated `<` is a buffer mid-keystroke: the name is still good.
        Some(open) => (&text[..open], Vec::new()),
        None => (text, Vec::new()),
    };
    let name = name.trim();
    match name {
        "boolean" => TypeRef::Primitive(Primitive::Boolean),
        "byte" => TypeRef::Primitive(Primitive::Byte),
        "short" => TypeRef::Primitive(Primitive::Short),
        "int" => TypeRef::Primitive(Primitive::Int),
        "long" => TypeRef::Primitive(Primitive::Long),
        "char" => TypeRef::Primitive(Primitive::Char),
        "float" => TypeRef::Primitive(Primitive::Float),
        "double" => TypeRef::Primitive(Primitive::Double),
        "void" => TypeRef::Void,
        "" => TypeRef::Wildcard,
        _ => TypeRef::Named {
            name: name.to_owned(),
            arguments,
        },
    }
}

/// The byte offset of the `>` closing the `<` at `open`, if one does.
fn closing(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (at, character) in text[open..].char_indices() {
        match character {
            '<' => depth += 1,
            '>' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + at);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split `String, Box<T, U>` on the commas that are not inside brackets.
///
/// `pub(crate)` because a type parameter list has the same problem: the comma
/// in `<K, V extends Map<K, V>>` that separates parameters is not the one
/// inside the bound.
pub(crate) fn split_arguments(inner: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut depth = 0i32;
    let mut start = 0usize;
    for (index, character) in inner.char_indices() {
        match character {
            '<' => depth += 1,
            '>' => depth -= 1,
            ',' if depth == 0 => {
                out.push(&inner[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    out.push(&inner[start..]);
    out.into_iter()
        .filter(|part| !part.trim().is_empty())
        .collect()
}

/// Drop leading type annotations: `@NonNull String` is a `String`.
pub(crate) fn strip_annotations(text: &str) -> &str {
    let mut rest = text;
    while let Some(after) = rest.strip_prefix('@') {
        let end = after
            .find(char::is_whitespace)
            .map_or(after.len(), |index| index);
        rest = after[end..].trim_start();
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_primitive_parses_as_itself() {
        assert_eq!(parse_type_ref("int"), TypeRef::Primitive(Primitive::Int));
        assert_eq!(
            parse_type_ref("boolean"),
            TypeRef::Primitive(Primitive::Boolean)
        );
        assert_eq!(parse_type_ref("void"), TypeRef::Void);
    }

    #[test]
    fn an_array_parses_as_its_element_type() {
        assert_eq!(
            parse_type_ref("int[]"),
            TypeRef::Array(Box::new(TypeRef::Primitive(Primitive::Int)))
        );
        assert_eq!(
            parse_type_ref("String[][]"),
            TypeRef::Array(Box::new(TypeRef::Array(Box::new(TypeRef::Named {
                name: "String".into(),
                arguments: Vec::new(),
            }))))
        );
    }

    #[test]
    fn a_generic_type_keeps_its_arguments() {
        assert_eq!(
            parse_type_ref("Map<String, Box<T>>"),
            TypeRef::Named {
                name: "Map".into(),
                arguments: vec![
                    TypeRef::Named {
                        name: "String".into(),
                        arguments: Vec::new()
                    },
                    TypeRef::Named {
                        name: "Box".into(),
                        arguments: vec![TypeRef::Named {
                            name: "T".into(),
                            arguments: Vec::new()
                        }],
                    },
                ],
            }
        );
    }

    #[test]
    fn a_qualified_name_keeps_only_its_leftmost_segment_resolvable() {
        assert_eq!(
            parse_type_ref("Map.Entry<K, V>"),
            TypeRef::Named {
                name: "Map.Entry".into(),
                arguments: vec![
                    TypeRef::Named {
                        name: "K".into(),
                        arguments: Vec::new()
                    },
                    TypeRef::Named {
                        name: "V".into(),
                        arguments: Vec::new()
                    },
                ],
            }
        );
    }

    #[test]
    fn a_wildcard_parses_as_a_wildcard() {
        assert_eq!(parse_type_ref("?"), TypeRef::Wildcard);
        assert_eq!(parse_type_ref("? extends Number"), TypeRef::Wildcard);
    }

    #[test]
    fn an_annotated_type_drops_the_annotation() {
        assert_eq!(
            parse_type_ref("@NonNull String"),
            TypeRef::Named {
                name: "String".into(),
                arguments: Vec::new()
            }
        );
    }

    #[test]
    fn nonsense_parses_to_a_named_type_rather_than_panicking() {
        // A buffer mid-keystroke produces exactly this.
        assert_eq!(
            parse_type_ref("List<"),
            TypeRef::Named {
                name: "List".into(),
                arguments: Vec::new(),
            }
        );
        assert_eq!(parse_type_ref(""), TypeRef::Wildcard);
    }

    /// A member type of a parameterized type is not its outer type: the
    /// arguments close before the name ends, and reading up to the first `<`
    /// would answer `Outer`'s members for an `Inner`.
    #[test]
    fn a_member_of_a_parameterized_type_is_unknown_rather_than_its_outer_type() {
        assert_eq!(parse_type_ref("Outer<String>.Inner"), TypeRef::Wildcard);
        assert_eq!(
            parse_type_ref("Outer<String>.Inner<Integer>"),
            TypeRef::Wildcard
        );
        assert_eq!(
            parse_type_ref("Map<String, List<Integer>>"),
            TypeRef::Named {
                name: "Map".into(),
                arguments: vec![
                    TypeRef::Named {
                        name: "String".into(),
                        arguments: Vec::new()
                    },
                    TypeRef::Named {
                        name: "List".into(),
                        arguments: vec![TypeRef::Named {
                            name: "Integer".into(),
                            arguments: Vec::new()
                        }],
                    },
                ],
            }
        );
    }

    #[test]
    fn deeply_nested_arguments_stop_instead_of_overflowing_the_stack() {
        let deep = format!("{}Box{}", "Box<".repeat(500), ">".repeat(500));
        let _ = parse_type_ref(&deep);
    }

    #[test]
    fn deeply_nested_arrays_stop_instead_of_overflowing_the_stack() {
        // A separate recursive path from the one above, and it needs its own
        // cap: nothing about `<` bounds how many `[]` a buffer can hold.
        let deep = format!("int{}", "[]".repeat(100_000));
        let _ = parse_type_ref(&deep);
    }

    #[test]
    fn a_non_ascii_type_name_survives_parsing() {
        assert_eq!(
            parse_type_ref("Größe"),
            TypeRef::Named {
                name: "Größe".into(),
                arguments: Vec::new()
            }
        );
        assert_eq!(
            parse_type_ref("Box<Größe>[]"),
            TypeRef::Array(Box::new(TypeRef::Named {
                name: "Box".into(),
                arguments: vec![TypeRef::Named {
                    name: "Größe".into(),
                    arguments: Vec::new()
                }],
            }))
        );
    }
}
