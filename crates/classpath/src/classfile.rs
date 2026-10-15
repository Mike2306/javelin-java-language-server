//! Reading a `.class` file's declarations into [`TypeFacts`].
//!
//! Declarations only — the constant pool, access flags, `this_class`,
//! `super_class`, `interfaces`, fields and methods by name, descriptor and
//! flags, and the `Signature` and `InnerClasses` attributes. No `Code`, no
//! stack maps, no verification: a class file is a format whose names are
//! already resolved, which is why reading one is not a second Java front end
//! (ADR 0005).
//!
//! The one soundness rule is erasure. A descriptor is erased — `List.get`
//! returns `Object` — and a member typed `java.lang.Object` would let overload
//! elimination exclude `log(String)` at `log(names.get(0))`. So a member's
//! types come from its `Signature` whenever it has one, where a type variable
//! stays a type variable and is recorded as [`MemberType::Unresolved`], as the
//! source reader records it.

use std::collections::HashMap;

use javelin_core::{Error, Result};
use javelin_parser::DeclKind;

use crate::sources::{InScope, MemberFacts, MemberType, TypeFacts};

const PRIVATE: u16 = 0x0002;
const PUBLIC: u16 = 0x0001;
const STATIC: u16 = 0x0008;
const ABSTRACT: u16 = 0x0400;
const BRIDGE: u16 = 0x0040;
const VARARGS: u16 = 0x0080;
const INTERFACE: u16 = 0x0200;
const SYNTHETIC: u16 = 0x1000;
const ANNOTATION: u16 = 0x2000;
const ENUM: u16 = 0x4000;
const MODULE: u16 = 0x8000;

/// How deep `InnerClasses` nesting is followed before a name is given up on —
/// a cycle is invalid and must not loop.
const MAX_NESTING: usize = 64;

/// The facts one class file declares, or `None` for one nothing outside it can
/// name: an anonymous or local class, a synthetic one, `module-info` and
/// `package-info`.
///
/// An error for bytes that are not a class file; never a panic, and never an
/// allocation larger than the bytes can back.
pub fn read_class(bytes: &[u8]) -> Result<Option<TypeFacts>> {
    let mut input = Cursor { bytes, at: 0 };
    if input.u32()? != 0xCAFE_BABE {
        return Err(malformed("not a class file"));
    }
    input.skip(4)?; // minor and major version
    let pool = Pool::read(&mut input)?;
    let flags = input.u16()?;
    let this = pool.class(input.u16()?)?;
    let super_index = input.u16()?;
    let superclass = if super_index == 0 {
        None
    } else {
        Some(pool.class(super_index)?)
    };
    let interfaces = (0..input.u16()?)
        .map(|_| pool.class(input.u16()?))
        .collect::<Result<Vec<_>>>()?;
    let fields = read_members(&mut input, &pool)?;
    let methods = read_members(&mut input, &pool)?;
    let attributes = read_attributes(&mut input, &pool)?;

    if flags & (MODULE | SYNTHETIC) != 0
        || this.ends_with("module-info")
        || this.ends_with("package-info")
    {
        return Ok(None);
    }
    let nesting = attributes
        .iter()
        .find(|(name, _)| *name == "InnerClasses")
        .map(|(_, body)| inner_classes(body, &pool))
        .transpose()?
        .unwrap_or_default();
    let names = Names { nesting: &nesting };
    let Some(fqn) = names.fqn(this) else {
        return Ok(None);
    };
    let simple = fqn.rsplit('.').next().unwrap_or(&fqn).to_owned();

    let kind = if flags & ANNOTATION != 0 {
        DeclKind::AnnotationType
    } else if flags & INTERFACE != 0 {
        DeclKind::Interface
    } else if flags & ENUM != 0 {
        DeclKind::Enum
    } else if superclass == Some("java/lang/Record") {
        DeclKind::Record
    } else {
        DeclKind::Class
    };

    // An interface's `super_class` is always `java/lang/Object`, which the
    // source reader never records for one; dropped so the two agree.
    let mut supertypes = Vec::new();
    let mut supertypes_complete = true;
    let written = superclass
        .filter(|_| kind != DeclKind::Interface && kind != DeclKind::AnnotationType)
        .into_iter()
        .chain(interfaces);
    for binary in written {
        match names.fqn(binary) {
            Some(fqn) => supertypes.push(fqn),
            None => supertypes_complete = false,
        }
    }

    // A class's `Signature` is its type parameters and then its supertypes
    // as written, arguments included, in `super_class`-then-`interfaces`
    // order.
    let (type_parameters, written_supertypes) = match signature(&attributes, &pool)? {
        Some(text) => {
            let mut parser = Signature::new(text);
            let parameters = parser.formal_parameters()?;
            let mut written = Vec::new();
            while parser.peek().is_some() {
                written.push(parser.reference()?);
            }
            (parameters, written)
        }
        None => (Vec::new(), Vec::new()),
    };
    let owner = InScope {
        owner: &type_parameters,
        own: &[],
    };
    let mut supertype_arguments = Vec::new();
    for written in &written_supertypes {
        if let MemberType::Parameterized(fqn, arguments) = names.member_type(written, &owner)
            && supertypes.contains(&fqn)
            && !supertype_arguments.iter().any(|(seen, _)| *seen == fqn)
        {
            supertype_arguments.push((fqn, arguments));
        }
    }

    let mut members = Vec::new();
    for field in &fields {
        if field.flags & (PRIVATE | SYNTHETIC) != 0 {
            continue;
        }
        let text = field.signature.unwrap_or(field.descriptor);
        let declared = trusted(Signature::new(text).java_type()?, field.signature.is_some());
        let scope = InScope {
            owner: &type_parameters,
            own: &[],
        };
        members.push(MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: field.name.to_owned(),
            kind: if field.flags & ENUM != 0 {
                DeclKind::EnumConstant
            } else {
                DeclKind::Field
            },
            is_static: field.flags & STATIC != 0,
            is_abstract: false,
            is_public: field.flags & PUBLIC != 0,
            declared: names.member_type(&declared, &scope),
            parameters: Vec::new(),
            variadic: false,
            name_start: 0,
            name_end: 0,
        });
    }
    for method in &methods {
        if method.flags & (PRIVATE | SYNTHETIC | BRIDGE) != 0
            || method.name == "<init>"
            || method.name == "<clinit>"
        {
            continue;
        }
        let text = method.signature.unwrap_or(method.descriptor);
        let (own, parameters, declared) = Signature::new(text).method()?;
        let scope = InScope {
            owner: &type_parameters,
            own: &own,
        };
        let signed = method.signature.is_some();
        let parameters: Vec<JavaType> = parameters
            .into_iter()
            .map(|ty| trusted(ty, signed))
            .collect();
        let declared = trusted(declared, signed);
        members.push(MemberFacts {
            type_parameters: own.clone(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: method.name.to_owned(),
            kind: if kind == DeclKind::AnnotationType {
                DeclKind::AnnotationElement
            } else {
                DeclKind::Method
            },
            is_static: method.flags & STATIC != 0,
            is_abstract: method.flags & ABSTRACT != 0,
            is_public: method.flags & PUBLIC != 0,
            declared: names.member_type(&declared, &scope),
            parameters: parameters
                .iter()
                .map(|ty| names.member_type(ty, &scope))
                .collect(),
            variadic: method.flags & VARARGS != 0,
            name_start: 0,
            name_end: 0,
        });
    }

    Ok(Some(TypeFacts {
        constructors: Vec::new(),
        doc_span: None,
        name_start: 0,
        name_end: 0,
        fqn,
        simple,
        kind,
        // A nested class's own flags say `public` for `protected` too; its
        // `InnerClasses` entry keeps what the source wrote.
        is_public: nesting.get(this).map_or(flags, |entry| entry.2) & PUBLIC != 0,
        type_parameters,
        supertypes,
        supertype_arguments,
        supertypes_complete,
        // A class file holds what javac generated too: an enum's `values()`
        // and a record's accessors are in it.
        members_complete: true,
        members,
        // A class file has no text to jump into.
        source: String::new(),
    }))
}

