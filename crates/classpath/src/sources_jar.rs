//! A dependency's `-sources.jar`, where one is already on disk: the text a
//! jump into a JAR type or member lands in.
//!
//! A class file is the fact — its members and their types are what the
//! compiler sees, and a source file is not trusted over it (ADR 0005) — so
//! the sources are read for *where* each name is written and nothing else:
//! a type's entry and the span of its name, a member's span. They are
//! grafted onto the class file's facts by fully-qualified name, and onto a
//! member only where exactly one written member can be it.
//!
//! Only a sources JAR where the build tool keeps one is read — Maven's
//! `a-1.0-sources.jar` in the same directory, Gradle's in a sibling hash
//! directory of its cache. `javelin_build::download_sources` fetches them to
//! exactly there, so nothing pairs a JAR with its sources but this module.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use javelin_core::{CancellationToken, Result};

use crate::sources::{MemberFacts, MemberType, TypeFacts, read_type_facts};

/// What separates the sources JAR from the entry in a grafted
/// [`TypeFacts::source`]: `/m2/a-1.0-sources.jar!/com/acme/A.java`. A
/// `src.zip` entry name never holds it.
pub const IN_ARCHIVE: &str = "!/";

/// The `-sources.jar` beside `jar`, if one is on disk.
pub fn sources_jar_for(jar: &Path) -> Option<PathBuf> {
    let stem = jar.file_stem()?.to_str()?;
    let name = format!("{stem}-sources.jar");
    let sibling = jar.with_file_name(&name);
    if sibling.is_file() {
        return Some(sibling);
    }
    // Gradle keeps each file of a module version in a directory named for
    // its hash: `files-2.1/<group>/<artifact>/<version>/<hash>/<file>` — and
    // a `-SNAPSHOT` once per build it fetched, so two of one name are two
    // builds, and which is this JAR's no name says.
    let version = jar.parent()?.parent()?;
    if !version
        .components()
        .any(|component| component.as_os_str() == "files-2.1")
    {
        return None;
    }
    let mut found = std::fs::read_dir(version)
        .ok()?
        .flatten()
        .map(|entry| entry.path().join(&name))
        .filter(|candidate| candidate.is_file());
    match (found.next(), found.next()) {
        (Some(only), None) => Some(only),
        _ => None,
    }
}

/// Graft where each of `facts`' types and members is written in
/// `sources_jar` onto them, and answer how many types took a source.
///
/// A type takes its entry and its name's span. A member takes its name's span
/// only when it is the one class-file member of its name, kind, staticness,
/// arity and variadicity that the sources write exactly once — or, among
/// several, the one whose parameter types' simple names match exactly one
/// written member's. Anything else keeps an empty span, which is no site:
/// a jump to the wrong overload is worse than none.
pub fn graft_sources(
    facts: &mut [TypeFacts],
    sources_jar: &Path,
    max_entry_bytes: u64,
    cancel: &CancellationToken,
) -> Result<usize> {
    // A name written twice — a multi-release overlay, a stray copy — is
    // written nowhere this can say.
    let mut written: HashMap<String, Option<TypeFacts>> = HashMap::new();
    for facts in read_type_facts(sources_jar, max_entry_bytes, cancel)? {
        if facts.source.starts_with("META-INF/") {
            continue;
        }
        written
            .entry(facts.fqn.clone())
            .and_modify(|seen| *seen = None)
            .or_insert(Some(facts));
    }
    let archive = sources_jar.to_string_lossy();
    let mut grafted = 0;
    for ty in facts.iter_mut() {
        let Some(Some(source)) = written.get(&ty.fqn) else {
            continue;
        };
        if source.source.is_empty() || source.name_end <= source.name_start {
            continue;
        }
        ty.source = format!("{archive}{IN_ARCHIVE}{}", source.source);
        ty.name_start = source.name_start;
        ty.name_end = source.name_end;
        ty.doc_span = source.doc_span;
        let compiled = ty.members.clone();
        for member in &mut ty.members {
            if let Some(found) = only_match(member, &compiled, &source.members) {
                member.name_start = found.name_start;
                member.name_end = found.name_end;
                member.doc_span = found.doc_span;
                // A name is shown on every call, so it is taken only where the
                // two say the same parameter types — not merely one member a
                // side, which another version's reordered overload can be.
                let shape = |found: &MemberFacts| -> Option<Vec<String>> {
                    found.parameters.iter().map(simple_name).collect()
                };
                if found.parameter_names.len() == member.parameters.len()
                    && shape(member).is_some_and(|compiled| shape(found) == Some(compiled))
                {
                    member.parameter_names.clone_from(&found.parameter_names);
                }
            }
        }
        grafted += 1;
    }
    Ok(grafted)
}

