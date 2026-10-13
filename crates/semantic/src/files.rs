//! Addressing more than one file.
//!
//! Everything else in this crate answers a question inside one file: a
//! `(&FileSyntax, &ScopeTree)` pair and a bare `Span` are enough when the
//! open buffer is the only thing that exists. Phase 3b breaks that — a
//! supertype can live in a sibling source file — so a declaration needs a
//! file to go with its span ([`DeclSite`]), and a query needs to carry more
//! than one file's views at once and know which of them it is currently
//! resolving names against ([`Files`]). [`FileId`] is what both are keyed on.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use javelin_parser::{FileSyntax, ScopeTree, Span};

/// Identifies a file this crate can address, open or not.
///
/// A re-export rather than a newtype: `DocumentId` already is
/// `Clone + Eq + Hash + Ord` over a refcounted URI, and a wrapper here would
/// buy a name and cost a conversion at every boundary. Named `FileId`
/// because semantic addresses files the document store never opened.
pub use javelin_document::DocumentId as FileId;

/// A declaration's address: which file, and the name span within it.
///
/// The span is the one `FileSyntax::declaration_at` takes — a declaration's
/// *name*, not its whole extent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclSite {
    pub file: FileId,
    pub span: Span,
}

impl DeclSite {
    pub fn new(file: FileId, span: Span) -> Self {
        Self { file, span }
    }
}

/// The set of files one query may reach, and which of them the query is
/// *in* right now.
///
/// The current file's views are held inline rather than found by scanning,
/// so `current_syntax`/`current_scopes` are infallible: there is no index to
/// be out of bounds and no absent entry to `expect` past on the request
/// path. `others` holds every other reachable file; `add` and `viewing` are
/// the only ways to change either part, and both keep the invariant that a
/// `FileId` never appears in both places at once — so the set never holds
/// one file twice and disagrees with itself about its views.
#[derive(Debug, Clone)]
pub struct Files<'a> {
    current: FileId,
    syntax: &'a FileSyntax,
    scopes: &'a ScopeTree,
    others: Vec<(FileId, &'a FileSyntax, &'a ScopeTree)>,
    /// What `infer::field_owner` reads off each enclosing type, by the type's
    /// file and name offset: every bare name a query asks about reads the
    /// same few types, and walking a god-class per name is quadratic.
    pub(crate) fields: Rc<RefCell<HashMap<(FileId, u32), Rc<crate::infer::FieldFrame<'a>>>>>,
}

impl<'a> Files<'a> {
    /// A set with one reachable file, which is also the current one.
    pub fn single(file: FileId, syntax: &'a FileSyntax, scopes: &'a ScopeTree) -> Self {
        Self {
            current: file,
            syntax,
            scopes,
            others: Vec::new(),
            fields: Rc::default(),
        }
    }

    /// Makes `file` reachable with these views, replacing its entry if the
    /// set already held one — including when `file` is the current file.
    pub fn add(&mut self, file: FileId, syntax: &'a FileSyntax, scopes: &'a ScopeTree) {
        self.fields = Rc::default();
        if file == self.current {
            self.syntax = syntax;
            self.scopes = scopes;
            return;
        }
        match self.others.iter_mut().find(|(held, ..)| *held == file) {
            Some(entry) => *entry = (file, syntax, scopes),
            None => self.others.push((file, syntax, scopes)),
        }
    }

    /// The file the query is currently resolving names against.
    pub fn current(&self) -> &FileId {
        &self.current
    }

    /// `file`'s syntax, if the set can reach it.
    pub fn syntax(&self, file: &FileId) -> Option<&'a FileSyntax> {
        if *file == self.current {
            return Some(self.syntax);
        }
        self.others
            .iter()
            .find(|(held, ..)| held == file)
            .map(|(_, syntax, _)| *syntax)
    }

    /// The current file's syntax. Infallible: see the struct doc.
    pub fn current_syntax(&self) -> &'a FileSyntax {
        self.syntax
    }

    /// The current file's scopes. Infallible: see the struct doc.
    pub fn current_scopes(&self) -> &'a ScopeTree {
        self.scopes
    }

    /// The same reachable files, with `current` moved to `file` — or `None`
    /// when the set does not hold `file`.
    ///
    /// This is how a supertype walk that follows a name into another file
    /// resolves *that* file's written names against that file's own imports
    /// and scopes, rather than the file the walk started in.
    ///
    /// ponytail: rebuilds the set rather than moving a cursor within it, so
    /// both supertype walks clone `others` once per frame. Free while
    /// `others` is empty, which is every set the server builds today, and one
    /// allocation per frame the moment a workspace index makes a real set. The
    /// upgrade is to hold the current file as an index into one vector of
    /// entries, so `viewing` is a `Copy` of that index and the entries are
    /// never touched — but only if the index cannot be wrong. An index the
    /// caller can supply is an index that can be out of bounds, which puts an
    /// `expect` back on the request path in `current_syntax`; keeping that
    /// infallible means the index stays private and is only ever produced by a
    /// `position` that already found the entry.
    pub fn viewing(&self, file: &FileId) -> Option<Files<'a>> {
        if *file == self.current {
            return Some(self.clone());
        }
        let index = self.others.iter().position(|(held, ..)| held == file)?;
        let mut others = self.others.clone();
        let (new_current, new_syntax, new_scopes) = others.remove(index);
        others.push((self.current.clone(), self.syntax, self.scopes));
        Some(Files {
            current: new_current,
            syntax: new_syntax,
            scopes: new_scopes,
            others,
            fields: Rc::clone(&self.fields),
        })
    }
}