/// Every nameable class a JAR holds.
///
/// Every `.class` entry but `module-info` and `package-info`, and nothing
/// under `META-INF/versions/`. An entry over `max_entry_bytes` — declared or
/// actual — or one that does not parse is skipped, so one bad entry costs that
/// entry and not the JAR. `cancel` is checked per entry.
///
/// ponytail: a multi-release JAR's overlays are not read, so a class whose
/// newer-release version adds a member is read as the base version — and is
/// marked `members_complete: false`, so nothing claims one of its members is
/// absent. The upgrade is reading the overlay for the release the JDK in use
/// is.
pub fn read_jar_facts(
    jar: &std::path::Path,
    max_entry_bytes: u64,
    cancel: &javelin_core::CancellationToken,
) -> Result<Vec<TypeFacts>> {
    read_jar_facts_reporting(jar, max_entry_bytes, cancel).map(|(facts, _)| facts)
}

/// [`read_jar_facts`], and whether every class in the JAR was read: `false`
/// when one was dropped for its size or because it could not be read, so a
/// type it declares is missing.
pub fn read_jar_facts_reporting(
    jar: &std::path::Path,
    max_entry_bytes: u64,
    cancel: &javelin_core::CancellationToken,
) -> Result<(Vec<TypeFacts>, bool)> {
    use std::io::Read as _;
    let file = std::fs::File::open(jar).map_err(|source| Error::io(jar, source))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|err| Error::Parse {
        path: jar.to_path_buf(),
        message: err.to_string(),
    })?;
    let mut facts = Vec::new();
    let mut entries: Vec<String> = Vec::new();
    let mut overlaid: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut whole = true;
    for index in 0..archive.len() {
        cancel.check()?;
        let Ok(entry) = archive.by_index(index) else {
            whole = false;
            continue;
        };
        let name = entry.name();
        if let Some(versioned) = name.strip_prefix("META-INF/versions/")
            && let Some((_, base)) = versioned.split_once('/')
            && base.ends_with(".class")
        {
            overlaid.insert(base.to_owned());
            continue;
        }
        if entry.is_dir() || !name.ends_with(".class") || name.starts_with("META-INF/") {
            continue;
        }
        if entry.size() > max_entry_bytes {
            whole = false;
            continue;
        }
        let name = name.to_owned();
        let mut bytes = Vec::new();
        if entry
            .take(max_entry_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .is_err()
            || bytes.len() as u64 > max_entry_bytes
        {
            whole = false;
            continue;
        }
        match read_class(&bytes) {
            Ok(Some(found)) => {
                facts.push(found);
                entries.push(name);
            }
            Ok(None) => {}
            Err(err) => {
                whole = false;
                tracing::debug!(jar = %jar.display(), entry = %name, %err, "class file skipped")
            }
        }
    }
    for (found, entry) in facts.iter_mut().zip(&entries) {
        if overlaid.contains(entry) {
            found.members_complete = false;
        }
    }
    Ok((facts, whole))
}