/// The one written member `member` is, among `written`, judged against the
/// rest of the `compiled` members it could be confused with.
fn only_match<'w>(
    member: &MemberFacts,
    compiled: &[MemberFacts],
    written: &'w [MemberFacts],
) -> Option<&'w MemberFacts> {
    let same = |a: &MemberFacts, b: &MemberFacts| {
        a.name == b.name
            && a.kind == b.kind
            && a.is_static == b.is_static
            && a.variadic == b.variadic
            && a.parameters.len() == b.parameters.len()
    };
    let rivals: Vec<&MemberFacts> = compiled
        .iter()
        .filter(|other| same(other, member))
        .collect();
    let candidates: Vec<&'w MemberFacts> =
        written.iter().filter(|other| same(other, member)).collect();
    let shape = |found: &MemberFacts| -> Option<Vec<String>> {
        found.parameters.iter().map(simple_name).collect()
    };
    // One each side is the same member — unless both say parameter types
    // that differ, which is a sources JAR of another version.
    if let ([_], [only]) = (rivals.as_slice(), candidates.as_slice()) {
        return match (shape(member), shape(only)) {
            (Some(compiled), Some(written)) if compiled != written => None,
            _ => Some(only),
        };
    }
    // Overloads of one arity: by the simple names of the parameter types,
    // where every one of them on both sides has one.
    let wanted = shape(member)?;
    let alike = |found: &MemberFacts| shape(found).as_ref() == Some(&wanted);
    let matching: Vec<&'w MemberFacts> = candidates
        .into_iter()
        .filter(|found| alike(found))
        .collect();
    match (
        rivals.iter().filter(|found| alike(found)).count(),
        matching.as_slice(),
    ) {
        (1, [only]) => Some(only),
        _ => None,
    }
}