#[cfg(test)]
mod tests {
    use javelin_document::TextDocument;
    use javelin_parser::JavaParser;

    use super::*;

    /// Builds the `(FileSyntax, ScopeTree)` pair a `Files` entry needs, for a
    /// file named `name` (e.g. `"A"` for `file:///A.java`) with `source`.
    fn views(name: &str, source: &str) -> (FileSyntax, ScopeTree) {
        let document = TextDocument::new(file(name), 1, source);
        let mut parser = JavaParser::new().expect("grammar loads");
        let tree = parser.parse(&document).expect("parses");
        let syntax = FileSyntax::build(&document, &tree);
        let scopes = ScopeTree::build(&document, &tree);
        (syntax, scopes)
    }

    fn file(name: &str) -> FileId {
        FileId::new(format!("file:///{name}.java"))
    }

    #[test]
    fn a_single_file_set_answers_for_its_own_file_and_none_for_any_other() {
        let (syn, scopes) = views("A", "class A {}");
        let a = file("A");
        let set = Files::single(a.clone(), &syn, &scopes);

        assert!(set.syntax(&a).is_some());
        assert_eq!(set.current(), &a);

        let b = file("B");
        assert!(set.syntax(&b).is_none());
        assert!(set.viewing(&b).is_none());
    }

    #[test]
    fn add_makes_a_second_file_reachable_without_disturbing_the_first() {
        let (syn_a, scopes_a) = views("A", "class A {}");
        let (syn_b, scopes_b) = views("B", "class B {}");
        let a = file("A");
        let b = file("B");

        let mut set = Files::single(a.clone(), &syn_a, &scopes_a);
        set.add(b.clone(), &syn_b, &scopes_b);

        assert!(std::ptr::eq(set.syntax(&a).expect("a is held"), &syn_a));
        assert!(std::ptr::eq(set.current_scopes(), &scopes_a));
        assert!(std::ptr::eq(set.syntax(&b).expect("b is held"), &syn_b));
        assert!(std::ptr::eq(
            set.viewing(&b).expect("b is held").current_scopes(),
            &scopes_b
        ));
        assert_eq!(set.current(), &a);
    }

    #[test]
    fn add_of_a_file_already_held_replaces_rather_than_duplicates() {
        let (syn_a, scopes_a) = views("A", "class A {}");
        let (syn_b1, scopes_b1) = views("B", "class B {}");
        let (syn_b2, scopes_b2) = views("B", "class B { int x; }");
        let b = file("B");

        let mut set = Files::single(file("A"), &syn_a, &scopes_a);
        set.add(b.clone(), &syn_b1, &scopes_b1);
        set.add(b.clone(), &syn_b2, &scopes_b2);

        // Replaced, not duplicated: still resolves, and to the newer view.
        assert!(std::ptr::eq(set.syntax(&b).expect("b is held"), &syn_b2));
        assert!(std::ptr::eq(
            set.viewing(&b).expect("b is held").current_scopes(),
            &scopes_b2
        ));
    }

    #[test]
    fn add_of_the_current_file_replaces_its_inline_view() {
        let (syn_a1, scopes_a1) = views("A", "class A {}");
        let (syn_a2, scopes_a2) = views("A", "class A { int x; }");
        let a = file("A");

        let mut set = Files::single(a.clone(), &syn_a1, &scopes_a1);
        set.add(a.clone(), &syn_a2, &scopes_a2);

        assert!(std::ptr::eq(set.current_syntax(), &syn_a2));
        assert!(std::ptr::eq(set.syntax(&a).unwrap(), &syn_a2));
    }

    #[test]
    fn viewing_an_unheld_file_answers_none() {
        let (syn, scopes) = views("A", "class A {}");
        let set = Files::single(file("A"), &syn, &scopes);

        assert!(set.viewing(&file("Nowhere")).is_none());
    }

    #[test]
    fn viewing_a_held_file_keeps_both_files_reachable_and_moves_current() {
        let (syn_a, scopes_a) = views("A", "class A {}");
        let (syn_b, scopes_b) = views("B", "class B {}");
        let a = file("A");
        let b = file("B");

        let mut set = Files::single(a.clone(), &syn_a, &scopes_a);
        set.add(b.clone(), &syn_b, &scopes_b);

        let moved = set.viewing(&b).expect("b is held");

        assert_eq!(moved.current(), &b);
        assert!(std::ptr::eq(moved.current_syntax(), &syn_b));
        assert!(std::ptr::eq(moved.current_scopes(), &scopes_b));
        // Both files are still reachable from the moved view.
        assert!(std::ptr::eq(moved.syntax(&a).expect("a is held"), &syn_a));
        assert!(std::ptr::eq(
            moved.viewing(&a).expect("a is held").current_scopes(),
            &scopes_a
        ));
        assert!(std::ptr::eq(moved.syntax(&b).expect("b is held"), &syn_b));

        // The set `viewing` was called on is untouched.
        assert_eq!(set.current(), &a);
    }
}