/// A type as far as it can be believed: from a descriptor alone, `Object` may
/// be an erased type variable whose `Signature` was stripped — ProGuard and
/// some Scala output — so it is kept as text and claims nothing.
///
/// ponytail: a *bounded* type variable erases to its bound, which a stripped
/// class file types as the bound, and an argument of that bound then excludes
/// a parameter of a narrower type Java would pick. javac always writes the
/// `Signature`; the gap is post-processed JARs. The upgrade is recognising a
/// class file whose signatures were stripped and trusting none of its types.
fn trusted(ty: JavaType, signed: bool) -> JavaType {
    match ty {
        JavaType::Class(binary, _) if !signed && binary == "java/lang/Object" => {
            JavaType::Variable("java.lang.Object".to_owned())
        }
        ty => ty,
    }
}

/// Whether `jar` registers an annotation processor — a
/// `META-INF/services/javax.annotation.processing.Processor` entry — so that a
/// build using it generates types no source file declares. A JAR that cannot
/// be read says it does, which is the answer that makes a caller careful.
pub fn registers_processor(jar: &std::path::Path) -> bool {
    std::fs::File::open(jar)
        .ok()
        .and_then(|file| zip::ZipArchive::new(file).ok())
        .is_none_or(|mut archive| {
            archive
                .by_name("META-INF/services/javax.annotation.processing.Processor")
                .is_ok()
        })
}

fn malformed(message: &str) -> Error {
    Error::Parse {
        path: std::path::PathBuf::from("<class file>"),
        message: message.to_owned(),
    }
}

struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .at
            .checked_add(len)
            .filter(|end| *end <= self.bytes.len())
            .ok_or_else(|| malformed("truncated"))?;
        let taken = &self.bytes[self.at..end];
        self.at = end;
        Ok(taken)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.take(len).map(|_| ())
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// The constant pool, as far as a declaration needs it: text and class names.
enum Constant<'a> {
    Utf8(&'a str),
    Class(u16),
    Other,
}

struct Pool<'a> {
    entries: Vec<Constant<'a>>,
}

impl<'a> Pool<'a> {
    fn read(input: &mut Cursor<'a>) -> Result<Self> {
        let count = input.u16()? as usize;
        let mut entries = Vec::with_capacity(count);
        entries.push(Constant::Other); // index 0 is unused
        while entries.len() < count {
            let tag = input.u8()?;
            let (entry, slots) = match tag {
                1 => {
                    let len = input.u16()? as usize;
                    // Modified UTF-8 differs from UTF-8 only for NUL and
                    // supplementary characters, which no name this reader
                    // keeps is expected to hold; one that does is skipped.
                    let text = std::str::from_utf8(input.take(len)?).unwrap_or("");
                    (Constant::Utf8(text), 1)
                }
                7 => (Constant::Class(input.u16()?), 1),
                3 | 4 => {
                    input.skip(4)?;
                    (Constant::Other, 1)
                }
                5 | 6 => {
                    input.skip(8)?;
                    (Constant::Other, 2)
                }
                8 | 16 | 19 | 20 => {
                    input.skip(2)?;
                    (Constant::Other, 1)
                }
                9 | 10 | 11 | 12 | 17 | 18 => {
                    input.skip(4)?;
                    (Constant::Other, 1)
                }
                15 => {
                    input.skip(3)?;
                    (Constant::Other, 1)
                }
                _ => return Err(malformed("unknown constant pool tag")),
            };
            entries.push(entry);
            if slots == 2 {
                entries.push(Constant::Other);
            }
        }
        Ok(Self { entries })
    }

    fn utf8(&self, index: u16) -> Result<&'a str> {
        match self.entries.get(index as usize) {
            Some(Constant::Utf8(text)) => Ok(text),
            _ => Err(malformed("expected a UTF-8 constant")),
        }
    }

    fn class(&self, index: u16) -> Result<&'a str> {
        match self.entries.get(index as usize) {
            Some(Constant::Class(name)) => self.utf8(*name),
            _ => Err(malformed("expected a class constant")),
        }
    }
}

struct Member<'a> {
    flags: u16,
    name: &'a str,
    descriptor: &'a str,
    signature: Option<&'a str>,
}

fn read_members<'a>(input: &mut Cursor<'a>, pool: &Pool<'a>) -> Result<Vec<Member<'a>>> {
    let count = input.u16()?;
    let mut members = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let flags = input.u16()?;
        let name = pool.utf8(input.u16()?)?;
        let descriptor = pool.utf8(input.u16()?)?;
        let attributes = read_attributes(input, pool)?;
        members.push(Member {
            flags,
            name,
            descriptor,
            signature: signature(&attributes, pool)?,
        });
    }
    Ok(members)
}

fn read_attributes<'a>(
    input: &mut Cursor<'a>,
    pool: &Pool<'a>,
) -> Result<Vec<(&'a str, &'a [u8])>> {
    let count = input.u16()?;
    let mut attributes = Vec::with_capacity(count as usize);
    for _ in 0..count {
        let name = pool.utf8(input.u16()?)?;
        let len = input.u32()? as usize;
        attributes.push((name, input.take(len)?));
    }
    Ok(attributes)
}