/// A parameter type's simple name, the one thing a class file's resolved
/// type and a source's possibly unresolved one can both say — `None` for an
/// array or a wildcard, whose text the two readers do not write alike.
fn simple_name(written: &MemberType) -> Option<String> {
    let last = |name: &str| name.rsplit('.').next().unwrap_or(name).to_owned();
    match written {
        MemberType::Primitive(keyword) => Some(keyword.clone()),
        MemberType::Class(fqn) | MemberType::Parameterized(fqn, _) => Some(last(fqn)),
        MemberType::Variable(name) => Some(name.clone()),
        MemberType::Unresolved(text) => {
            let base = text.split('<').next().unwrap_or(text).trim();
            (!base.contains('[')
                && !base.contains("...")
                && !base.is_empty()
                && !base.contains('?'))
            .then(|| last(base))
        }
        MemberType::Void | MemberType::Extends(_) | MemberType::Super(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("javelin-classpath-tests")
            .join("sources-jar")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir
    }

    fn zip(path: &Path, entries: &[(&str, &str)]) {
        let mut writer = zip::ZipWriter::new(std::fs::File::create(path).expect("zip"));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, text) in entries {
            writer.start_file(*name, options).expect("entry");
            writer.write_all(text.as_bytes()).expect("write");
        }
        writer.finish().expect("finish");
    }

    fn member(name: &str, parameters: Vec<MemberType>) -> MemberFacts {
        MemberFacts {
            type_parameters: Vec::new(),
            doc_span: None,
            parameter_names: Vec::new(),
            name: name.to_owned(),
            kind: crate::DeclKind::Method,
            is_static: false,
            is_abstract: false,
            is_public: true,
            declared: MemberType::Void,
            parameters,
            variadic: false,
            name_start: 0,
            name_end: 0,
        }
    }

    fn compiled() -> TypeFacts {
        TypeFacts {
            is_public: true,
            constructors: Vec::new(),
            doc_span: None,
            fqn: "com.acme.Greeter".to_owned(),
            simple: "Greeter".to_owned(),
            kind: crate::DeclKind::Class,
            type_parameters: Vec::new(),
            supertype_arguments: Vec::new(),
            supertypes: vec!["java.lang.Object".to_owned()],
            supertypes_complete: true,
            members_complete: true,
            members: vec![
                member("greet", Vec::new()),
                member(
                    "pick",
                    vec![MemberType::Class("java.lang.String".to_owned())],
                ),
                member("pick", vec![MemberType::Primitive("int".to_owned())]),
                member("twice", vec![MemberType::Class("a.Foo".to_owned())]),
                member("twice", vec![MemberType::Class("b.Foo".to_owned())]),
            ],
            source: String::new(),
            name_start: 0,
            name_end: 0,
        }
    }

    const GREETER: &str = "package com.acme;\npublic class Greeter {\n    \
                           public void greet() {}\n    public void pick(String s) {}\n    \
                           public void pick(int i) {}\n    public void twice(a.Foo f) {}\n    \
                           public void twice(b.Foo f) {}\n}\n";

    #[test]
    fn a_sources_jar_sites_the_type_and_every_member_one_written_member_can_be() {
        let dir = scratch("graft");
        let jar = dir.join("acme-1.0-sources.jar");
        zip(&jar, &[("com/acme/Greeter.java", GREETER)]);
        let mut facts = vec![compiled()];
        assert_eq!(
            graft_sources(&mut facts, &jar, 1 << 20, &CancellationToken::new()).expect("read"),
            1
        );
        let ty = &facts[0];
        assert_eq!(
            ty.source,
            format!("{}!/com/acme/Greeter.java", jar.display())
        );
        assert_eq!(
            &GREETER[ty.name_start as usize..ty.name_end as usize],
            "Greeter"
        );
        let at = |index: usize| {
            let found = &ty.members[index];
            let line = GREETER[..found.name_start as usize].matches('\n').count();
            (
                GREETER[found.name_start as usize..found.name_end as usize].to_owned(),
                line,
            )
        };
        assert_eq!(at(0), ("greet".to_owned(), 2));
        assert_eq!(
            ty.members[1].parameter_names,
            vec!["s"],
            "names come with the span"
        );
        assert!(ty.members[3].parameter_names.is_empty(), "and only with it");
        assert_eq!(at(1), ("pick".to_owned(), 3), "by String");
        assert_eq!(at(2), ("pick".to_owned(), 4), "by int");
        // `a.Foo` and `b.Foo` are both `Foo` by simple name: neither is sited.
        assert_eq!(ty.members[3].name_end, 0);
        assert_eq!(ty.members[4].name_end, 0);
    }

    #[test]
    fn a_sources_jar_is_found_beside_a_maven_jar_and_in_a_gradle_cache() {
        let dir = scratch("find");
        let maven = dir.join("m2/acme/1.0");
        std::fs::create_dir_all(&maven).expect("m2");
        std::fs::write(maven.join("acme-1.0.jar"), "").expect("jar");
        assert_eq!(sources_jar_for(&maven.join("acme-1.0.jar")), None);
        std::fs::write(maven.join("acme-1.0-sources.jar"), "").expect("sources");
        assert_eq!(
            sources_jar_for(&maven.join("acme-1.0.jar")),
            Some(maven.join("acme-1.0-sources.jar"))
        );
        let gradle = dir.join("files-2.1/com.acme/acme/1.0");
        std::fs::create_dir_all(gradle.join("aaa")).expect("hash");
        std::fs::create_dir_all(gradle.join("bbb")).expect("hash");
        std::fs::write(gradle.join("aaa/acme-1.0.jar"), "").expect("jar");
        std::fs::write(gradle.join("bbb/acme-1.0-sources.jar"), "").expect("sources");
        assert_eq!(
            sources_jar_for(&gradle.join("aaa/acme-1.0.jar")),
            Some(gradle.join("bbb/acme-1.0-sources.jar"))
        );
    }

    /// Two builds' sources of one name, or a loose JAR's two levels up: which
    /// is this JAR's, nothing says.
    #[test]
    fn an_ambiguous_or_loose_sources_jar_is_not_taken() {
        let dir = scratch("find-ambiguous");
        let gradle = dir.join("files-2.1/com.acme/acme/1.0");
        for hash in ["aaa", "bbb", "ccc"] {
            std::fs::create_dir_all(gradle.join(hash)).expect("hash");
        }
        std::fs::write(gradle.join("aaa/acme-1.0.jar"), "").expect("jar");
        std::fs::write(gradle.join("bbb/acme-1.0-sources.jar"), "").expect("sources");
        std::fs::write(gradle.join("ccc/acme-1.0-sources.jar"), "").expect("sources");
        assert_eq!(sources_jar_for(&gradle.join("aaa/acme-1.0.jar")), None);
        let loose = dir.join("libs/old");
        std::fs::create_dir_all(&loose).expect("libs");
        std::fs::create_dir_all(dir.join("libs/new")).expect("libs");
        std::fs::write(loose.join("foo.jar"), "").expect("jar");
        std::fs::write(dir.join("libs/new/foo-sources.jar"), "").expect("sources");
        assert_eq!(sources_jar_for(&loose.join("foo.jar")), None);
    }

    /// Sources of another version: one member of the name each side, with
    /// other parameter types, is not the same member; and a type written
    /// twice outside `META-INF/` is not sited.
    #[test]
    fn sources_of_another_version_site_nothing_they_cannot_vouch_for() {
        let dir = scratch("stale");
        let jar = dir.join("acme-1.0-sources.jar");
        zip(
            &jar,
            &[(
                "com/acme/Greeter.java",
                "package com.acme;\npublic class Greeter {\n    public void greet(Request r) {}\n}\n",
            )],
        );
        let mut facts = vec![compiled()];
        facts[0].members = vec![member(
            "greet",
            vec![MemberType::Class("com.acme.Order".to_owned())],
        )];
        graft_sources(&mut facts, &jar, 1 << 20, &CancellationToken::new()).expect("read");
        assert_eq!(facts[0].members[0].name_end, 0, "Order is not Request");

        // Another version's reordered parameters, where an array leaves the
        // types unsayable: the span may be the method's, the names are not.
        let reordered = dir.join("reordered-sources.jar");
        zip(
            &reordered,
            &[(
                "com/acme/Greeter.java",
                "package com.acme;\npublic class Greeter {\n    public void put(String[] keys, int n) {}\n}\n",
            )],
        );
        let mut facts = vec![compiled()];
        facts[0].members = vec![member(
            "put",
            vec![
                MemberType::Primitive("int".to_owned()),
                MemberType::Unresolved("String[]".to_owned()),
            ],
        )];
        graft_sources(&mut facts, &reordered, 1 << 20, &CancellationToken::new()).expect("read");
        assert!(
            facts[0].members[0].parameter_names.is_empty(),
            "{:?}",
            facts[0].members[0]
        );

        let twice = dir.join("twice-sources.jar");
        zip(
            &twice,
            &[
                ("com/acme/Greeter.java", GREETER),
                ("com/acme/copy/Greeter.java", GREETER),
            ],
        );
        let mut facts = vec![compiled()];
        graft_sources(&mut facts, &twice, 1 << 20, &CancellationToken::new()).expect("read");
        assert!(
            facts[0].source.is_empty(),
            "written twice: {}",
            facts[0].source
        );

        let overlay = dir.join("overlay-sources.jar");
        zip(
            &overlay,
            &[
                ("com/acme/Greeter.java", GREETER),
                ("META-INF/versions/9/com/acme/Greeter.java", GREETER),
            ],
        );
        let mut facts = vec![compiled()];
        graft_sources(&mut facts, &overlay, 1 << 20, &CancellationToken::new()).expect("read");
        assert!(
            facts[0].source.ends_with("!/com/acme/Greeter.java"),
            "{}",
            facts[0].source
        );
    }
}
