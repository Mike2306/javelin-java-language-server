//! Which dependency JARs a file's own module compiles against.
//!
//! The build tool says what each module claims and what it compiles against
//! (`javelin_build::Module`); this keeps that beside which JARs hold each
//! dependency type, so a window onto the world can be narrowed to one
//! module's JARs. Only the type diagnostic narrows: a name another module's
//! JAR holds is unknown to this file, and navigation keeps reading the union.
//!
//! ponytail: only JARs are scoped. The workspace's own sources are visible
//! to every module, so a type a sibling module declares that this one does
//! not depend on is not flagged; a module's source dependencies, from the
//! same build answer, would narrow them too. And a Maven module is one
//! module, its main and test sources claiming one classpath with test
//! scope in it, so a test library named from `src/main` is not flagged; a
//! scope per source root is a second `build-classpath` run without test
//! scope.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use javelin_build::Module;
use javelin_classpath::TypeFacts;

/// The build's modules, and every dependency type's JARs.
#[derive(Debug)]
pub(crate) struct Modules {
    /// Each dependency fqn's JARs, as indices into the build's classpath.
    holders: HashMap<String, Vec<usize>>,
    modules: Vec<Module>,
}

/// One module's JARs — a window onto the world sees a dependency type
/// through it only when one of them holds the type.
#[derive(Debug)]
pub(crate) struct Scope {
    modules: Arc<Modules>,
    jars: HashSet<usize>,
}

impl Modules {
    /// How many modules the build tool answered for.
    pub(crate) fn len(&self) -> usize {
        self.modules.len()
    }

    /// `jars` is each classpath JAR's facts, by its index in the build's
    /// classpath.
    pub(crate) fn new<'a>(
        jars: impl IntoIterator<Item = (usize, &'a [TypeFacts])>,
        mut modules: Vec<Module>,
    ) -> Self {
        let mut holders: HashMap<String, Vec<usize>> = HashMap::new();
        for (jar, facts) in jars {
            for fact in facts {
                holders.entry(fact.fqn.clone()).or_default().push(jar);
            }
        }
        for module in &mut modules {
            for source in &mut module.sources {
                *source = real(source);
            }
        }
        Self { holders, modules }
    }

    /// The JARs the file at `path` compiles against: its module's — the one
    /// claiming the deepest directory above it, or the union where several
    /// claim it — and `None` where no module does, or one the tool did not
    /// answer for, which is a file nothing here can judge.
    pub(crate) fn scope_of(self: &Arc<Self>, path: &Path) -> Option<Arc<Scope>> {
        let path = &real(path);
        let mut deepest = 0;
        let mut claiming: Vec<&Module> = Vec::new();
        for module in &self.modules {
            let Some(depth) = module
                .sources
                .iter()
                .filter(|source| path.starts_with(source))
                .map(|source| source.components().count())
                .max()
            else {
                continue;
            };
            if depth > deepest {
                deepest = depth;
                claiming.clear();
            }
            if depth == deepest {
                claiming.push(module);
            }
        }
        if claiming.is_empty() {
            return None;
        }
        let mut jars = HashSet::new();
        for module in claiming {
            jars.extend(module.jars.as_ref()?);
        }
        Some(Arc::new(Scope {
            modules: Arc::clone(self),
            jars,
        }))
    }
}

/// `path` with every symlink resolved — Gradle prints its source directories
/// so, and a client names a file by the path it opened — through its deepest
/// ancestor on disk, for a file or a directory not written yet.
fn real(path: &Path) -> PathBuf {
    path.ancestors()
        .find_map(|ancestor| {
            let rest = path.strip_prefix(ancestor).ok()?;
            Some(ancestor.canonicalize().ok()?.join(rest))
        })
        .unwrap_or_else(|| path.to_path_buf())
}

impl Scope {
    /// Whether a type the world holds as `fqn` is on this module's
    /// classpath: one no dependency JAR holds is the JDK's, and always is.
    pub(crate) fn sees(&self, fqn: &str) -> bool {
        self.modules
            .holders
            .get(fqn)
            .is_none_or(|jars| jars.iter().any(|jar| self.jars.contains(jar)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(fqn: &str) -> TypeFacts {
        crate::classpath::fact(fqn)
    }

    fn module(sources: &[&str], jars: Option<Vec<usize>>) -> Module {
        Module {
            sources: sources.iter().map(PathBuf::from).collect(),
            jars,
        }
    }

    fn modules() -> Arc<Modules> {
        let guava = [fact("com.google.Guava")];
        let mockito = [fact("org.mockito.Mockito"), fact("com.google.Guava")];
        Arc::new(Modules::new(
            [(0, &guava[..]), (1, &mockito[..])],
            vec![
                module(&["/w"], Some(vec![])),
                module(&["/w/core/src/main/java"], Some(vec![0])),
                module(&["/w/web"], Some(vec![1])),
                module(&["/w/shared"], Some(vec![0])),
                module(&["/w/shared", "/w/other"], Some(vec![1])),
                module(&["/w/standalone"], None),
            ],
        ))
    }

    #[test]
    fn a_file_sees_the_jars_of_the_deepest_module_claiming_it() {
        let modules = modules();
        let core = modules
            .scope_of(Path::new("/w/core/src/main/java/a/A.java"))
            .expect("claimed");
        assert!(core.sees("com.google.Guava"));
        assert!(!core.sees("org.mockito.Mockito"), "another module's JAR");
        assert!(core.sees("java.lang.String"), "no JAR holds it: the JDK's");
        let web = modules
            .scope_of(Path::new("/w/web/src/W.java"))
            .expect("claimed");
        assert!(web.sees("org.mockito.Mockito"));
        assert!(web.sees("com.google.Guava"), "a second JAR holding it");
        let root = modules
            .scope_of(Path::new("/w/src/R.java"))
            .expect("claimed");
        assert!(!root.sees("com.google.Guava"));
    }

    #[test]
    fn a_directory_two_modules_claim_sees_both() {
        let shared = modules()
            .scope_of(Path::new("/w/shared/S.java"))
            .expect("claimed");
        assert!(shared.sees("org.mockito.Mockito"));
    }

    /// Gradle prints `/private/tmp/w/src` for a root the client opened as
    /// `/tmp/w`; the two spellings are one directory.
    #[cfg(unix)]
    #[test]
    fn a_module_claims_its_files_through_a_symlink_either_way() {
        let base = std::env::temp_dir().join(format!("javelin-modules-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("real/src")).expect("src");
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).expect("link");
        let guava = [fact("com.google.Guava")];
        for (claimed, opened) in [("real", "link"), ("link", "real")] {
            let modules = Arc::new(Modules::new(
                [(0, &guava[..])],
                vec![Module {
                    sources: vec![base.join(claimed).join("src")],
                    jars: Some(vec![0]),
                }],
            ));
            for file in ["A.java", "Unsaved.java"] {
                if file == "A.java" {
                    std::fs::write(base.join("real/src/A.java"), "class A {}").expect("file");
                }
                let scope = modules.scope_of(&base.join(opened).join("src").join(file));
                assert!(
                    scope.is_some_and(|scope| scope.sees("com.google.Guava")),
                    "{file}"
                );
            }
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_file_no_module_answers_for_has_no_scope() {
        let modules = modules();
        assert!(
            modules
                .scope_of(Path::new("/w/standalone/src/S.java"))
                .is_none()
        );
        assert!(modules.scope_of(Path::new("/elsewhere/E.java")).is_none());
    }
}