fn signature<'a>(attributes: &[(&str, &[u8])], pool: &Pool<'a>) -> Result<Option<&'a str>> {
    let Some((_, body)) = attributes.iter().find(|(name, _)| *name == "Signature") else {
        return Ok(None);
    };
    let mut body = Cursor { bytes: body, at: 0 };
    pool.utf8(body.u16()?).map(Some)
}

/// Each nested class's binary name to its outer class's binary name, its
/// own simple name and its access flags as written, where the class file
/// records them. `None` for either name means anonymous (no simple name) or
/// local (no outer class).
type Nesting<'a> = HashMap<&'a str, (Option<&'a str>, Option<&'a str>, u16)>;

fn inner_classes<'a>(body: &'a [u8], pool: &Pool<'a>) -> Result<Nesting<'a>> {
    let mut input = Cursor { bytes: body, at: 0 };
    let mut nesting = HashMap::new();
    for _ in 0..input.u16()? {
        let inner = pool.class(input.u16()?)?;
        let outer = match input.u16()? {
            0 => None,
            index => Some(pool.class(index)?),
        };
        let simple = match input.u16()? {
            0 => None,
            index => Some(pool.utf8(index)?),
        };
        let flags = input.u16()?;
        nesting.insert(inner, (outer, simple, flags));
    }
    Ok(nesting)
}

struct Names<'n, 'a> {
    nesting: &'n Nesting<'a>,
}

impl Names<'_, '_> {
    /// The source-form name of a binary one, or `None` for a class nothing
    /// outside it can name.
    fn fqn(&self, binary: &str) -> Option<String> {
        self.fqn_at(binary, 0)
    }

    fn fqn_at(&self, binary: &str, depth: usize) -> Option<String> {
        if depth > MAX_NESTING {
            return None;
        }
        match self.nesting.get(binary) {
            Some((Some(outer), Some(simple), _)) => {
                Some(format!("{}.{simple}", self.fqn_at(outer, depth + 1)?))
            }
            Some(_) => None,
            // ponytail: no `InnerClasses` entry, so `$` is read as nesting —
            // a top-level class whose own name holds a `$` is misnamed. javac
            // records every nested class a file mentions, so this is reached
            // for foreign names in a hand-written or obfuscated class file.
            // A trailing or doubled `$` — Scala's `Foo$` module class — is
            // not a name Java source can write. The upgrade is naming it from
            // its own class file, whose `InnerClasses` lists it if it is
            // nested — a second pass over the JAR, and a change to the facts
            // the JAR store keeps.
            None => {
                let fqn = binary.replace(['/', '$'], ".");
                (!fqn.split('.').any(str::is_empty)).then_some(fqn)
            }
        }
    }

    /// A type as a fact: an owner's type variable is a `Variable`, a
    /// method's own and an array claim nothing, and a class keeps its
    /// arguments.
    fn member_type(&self, ty: &JavaType, scope: &InScope<'_>) -> MemberType {
        match ty {
            JavaType::Base(keyword) if *keyword == "void" => MemberType::Void,
            JavaType::Base(keyword) => MemberType::Primitive((*keyword).to_owned()),
            JavaType::Class(binary, arguments) => match self.fqn(binary) {
                Some(fqn) if arguments.is_empty() => MemberType::Class(fqn),
                Some(fqn) => MemberType::Parameterized(
                    fqn,
                    arguments
                        .iter()
                        .map(|argument| match argument {
                            Argument::Any => MemberType::Unresolved("?".to_owned()),
                            Argument::Exact(ty) => self.member_type(ty, scope),
                            Argument::Extends(ty) => {
                                MemberType::Extends(Box::new(self.member_type(ty, scope)))
                            }
                            Argument::Super(ty) => {
                                MemberType::Super(Box::new(self.member_type(ty, scope)))
                            }
                        })
                        .collect(),
                ),
                None => MemberType::Unresolved(binary.replace(['/', '$'], ".")),
            },
            JavaType::Variable(name) if scope.owner.contains(name) && !scope.own.contains(name) => {
                MemberType::Variable(name.clone())
            }
            JavaType::Variable(_) | JavaType::Array(_) => MemberType::Unresolved(self.render(ty)),
        }
    }

    fn render(&self, ty: &JavaType) -> String {
        match ty {
            JavaType::Base(keyword) => (*keyword).to_owned(),
            JavaType::Class(binary, _) => self
                .fqn(binary)
                .unwrap_or_else(|| binary.replace(['/', '$'], ".")),
            JavaType::Variable(name) => name.clone(),
            JavaType::Array(element) => format!("{}[]", self.render(element)),
        }
    }
}

/// A type as a descriptor or a signature writes it.
#[derive(Debug, PartialEq)]
enum JavaType {
    Base(&'static str),
    /// A binary name, `java/util/Map$Entry`, and its type arguments — none
    /// for a member of a parameterized outer class, `Outer<T>.Inner<U>`,
    /// whose arguments are dropped.
    Class(String, Vec<Argument>),
    Variable(String),
    Array(Box<JavaType>),
}

/// One type argument, JVMS 4.7.9.1.
#[derive(Debug, PartialEq)]
enum Argument {
    /// `*`, an unbounded `?`.
    Any,
    Exact(JavaType),
    /// `+`, `? extends`.
    Extends(JavaType),
    /// `-`, `? super`.
    Super(JavaType),
}

/// Descriptors and signatures, JVMS 4.3 and 4.7.9.1: the same grammar, a
/// descriptor being a signature with no generics in it.
struct Signature<'a> {
    text: &'a [u8],
    at: usize,
    /// How deep the recursion into arrays and type arguments is: bounded,
    /// because a crafted class file can nest without end and the stack cannot.
    depth: usize,
}

/// JVMS 4.4.1 caps an array at 255 dimensions; type arguments nest far less
/// in any real signature. Both count against one budget.
const MAX_DEPTH: usize = 300;

impl<'a> Signature<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text: text.as_bytes(),
            at: 0,
            depth: 0,
        }
    }

    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    fn next(&mut self) -> Result<u8> {
        let byte = self
            .peek()
            .ok_or_else(|| malformed("truncated signature"))?;
        self.at += 1;
        Ok(byte)
    }

    fn expect(&mut self, byte: u8) -> Result<()> {
        if self.next()? == byte {
            Ok(())
        } else {
            Err(malformed("unexpected signature character"))
        }
    }

    /// Up to, not including, the first of `ends`.
    fn identifier(&mut self, ends: &[u8]) -> Result<&'a str> {
        let start = self.at;
        while let Some(byte) = self.peek() {
            if ends.contains(&byte) {
                break;
            }
            self.at += 1;
        }
        if self.at == start || self.peek().is_none() {
            return Err(malformed("bad identifier in signature"));
        }
        std::str::from_utf8(&self.text[start..self.at]).map_err(|_| malformed("bad signature"))
    }

    /// `<T:Ljava/lang/Object;U::Ljava/lang/Comparable<TU;>;>`, by name; none
    /// when there is none.
    fn formal_parameters(&mut self) -> Result<Vec<String>> {
        if self.peek() != Some(b'<') {
            return Ok(Vec::new());
        }
        self.at += 1;
        let mut names = Vec::new();
        while self.peek() != Some(b'>') {
            names.push(self.identifier(b":")?.to_owned());
            while self.peek() == Some(b':') {
                self.at += 1;
                // A class bound may be empty: `T::Ljava/lang/Runnable;`.
                if !matches!(self.peek(), Some(b':') | Some(b'>')) {
                    self.reference()?;
                }
                if matches!(self.peek(), Some(b'>')) {
                    break;
                }
                if !matches!(self.peek(), Some(b':')) {
                    break;
                }
            }
        }
        self.expect(b'>')?;
        Ok(names)
    }

    /// A method's own type parameters, its parameters and its return type.
    fn method(&mut self) -> Result<(Vec<String>, Vec<JavaType>, JavaType)> {
        let own = self.formal_parameters()?;
        self.expect(b'(')?;
        let mut parameters = Vec::new();
        while self.peek() != Some(b')') {
            parameters.push(self.java_type()?);
        }
        self.expect(b')')?;
        let returns = self.java_type()?;
        Ok((own, parameters, returns))
    }

    fn java_type(&mut self) -> Result<JavaType> {
        let base = match self.peek() {
            Some(b'B') => "byte",
            Some(b'C') => "char",
            Some(b'D') => "double",
            Some(b'F') => "float",
            Some(b'I') => "int",
            Some(b'J') => "long",
            Some(b'S') => "short",
            Some(b'Z') => "boolean",
            Some(b'V') => "void",
            _ => return self.reference(),
        };
        self.at += 1;
        Ok(JavaType::Base(base))
    }

    fn reference(&mut self) -> Result<JavaType> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(malformed("signature nested too deeply"));
        }
        let found = self.reference_at_depth();
        self.depth -= 1;
        found
    }

    fn reference_at_depth(&mut self) -> Result<JavaType> {
        match self.next()? {
            b'L' => {
                let mut binary = self.identifier(b"<;.")?.to_owned();
                let mut arguments = Vec::new();
                // `Outer<TT;>.Inner`: a member of a parameterized outer, whose
                // arguments are not the inner's — so neither set is kept.
                let mut in_parameterized = false;
                loop {
                    if self.peek() == Some(b'<') {
                        arguments = self.arguments()?;
                    }
                    match self.next()? {
                        b';' if in_parameterized => return Ok(JavaType::Class(binary, Vec::new())),
                        b';' => return Ok(JavaType::Class(binary, arguments)),
                        b'.' => {
                            in_parameterized |= !arguments.is_empty();
                            binary.push('$');
                            binary.push_str(self.identifier(b"<;.")?);
                        }
                        _ => return Err(malformed("bad class type in signature")),
                    }
                }
            }
            b'T' => {
                let name = self.identifier(b";")?.to_owned();
                self.expect(b';')?;
                Ok(JavaType::Variable(name))
            }
            b'[' => Ok(JavaType::Array(Box::new(self.java_type()?))),
            _ => Err(malformed("bad type in signature")),
        }
    }

    fn arguments(&mut self) -> Result<Vec<Argument>> {
        self.expect(b'<')?;
        let mut arguments = Vec::new();
        while self.peek() != Some(b'>') {
            arguments.push(match self.peek() {
                Some(b'*') => {
                    self.at += 1;
                    Argument::Any
                }
                Some(b'+') => {
                    self.at += 1;
                    Argument::Extends(self.reference()?)
                }
                Some(b'-') => {
                    self.at += 1;
                    Argument::Super(self.reference()?)
                }
                _ => Argument::Exact(self.reference()?),
            });
        }
        self.expect(b'>')?;
        Ok(arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/classfiles/classes/com/acme")
            .join(format!("{name}.class"));
        std::fs::read(&path).unwrap_or_else(|err| panic!("{}: {err}", path.display()))
    }

    fn facts(name: &str) -> TypeFacts {
        read_class(&fixture(name))
            .expect("a class file")
            .expect("a nameable class")
    }

    fn member<'a>(facts: &'a TypeFacts, name: &str) -> &'a MemberFacts {
        facts
            .members
            .iter()
            .find(|member| member.name == name)
            .unwrap_or_else(|| panic!("no {name} in {:?}", names(facts)))
    }

    fn names(facts: &TypeFacts) -> Vec<&str> {
        facts
            .members
            .iter()
            .map(|member| member.name.as_str())
            .collect()
    }

    fn class(fqn: &str) -> MemberType {
        MemberType::Class(fqn.to_owned())
    }

    fn unresolved(text: &str) -> MemberType {
        MemberType::Unresolved(text.to_owned())
    }

    #[test]
    fn a_generic_class_reads_its_name_kind_supertypes_and_arity() {
        let facts = facts("Box");
        assert_eq!(facts.fqn, "com.acme.Box");
        assert_eq!(facts.simple, "Box");
        assert_eq!(facts.kind, DeclKind::Class);
        assert_eq!(facts.type_parameters, vec!["T"]);
        assert_eq!(
            facts.supertypes,
            vec!["java.lang.Object", "java.io.Serializable"]
        );
        assert!(facts.supertypes_complete);
        assert_eq!(facts.source, "", "a class file has no text to jump into");
    }

    #[test]
    fn private_synthetic_and_initialiser_members_are_not_facts() {
        let facts = facts("Box");
        let mut listed = names(&facts);
        listed.sort_unstable();
        assert_eq!(
            listed,
            vec![
                "LIMIT", "get", "label", "lambda", "local", "map", "names", "of", "put", "shadow",
                "shelves", "task"
            ]
        );
    }

    fn variable(name: &str) -> MemberType {
        MemberType::Variable(name.to_owned())
    }

    fn parameterized(fqn: &str, arguments: Vec<MemberType>) -> MemberType {
        MemberType::Parameterized(fqn.to_owned(), arguments)
    }

    /// The soundness rule: `get`'s descriptor returns `Object`, its signature
    /// returns `T`, and only the second is what the source says — a variable
    /// of the class, which a use's argument can replace. A method's own `R`
    /// is never one.
    #[test]
    fn a_type_variable_is_a_variable_or_unresolved_and_never_its_erasure() {
        let facts = facts("Box");
        assert_eq!(member(&facts, "get").declared, variable("T"));
        let map = member(&facts, "map");
        assert_eq!(map.declared, unresolved("R"));
        assert_eq!(
            map.parameters,
            vec![parameterized(
                "java.util.function.Function",
                vec![variable("T"), unresolved("R")]
            )]
        );
        let put = member(&facts, "put");
        assert_eq!(put.declared, MemberType::Void);
        assert_eq!(put.parameters, vec![variable("T"), unresolved("int[]")]);
        // `<T> T shadow(T)`: the method's `T`, not the class's.
        let shadow = member(&facts, "shadow");
        assert_eq!(shadow.declared, unresolved("T"));
        assert_eq!(shadow.parameters, vec![unresolved("T")]);
    }

    #[test]
    fn a_parameterized_type_keeps_its_arguments_and_an_array_is_unresolved() {
        let facts = facts("Box");
        assert_eq!(
            member(&facts, "names").declared,
            parameterized("java.util.List", vec![class("java.lang.String")])
        );
        assert_eq!(
            member(&facts, "shelves").declared,
            unresolved("java.util.List[]")
        );
        let of = member(&facts, "of");
        assert!(of.is_static && of.variadic);
        assert_eq!(
            of.declared,
            parameterized("com.acme.Box", vec![class("java.lang.String")])
        );
        assert_eq!(of.parameters, vec![unresolved("java.lang.String[]")]);
        let limit = member(&facts, "LIMIT");
        assert!(limit.is_static);
        assert_eq!(limit.declared, MemberType::Primitive("int".to_owned()));
        assert_eq!(member(&facts, "task").declared, class("java.lang.Runnable"));
    }

    #[test]
    fn a_supertype_written_with_arguments_records_them() {
        assert_eq!(
            facts("Square").supertype_arguments,
            vec![(
                "java.lang.Comparable".to_owned(),
                vec![class("com.acme.Square")]
            )]
        );
        assert!(facts("Box").supertype_arguments.is_empty());
    }

    #[test]
    fn signature_arguments_parse_wildcards_and_drop_a_parameterized_outers() {
        let wild = Signature::new("Ljava/util/Map<*+TK;-Ljava/lang/String;>;")
            .java_type()
            .expect("a signature");
        assert_eq!(
            wild,
            JavaType::Class(
                "java/util/Map".to_owned(),
                vec![
                    Argument::Any,
                    Argument::Extends(JavaType::Variable("K".to_owned())),
                    Argument::Super(JavaType::Class("java/lang/String".to_owned(), Vec::new())),
                ]
            )
        );
        let inner = Signature::new("La/Outer<TT;>.Inner<TU;>;")
            .java_type()
            .expect("a signature");
        assert_eq!(
            inner,
            JavaType::Class("a/Outer$Inner".to_owned(), Vec::new())
        );
    }

    /// A member with no `Signature` whose descriptor says `Object` may be a
    /// type variable whose `Signature` was stripped — ProGuard, some Scala —
    /// and `Object` would exclude `log(String)` where Java picks it.
    #[test]
    fn a_descriptor_only_object_is_not_claimed() {
        assert_eq!(
            member(&facts("Square"), "any").declared,
            unresolved("java.lang.Object")
        );
    }

    #[test]
    fn an_anonymous_or_local_class_is_not_a_fact() {
        for name in ["Box$1", "Box$1Local"] {
            assert_eq!(
                read_class(&fixture(name)).expect("a class file"),
                None,
                "{name}"
            );
        }
    }

    #[test]
    fn a_nested_class_is_named_through_its_outer_classes() {
        assert_eq!(facts("Box$Entry").fqn, "com.acme.Box.Entry");
        let deeper = facts("Box$Inner$Deeper");
        assert_eq!(deeper.fqn, "com.acme.Box.Inner.Deeper");
        assert_eq!(deeper.simple, "Deeper");
    }

    #[test]
    fn a_name_java_source_cannot_write_is_not_a_fact() {
        let nesting = Nesting::new();
        let names = Names { nesting: &nesting };
        assert_eq!(names.fqn("scala/Foo$"), None);
        assert_eq!(names.fqn("a/B$$C"), None);
        assert_eq!(names.fqn("a/B$C").as_deref(), Some("a.B.C"));
    }

    #[test]
    fn an_interface_records_no_object_and_keeps_its_statics() {
        let facts = facts("Shape");
        assert_eq!(facts.kind, DeclKind::Interface);
        assert!(facts.supertypes.is_empty(), "{:?}", facts.supertypes);
        assert!(member(&facts, "SIDES").is_static);
        assert!(member(&facts, "unit").is_static);
        assert!(!member(&facts, "describe").is_static);
        assert_eq!(
            member(&facts, "area").declared,
            MemberType::Primitive("double".to_owned())
        );
        assert!(member(&facts, "area").is_abstract);
        assert!(!member(&facts, "describe").is_abstract, "a default method");
        assert!(!member(&facts, "unit").is_abstract, "a static method");
        let square = self::facts("Square");
        assert!(!member(&square, "area").is_abstract);
    }

    #[test]
    fn a_bridge_method_is_not_a_second_overload() {
        let facts = facts("Square");
        let compare: Vec<&MemberFacts> = facts
            .members
            .iter()
            .filter(|member| member.name == "compareTo")
            .collect();
        assert_eq!(compare.len(), 1, "{compare:?}");
        assert_eq!(compare[0].parameters, vec![class("com.acme.Square")]);
        assert_eq!(
            facts.supertypes,
            vec!["java.lang.Object", "com.acme.Shape", "java.lang.Comparable"]
        );
    }

    #[test]
    fn an_enum_record_and_annotation_type_carry_their_written_supertype_and_are_complete() {
        let color = facts("Color");
        assert_eq!(color.kind, DeclKind::Enum);
        assert_eq!(color.supertypes, vec!["java.lang.Enum"]);
        assert!(color.supertypes_complete);
        assert_eq!(member(&color, "RED").kind, DeclKind::EnumConstant);
        assert!(member(&color, "RED").is_static);
        let mut listed = names(&color);
        listed.sort_unstable();
        assert_eq!(listed, vec!["GREEN", "RED", "code", "valueOf", "values"]);

        let point = facts("Point");
        assert_eq!(point.kind, DeclKind::Record);
        assert_eq!(point.supertypes, vec!["java.lang.Record"]);
        assert_eq!(member(&point, "x").kind, DeclKind::Method);

        let tag = facts("Tag");
        assert_eq!(tag.kind, DeclKind::AnnotationType);
        assert_eq!(tag.supertypes, vec!["java.lang.annotation.Annotation"]);
        assert_eq!(member(&tag, "weight").kind, DeclKind::AnnotationElement);
    }

    fn jar_fixture() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures/classfiles/acme.jar")
    }

    #[test]
    fn a_protected_nested_class_is_not_public_though_its_own_flags_say_so() {
        assert!(!facts("Box$Guarded").is_public);
        assert!(facts("Box$Entry").is_public);
        assert!(facts("Box").is_public);
    }

    #[test]
    fn a_jar_yields_every_nameable_class_it_holds() {
        let facts = read_jar_facts(
            &jar_fixture(),
            1 << 20,
            &javelin_core::CancellationToken::new(),
        )
        .expect("a jar");
        let mut fqns: Vec<&str> = facts.iter().map(|facts| facts.fqn.as_str()).collect();
        fqns.sort_unstable();
        assert_eq!(
            fqns,
            vec![
                "com.acme.Box",
                "com.acme.Box.Entry",
                "com.acme.Box.Guarded",
                "com.acme.Box.Inner",
                "com.acme.Box.Inner.Deeper",
                "com.acme.Color",
                "com.acme.Point",
                "com.acme.Shape",
                "com.acme.Square",
                "com.acme.Tag",
            ]
        );
    }

    /// One bad entry costs that entry, never the JAR; a multi-release
    /// overlay and an entry over the size limit are not read.
    #[test]
    fn a_bad_entry_an_overlay_and_an_oversized_entry_are_skipped() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("javelin-jar-skips-{}.jar", std::process::id()));
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("jar"));
            let options = zip::write::SimpleFileOptions::default();
            for (name, bytes) in [
                ("com/acme/Box.class", fixture("Box")),
                (
                    "com/acme/Broken.class",
                    b"\xca\xfe\xba\xbe garbage".to_vec(),
                ),
                (
                    "META-INF/versions/21/com/acme/Shape.class",
                    fixture("Shape"),
                ),
                ("com/acme/Huge.class", {
                    let mut huge = fixture("Tag");
                    huge.resize(16_384, 0);
                    huge
                }),
            ] {
                zip.start_file(name, options).expect("entry");
                zip.write_all(&bytes).expect("write");
            }
            zip.finish().expect("finish");
        }
        let facts =
            read_jar_facts(&path, 8192, &javelin_core::CancellationToken::new()).expect("a jar");
        let fqns: Vec<&str> = facts.iter().map(|facts| facts.fqn.as_str()).collect();
        assert_eq!(fqns, vec!["com.acme.Box"]);
        let _ = std::fs::remove_file(&path);
    }

    /// A newer release's overlay can add members the base class lacks, and
    /// the overlay is not read — so the base class's members are not all of
    /// them.
    #[test]
    fn a_class_a_multi_release_overlay_replaces_has_no_whole_member_list() {
        use std::io::Write;
        let path =
            std::env::temp_dir().join(format!("javelin-jar-overlay-{}.jar", std::process::id()));
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("jar"));
            let options = zip::write::SimpleFileOptions::default();
            for (name, bytes) in [
                ("com/acme/Box.class", fixture("Box")),
                ("com/acme/Shape.class", fixture("Shape")),
                (
                    "META-INF/versions/21/com/acme/Shape.class",
                    fixture("Shape"),
                ),
            ] {
                zip.start_file(name, options).expect("entry");
                zip.write_all(&bytes).expect("write");
            }
            zip.finish().expect("finish");
        }
        let facts =
            read_jar_facts(&path, 1 << 20, &javelin_core::CancellationToken::new()).expect("a jar");
        let whole = |fqn: &str| {
            facts
                .iter()
                .find(|facts| facts.fqn == fqn)
                .expect(fqn)
                .members_complete
        };
        assert!(whole("com.acme.Box"));
        assert!(!whole("com.acme.Shape"));
        let _ = std::fs::remove_file(&path);
    }

    /// A JAR that registers an annotation processor means the build
    /// generates types no source declares, which diagnostics must allow for.
    #[test]
    fn a_jar_registering_an_annotation_processor_says_so() {
        use std::io::Write;
        assert!(!registers_processor(&jar_fixture()));
        let path =
            std::env::temp_dir().join(format!("javelin-jar-processor-{}.jar", std::process::id()));
        {
            let mut zip = zip::ZipWriter::new(std::fs::File::create(&path).expect("jar"));
            zip.start_file(
                "META-INF/services/javax.annotation.processing.Processor",
                zip::write::SimpleFileOptions::default(),
            )
            .expect("entry");
            zip.write_all(b"com.acme.Processor\n").expect("write");
            zip.finish().expect("finish");
        }
        assert!(registers_processor(&path));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_file_that_is_not_a_jar_is_an_error() {
        let path =
            std::env::temp_dir().join(format!("javelin-not-a-jar-{}.jar", std::process::id()));
        std::fs::write(&path, b"not a zip").expect("write");
        assert!(read_jar_facts(&path, 1 << 20, &javelin_core::CancellationToken::new()).is_err());
        let _ = std::fs::remove_file(&path);
    }

    /// Run by hand against a real JAR: `JAVELIN_JAR=<path> cargo test -p
    /// javelin-classpath real_jar -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads a JAR named by JAVELIN_JAR"]
    fn reading_a_real_jar_reports_its_cost() {
        let jar = std::env::var_os("JAVELIN_JAR").expect("JAVELIN_JAR names a jar");
        let started = std::time::Instant::now();
        let facts = read_jar_facts(
            std::path::Path::new(&jar),
            1 << 22,
            &javelin_core::CancellationToken::new(),
        )
        .expect("a jar");
        let members: usize = facts.iter().map(|facts| facts.members.len()).sum();
        println!(
            "{} types, {members} members in {:?}",
            facts.len(),
            started.elapsed()
        );
        assert!(!facts.is_empty());
    }

    /// A crafted signature nests without end; the parser recurses, so depth
    /// is bounded rather than left to the stack.
    #[test]
    fn a_signature_nested_past_any_real_depth_is_an_error_not_a_stack_overflow() {
        let arrays = format!("{}I", "[".repeat(100_000));
        assert!(Signature::new(&arrays).java_type().is_err());
        let arguments = format!(
            "{}{}",
            "Ljava/util/List<".repeat(50_000),
            ">;".repeat(50_000)
        );
        assert!(Signature::new(&arguments).java_type().is_err());
        let fine = format!("{}I", "[".repeat(255));
        assert!(
            Signature::new(&fine).java_type().is_ok(),
            "JVMS allows 255 dimensions"
        );
    }

    #[test]
    fn a_truncated_class_file_is_an_error_at_every_length() {
        let bytes = fixture("Box");
        for len in 0..bytes.len() {
            assert!(
                read_class(&bytes[..len]).is_err(),
                "a prefix of {len} bytes"
            );
        }
    }

    #[test]
    fn a_corrupted_class_file_never_panics() {
        let bytes = fixture("Box");
        for at in 0..bytes.len() {
            for flip in [0x01, 0x80, 0xff] {
                let mut corrupt = bytes.clone();
                corrupt[at] ^= flip;
                let _ = read_class(&corrupt);
            }
        }
    }
}
