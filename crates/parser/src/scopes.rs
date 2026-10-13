//! Which names a file binds, and over which regions of it they are visible.
//!
//! This is the answer to "what does the identifier under the cursor refer to?"
//! for everything declared in the file the user is looking at — and only that.
//! It is *syntax*: a binding records that a name is bound over a byte range. It
//! never records what a type name refers to. A field inherited from a
//! superclass is invisible here, and so is anything an import brings in.
//! Resolving those needs `javelin-semantic`, which does not exist yet.
//!
//! Two rules carry the whole thing:
//!
//! * **Shadowing.** Resolution walks outward from the innermost scope holding
//!   the offset and takes the first binding of the name it meets.
//! * **Java's visibility asymmetry.** A local is visible from its own
//!   declaration to the end of its scope; a field is visible throughout the type
//!   body no matter where in it the field was written.
//!
//! Built from the Tree-sitter tree rather than from [`crate::FileSyntax`],
//! because that view deliberately flattens a method body into one list of
//! declarations — it has no block structure to hang scopes off.

use std::collections::{HashMap, HashSet};

use javelin_document::{Range, TextDocument};
use tree_sitter::Node;

use crate::SyntaxTree;
use crate::syntax::{MAX_DEPTH, Span, named_child};

/// What kind of region a [`Scope`] covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ScopeKind {
    /// The whole file: its top-level type declarations live here.
    CompilationUnit,
    /// A class, interface, enum, record, annotation type, or the body of an
    /// anonymous class or an enum constant.
    Type,
    /// A method or constructor, covering its type parameters and parameters.
    Method,
    /// `{ … }` — a method body, an initialiser block, a `switch` block, or a
    /// block written on its own.
    Block,
    /// A `for` or enhanced-`for` statement: header and body, nothing outside.
    For,
    /// A `catch` clause: the exception parameter and the handler body.
    Catch,
    /// `try (…)`: the resource list and the `try` block. Deliberately stops
    /// before the handlers, which cannot see the resources.
    TryResources,
    /// A lambda: its parameters and its body. Nested inside the scope it was
    /// written in, so a lambda body sees everything around it.
    Lambda,
    /// One `case` arm, arrow or colon form, holding its pattern bindings.
    SwitchCase,
}

/// What a [`Binding`] binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BindingKind {
    /// A local variable, including a `try`-with-resources resource.
    LocalVariable,
    /// A method, constructor, lambda or `catch` parameter.
    Parameter,
    /// A name introduced by a pattern: `instanceof`, a `case` label, or a
    /// component of a record deconstruction.
    PatternBinding,
    /// A field, a record component or an enum constant.
    Field,
    TypeParameter,
    /// A class, interface, enum, record or annotation type.
    TypeDeclaration,
}

impl BindingKind {
    /// Whether the name lands in Java's type namespace rather than its variable
    /// namespace. The two do not shadow each other.
    fn is_type(self) -> bool {
        matches!(
            self,
            BindingKind::TypeParameter | BindingKind::TypeDeclaration
        )
    }
}

/// One name, bound over one byte range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    pub name: String,
    pub kind: BindingKind,
    /// The declaration the name came from, e.g. all of `int x = 1;`.
    pub span: Span,
    /// Just the name, which is where go-to-definition lands.
    pub name_span: Span,
    /// First byte at which the name resolves. For a local this is its own
    /// declaration; for a field it is the start of the type that declares it.
    pub visible_from: u32,
    /// The end of the scope that owns the binding. A cursor sitting exactly
    /// here still resolves the name.
    pub visible_to: u32,
}

impl Binding {
    pub fn is_visible_at(&self, offset: u32) -> bool {
        self.visible_from <= offset && offset <= self.visible_to
    }
}

/// A region of the file, and the names it introduces.
///
/// Bytes rather than an LSP range: nothing on the wire names a scope, and a
/// scope is created for every block in the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub kind: ScopeKind,
    pub start_byte: u32,
    pub end_byte: u32,
    parent: Option<usize>,
    children: Vec<usize>,
    bindings: Vec<usize>,
}

impl Scope {
    fn contains(&self, offset: u32) -> bool {
        self.start_byte <= offset && offset <= self.end_byte
    }
}

/// The scope tree of one file, tied to the document version it describes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeTree {
    version: i32,
    /// Index 0 is always the compilation unit; every other scope has a parent.
    scopes: Vec<Scope>,
    bindings: Vec<Binding>,
    /// Every binding of a name, as `(scope, binding)` pairs sorted by scope
    /// and then by binding — so a scope's bindings of one name are one
    /// contiguous run, in the order they were written.
    ///
    /// What makes a lookup cost the handful of same-named bindings rather
    /// than every binding of the scope: a generated class body with twenty
    /// thousand fields made a scan of that scope per identifier, and a walk
    /// that resolves every identifier in the file quadratic in its size.
    by_name: HashMap<Box<str>, Vec<(u32, u32)>>,
}

impl ScopeTree {
    /// Build the scope tree of a parsed file.
    ///
    /// Never fails. A file mid-keystroke is full of `ERROR` and `MISSING` nodes;
    /// whatever bound correctly is still worth having, because the user is
    /// looking at that buffer right now.
    pub fn build(document: &TextDocument, tree: &SyntaxTree) -> Self {
        let root = tree.root();
        let mut builder = Builder {
            document,
            tree: ScopeTree {
                version: tree.version(),
                scopes: Vec::new(),
                bindings: Vec::new(),
                by_name: HashMap::new(),
            },
        };
        // The root node stops at the last token, but a cursor can sit past it.
        let end = root.end_byte().max(document.text().len()) as u32;
        let unit = builder.push(
            ScopeKind::CompilationUnit,
            root.start_byte() as u32,
            end,
            None,
        );
        builder.walk_children(root, unit, 0);
        let mut tree = builder.tree;
        for pairs in tree.by_name.values_mut() {
            pairs.sort_unstable();
        }
        tree
    }

    pub fn version(&self) -> i32 {
        self.version
    }

    /// The innermost scope containing `offset`.
    pub fn scope_at(&self, offset: u32) -> &Scope {
        &self.scopes[self.scope_index_at(offset)]
    }

    /// The binding `name` refers to at `offset`, whichever namespace it is in,
    /// or `None` if nothing visible there is called that.
    ///
    /// This is the shadowing rule: innermost scope first, then outward.
    ///
    /// Namespace-blind, so `class Value {} int Value;` is ambiguous here. Use
    /// [`resolve_type`](Self::resolve_type) or
    /// [`resolve_value`](Self::resolve_value) wherever the position already
    /// says which of the two Java would read — a type reference or an
    /// expression — because Java keeps the two namespaces apart and one does
    /// not shadow the other.
    pub fn resolve(&self, offset: u32, name: &str) -> Option<&Binding> {
        self.find(offset, name, |_| true)
    }

    /// The type `name` refers to at `offset`: a type declaration or a type
    /// parameter, never a variable that happens to share the name.
    pub fn resolve_type(&self, offset: u32, name: &str) -> Option<&Binding> {
        self.find(offset, name, |kind| kind.is_type())
    }

    /// The variable `name` refers to at `offset`: a local, parameter, field or
    /// pattern binding, never a type that happens to share the name.
    pub fn resolve_value(&self, offset: u32, name: &str) -> Option<&Binding> {
        self.find(offset, name, |kind| !kind.is_type())
    }

    fn find(
        &self,
        offset: u32,
        name: &str,
        namespace: impl Fn(BindingKind) -> bool,
    ) -> Option<&Binding> {
        let pairs = self.by_name.get(name)?;
        self.enclosing(offset).find_map(|scope| {
            let scope = scope as u32;
            let start = pairs.partition_point(|&(owner, _)| owner < scope);
            let end = pairs.partition_point(|&(owner, _)| owner <= scope);
            // Reversed so that in a file that (illegally) binds one name twice
            // in a scope, the declaration nearer the cursor wins.
            pairs[start..end]
                .iter()
                .rev()
                .map(|&(_, binding)| &self.bindings[binding as usize])
                .find(|binding| namespace(binding.kind) && binding.is_visible_at(offset))
        })
    }

    /// Every binding visible at `offset`, innermost scope first and in source
    /// order within a scope.
    ///
    /// Shadowed bindings are left out: they are, by definition, not visible.
    pub fn visible_at(&self, offset: u32) -> Vec<&Binding> {
        let mut seen: HashSet<(&str, bool)> = HashSet::new();
        let mut visible = Vec::new();
        for scope in self.enclosing(offset) {
            for &binding in &self.scopes[scope].bindings {
                let binding = &self.bindings[binding];
                if binding.is_visible_at(offset)
                    && seen.insert((binding.name.as_str(), binding.kind.is_type()))
                {
                    visible.push(binding);
                }
            }
        }
        visible
    }

    /// The scopes containing `offset`, innermost first.
    fn enclosing(&self, offset: u32) -> impl Iterator<Item = usize> {
        let mut next = Some(self.scope_index_at(offset));
        std::iter::from_fn(move || {
            let current = next?;
            next = self.scopes[current].parent;
            Some(current)
        })
    }

    fn scope_index_at(&self, offset: u32) -> usize {
        let mut current = 0;
        // A scope's children are pushed in source order and never overlap —
        // two siblings share at most a boundary, `}` meeting `{` — so the
        // only child that can contain `offset` is the last one starting at or
        // before it. Taking the last also settles that shared boundary: the
        // cursor is taken to be in the scope it is about to enter.
        loop {
            let children = &self.scopes[current].children;
            let after = children.partition_point(|&child| self.scopes[child].start_byte <= offset);
            match after.checked_sub(1).map(|last| children[last]) {
                Some(child) if self.scopes[child].contains(offset) => current = child,
                _ => return current,
            }
        }
    }
}

/// When a binding starts being visible.
#[derive(Debug, Clone, Copy)]
enum Visibility {
    /// From the start of the owning scope, wherever in it the declaration was
    /// written. Fields, type parameters and member types work this way.
    WholeScope,
    /// From the name itself. Locals, parameters and pattern bindings work this
    /// way: Java requires them to be declared before use.
    FromName,
}

struct Builder<'a> {
    document: &'a TextDocument,
    tree: ScopeTree,
}

impl Builder<'_> {
    fn push(&mut self, kind: ScopeKind, start: u32, end: u32, parent: Option<usize>) -> usize {
        let index = self.tree.scopes.len();
        self.tree.scopes.push(Scope {
            kind,
            start_byte: start,
            end_byte: end.max(start),
            parent,
            children: Vec::new(),
            bindings: Vec::new(),
        });
        if let Some(parent) = parent {
            debug_assert!(
                self.tree.scopes[parent]
                    .children
                    .last()
                    .is_none_or(|&sibling| self.tree.scopes[sibling].end_byte <= start),
                "scope_index_at's binary search needs siblings in order and apart"
            );
            self.tree.scopes[parent].children.push(index);
        }
        index
    }

    fn scope(&mut self, kind: ScopeKind, node: Node<'_>, parent: usize) -> usize {
        self.push(
            kind,
            node.start_byte() as u32,
            node.end_byte() as u32,
            Some(parent),
        )
    }

    fn bind(
        &mut self,
        scope: usize,
        kind: BindingKind,
        declaration: Node<'_>,
        name: Node<'_>,
        visibility: Visibility,
    ) {
        let name_span = self.span(name);
        let visible_from = match visibility {
            Visibility::WholeScope => self.tree.scopes[scope].start_byte,
            Visibility::FromName => name_span.start_byte,
        };
        let index = self.tree.bindings.len();
        self.tree.bindings.push(Binding {
            name: self.text(name).to_string(),
            kind,
            span: self.span(declaration),
            name_span,
            visible_from,
            visible_to: self.tree.scopes[scope].end_byte,
        });
        self.tree.scopes[scope].bindings.push(index);
        let name = self.tree.bindings[index].name.as_str();
        match self.tree.by_name.get_mut(name) {
            Some(pairs) => pairs.push((scope as u32, index as u32)),
            None => {
                self.tree
                    .by_name
                    .insert(name.into(), vec![(scope as u32, index as u32)]);
            }
        }
    }

    fn walk_children(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        let mut cursor = node.walk();
        for child in node.named_children(&mut cursor) {
            self.walk(child, scope, depth + 1);
        }
    }

    fn walk(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        match node.kind() {
            "class_declaration"
            | "interface_declaration"
            | "enum_declaration"
            | "record_declaration"
            | "annotation_type_declaration" => self.type_declaration(node, scope, depth),
            "object_creation_expression" => self.object_creation(node, scope, depth),
            "enum_constant" => self.enum_constant(node, scope, depth),
            "method_declaration"
            | "constructor_declaration"
            | "compact_constructor_declaration" => self.callable(node, scope, depth),
            "lambda_expression" => self.lambda(node, scope, depth),
            // A static initialiser wraps a block, which becomes the scope; an
            // instance initialiser *is* one.
            "block" | "constructor_body" | "switch_block" => {
                let inner = self.scope(ScopeKind::Block, node, scope);
                self.walk_children(node, inner, depth);
            }
            "for_statement" => {
                let inner = self.scope(ScopeKind::For, node, scope);
                self.walk_children(node, inner, depth);
            }
            "enhanced_for_statement" => self.enhanced_for(node, scope, depth),
            "catch_clause" => self.catch_clause(node, scope, depth),
            "try_with_resources_statement" => self.try_with_resources(node, scope, depth),
            "switch_rule" => {
                let inner = self.scope(ScopeKind::SwitchCase, node, scope);
                self.walk_children(node, inner, depth);
            }
            // A colon-form group's patterns are its own, but a local it
            // declares is in scope for the rest of the switch block (JLS 6.3),
            // so it is bound there and its initialiser walked in the group.
            "switch_block_statement_group" => {
                let inner = self.scope(ScopeKind::SwitchCase, node, scope);
                let mut cursor = node.walk();
                for child in node.named_children(&mut cursor) {
                    if child.kind() == "local_variable_declaration" {
                        self.variables(
                            child,
                            (scope, inner),
                            BindingKind::LocalVariable,
                            Visibility::FromName,
                            depth + 1,
                        );
                    } else {
                        self.walk(child, inner, depth + 1);
                    }
                }
            }
            "local_variable_declaration" => {
                self.variables(
                    node,
                    (scope, scope),
                    BindingKind::LocalVariable,
                    Visibility::FromName,
                    depth,
                );
            }
            "field_declaration" | "constant_declaration" => {
                self.variables(
                    node,
                    (scope, scope),
                    BindingKind::Field,
                    Visibility::WholeScope,
                    depth,
                );
            }
            "instanceof_expression" => self.instanceof(node, scope, depth),
            "pattern" | "type_pattern" | "record_pattern" => self.pattern(node, scope, depth),
            _ => self.walk_children(node, scope, depth),
        }
    }

    fn type_declaration(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        if let Some(name) = node.child_by_field_name("name") {
            // A member type is visible throughout its enclosing type; a class
            // declared inside a method is a local and follows the local rule.
            let visibility = match self.tree.scopes[scope].kind {
                ScopeKind::CompilationUnit | ScopeKind::Type => Visibility::WholeScope,
                _ => Visibility::FromName,
            };
            self.bind(scope, BindingKind::TypeDeclaration, node, name, visibility);
        }
        // The scope covers the whole declaration so that a type parameter is
        // visible in the `extends` clause that may use it.
        let inner = self.scope(ScopeKind::Type, node, scope);
        self.type_parameters(node, inner);
        // Only a record has these, and they are fields as much as parameters.
        if let Some(header) = node.child_by_field_name("parameters") {
            self.formal_parameters(header, inner, BindingKind::Field);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk_children(body, inner, depth + 1);
        }
    }

    fn object_creation(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        // The arguments are evaluated in the enclosing scope, not in the body.
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.walk_children(arguments, scope, depth + 1);
        }
        if let Some(body) = named_child(node, "class_body") {
            let inner = self.scope(ScopeKind::Type, body, scope);
            self.walk_children(body, inner, depth + 1);
        }
    }

    fn enum_constant(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        if let Some(name) = node.child_by_field_name("name") {
            self.bind(
                scope,
                BindingKind::Field,
                node,
                name,
                Visibility::WholeScope,
            );
        }
        if let Some(arguments) = node.child_by_field_name("arguments") {
            self.walk_children(arguments, scope, depth + 1);
        }
        if let Some(body) = node.child_by_field_name("body") {
            let inner = self.scope(ScopeKind::Type, body, scope);
            self.walk_children(body, inner, depth + 1);
        }
    }

    fn callable(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        let inner = self.scope(ScopeKind::Method, node, scope);
        self.type_parameters(node, inner);
        if let Some(parameters) = node.child_by_field_name("parameters") {
            self.formal_parameters(parameters, inner, BindingKind::Parameter);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, inner, depth + 1);
        }
    }

    fn lambda(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        let inner = self.scope(ScopeKind::Lambda, node, scope);
        if let Some(parameters) = node.child_by_field_name("parameters") {
            match parameters.kind() {
                // `x -> …`
                "identifier" => self.bind(
                    inner,
                    BindingKind::Parameter,
                    parameters,
                    parameters,
                    Visibility::WholeScope,
                ),
                // `(x, y) -> …`
                "inferred_parameters" => {
                    let mut cursor = parameters.walk();
                    let names: Vec<_> = parameters
                        .named_children(&mut cursor)
                        .filter(|child| child.kind() == "identifier")
                        .collect();
                    for name in names {
                        self.bind(
                            inner,
                            BindingKind::Parameter,
                            name,
                            name,
                            Visibility::WholeScope,
                        );
                    }
                }
                // `(int x) -> …`
                _ => self.formal_parameters(parameters, inner, BindingKind::Parameter),
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, inner, depth + 1);
        }
    }

    fn enhanced_for(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        let inner = self.scope(ScopeKind::For, node, scope);
        // No single node covers `String s`, so the name stands in for the
        // declaration — the same compromise `crate::syntax` makes.
        if let Some(name) = node.child_by_field_name("name") {
            self.bind(
                inner,
                BindingKind::LocalVariable,
                name,
                name,
                Visibility::FromName,
            );
        }
        if let Some(value) = node.child_by_field_name("value") {
            self.walk(value, inner, depth + 1);
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, inner, depth + 1);
        }
    }

    fn catch_clause(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        let inner = self.scope(ScopeKind::Catch, node, scope);
        if let Some(parameter) = named_child(node, "catch_formal_parameter")
            && let Some(name) = parameter.child_by_field_name("name")
        {
            self.bind(
                inner,
                BindingKind::Parameter,
                parameter,
                name,
                Visibility::WholeScope,
            );
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.walk(body, inner, depth + 1);
        }
    }

    fn try_with_resources(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        let body = node.child_by_field_name("body");
        // A resource is out of scope in the handlers, so the scope ends where
        // the `try` block does rather than where the statement does.
        let end = body.map_or(node.end_byte(), |body| body.end_byte()) as u32;
        let inner = self.push(
            ScopeKind::TryResources,
            node.start_byte() as u32,
            end,
            Some(scope),
        );
        if let Some(resources) = node.child_by_field_name("resources") {
            let mut cursor = resources.walk();
            let entries: Vec<_> = resources
                .named_children(&mut cursor)
                .filter(|child| child.kind() == "resource")
                .collect();
            for resource in entries {
                match resource.child_by_field_name("name") {
                    Some(name) => {
                        self.bind(
                            inner,
                            BindingKind::LocalVariable,
                            resource,
                            name,
                            Visibility::FromName,
                        );
                        if let Some(value) = resource.child_by_field_name("value") {
                            self.walk(value, inner, depth + 1);
                        }
                    }
                    // `try (existing)` names a variable rather than declaring one.
                    None => self.walk_children(resource, inner, depth + 1),
                }
            }
        }
        if let Some(body) = body {
            self.walk(body, inner, depth + 1);
        }
        // The handlers belong to the enclosing scope.
        let mut cursor = node.walk();
        let handlers: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| matches!(child.kind(), "catch_clause" | "finally_clause"))
            .collect();
        for handler in handlers {
            self.walk(handler, scope, depth + 1);
        }
    }

    fn instanceof(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        // ponytail: a pattern binding is bound to the end of the enclosing
        // scope, where Java scopes it by definite assignment — `if (!(o
        // instanceof String s)) return;` binds `s` after the `if`, and
        // `if (o instanceof String s)` does not. Over-permissive, never
        // under-permissive. Narrowing it needs flow analysis, which is Phase 2.
        if let Some(name) = node.child_by_field_name("name") {
            self.bind(
                scope,
                BindingKind::PatternBinding,
                name,
                name,
                Visibility::FromName,
            );
        }
        if let Some(pattern) = node.child_by_field_name("pattern") {
            self.pattern(pattern, scope, depth + 1);
        }
        if let Some(left) = node.child_by_field_name("left") {
            self.walk(left, scope, depth + 1);
        }
    }

    /// Bind the names a pattern introduces, however deeply it deconstructs.
    fn pattern(&mut self, node: Node<'_>, scope: usize, depth: u32) {
        if depth > MAX_DEPTH {
            return;
        }
        match node.kind() {
            "type_pattern" => {
                if let Some(name) = named_child(node, "identifier") {
                    self.bind(
                        scope,
                        BindingKind::PatternBinding,
                        node,
                        name,
                        Visibility::FromName,
                    );
                }
            }
            "record_pattern_component" => {
                if let Some(name) = named_child(node, "identifier") {
                    self.bind(
                        scope,
                        BindingKind::PatternBinding,
                        node,
                        name,
                        Visibility::FromName,
                    );
                }
            }
            // `record_pattern` leads with the type name, which binds nothing;
            // everything bindable is inside the body.
            "record_pattern" => {
                if let Some(body) = named_child(node, "record_pattern_body") {
                    self.pattern(body, scope, depth + 1);
                }
            }
            // `pattern`, `record_pattern_body`, and the ERROR node recovery
            // wraps `case A, B ->` in.
            _ => {
                let mut cursor = node.walk();
                let children: Vec<_> = node.named_children(&mut cursor).collect();
                for child in children {
                    self.pattern(child, scope, depth + 1);
                }
            }
        }
    }

    fn type_parameters(&mut self, node: Node<'_>, scope: usize) {
        let Some(list) = node.child_by_field_name("type_parameters") else {
            return;
        };
        let mut cursor = list.walk();
        let names: Vec<_> = list
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "type_parameter")
            .filter_map(|parameter| {
                named_child(parameter, "type_identifier").map(|name| (parameter, name))
            })
            .collect();
        for (parameter, name) in names {
            self.bind(
                scope,
                BindingKind::TypeParameter,
                parameter,
                name,
                Visibility::WholeScope,
            );
        }
    }

    fn formal_parameters(&mut self, list: Node<'_>, scope: usize, kind: BindingKind) {
        let mut cursor = list.walk();
        let parameters: Vec<_> = list.named_children(&mut cursor).collect();
        for parameter in parameters {
            let name = match parameter.kind() {
                "formal_parameter" => parameter.child_by_field_name("name"),
                // A varargs parameter wraps its name in a declarator instead.
                "spread_parameter" => named_child(parameter, "variable_declarator")
                    .and_then(|declarator| declarator.child_by_field_name("name")),
                // `receiver_parameter` (`Foo this`) binds no new name.
                _ => None,
            };
            if let Some(name) = name {
                self.bind(scope, kind, parameter, name, Visibility::WholeScope);
            }
        }
    }

    /// One binding per declarator, so `int a, b;` binds two names.
    /// Binds each declarator in the first scope and walks its initialiser in
    /// the second.
    fn variables(
        &mut self,
        node: Node<'_>,
        (scope, values): (usize, usize),
        kind: BindingKind,
        visibility: Visibility,
        depth: u32,
    ) {
        let mut cursor = node.walk();
        let declarators: Vec<_> = node
            .named_children(&mut cursor)
            .filter(|child| child.kind() == "variable_declarator")
            .collect();
        for declarator in declarators {
            if let Some(name) = declarator.child_by_field_name("name") {
                self.bind(scope, kind, node, name, visibility);
            }
            // An initialiser can hold a lambda or an anonymous class.
            if let Some(value) = declarator.child_by_field_name("value") {
                self.walk(value, values, depth + 1);
            }
        }
    }

    fn span(&self, node: Node<'_>) -> Span {
        let start_byte = node.start_byte() as u32;
        let end_byte = node.end_byte() as u32;
        Span {
            start_byte,
            end_byte,
            range: Range::new(
                self.document.position_at(start_byte),
                self.document.position_at(end_byte),
            ),
        }
    }

    /// Slicing rather than `utf8_text` so a node whose range the grammar
    /// reported oddly yields nothing instead of an error.
    fn text(&self, node: Node<'_>) -> &str {
        self.document
            .text()
            .get(node.start_byte()..node.end_byte())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JavaParser;
    use javelin_document::DocumentId;

    /// A source string plus the scope tree built from it, so a test can name
    /// offsets by the text at them instead of counting bytes.
    struct Fixture {
        source: String,
        scopes: ScopeTree,
    }

    fn scopes(source: &str) -> Fixture {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 1, source);
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parse");
        Fixture {
            source: source.to_string(),
            scopes: ScopeTree::build(&document, &tree),
        }
    }

    impl Fixture {
        /// The offset just after `marker`, which is where a cursor sits when the
        /// user has finished typing it.
        fn after(&self, marker: &str) -> u32 {
            let at = self
                .source
                .find(marker)
                .unwrap_or_else(|| panic!("{marker:?} is not in the fixture"));
            (at + marker.len()) as u32
        }

        fn at(&self, marker: &str) -> u32 {
            self.source
                .find(marker)
                .unwrap_or_else(|| panic!("{marker:?} is not in the fixture")) as u32
        }

        fn resolve(&self, marker: &str, name: &str) -> Option<&Binding> {
            self.scopes.resolve(self.after(marker), name)
        }

        fn kind_of(&self, marker: &str, name: &str) -> Option<BindingKind> {
            self.resolve(marker, name).map(|binding| binding.kind)
        }

        fn visible(&self, marker: &str) -> Vec<&str> {
            self.scopes
                .visible_at(self.after(marker))
                .into_iter()
                .map(|binding| binding.name.as_str())
                .collect()
        }
    }

    #[test]
    fn a_top_level_type_is_bound_in_the_compilation_unit() {
        let f = scopes("class A { void a() { } } class B { void b() { } }");
        assert_eq!(
            f.kind_of("void a() {", "B"),
            Some(BindingKind::TypeDeclaration)
        );
        assert_eq!(
            f.kind_of("void b() {", "A"),
            Some(BindingKind::TypeDeclaration)
        );
    }

    #[test]
    fn a_field_is_visible_above_its_own_declaration() {
        let f = scopes("class A { int early() { return late; } int late; }");
        assert_eq!(f.kind_of("return ", "late"), Some(BindingKind::Field));
    }

    #[test]
    fn a_local_is_not_visible_above_its_own_declaration() {
        let f = scopes("class A { void m() { use(); int later = 1; } }");
        assert!(f.resolve("use()", "later").is_none());
        assert!(f.resolve("int later = 1", "later").is_some());
    }

    #[test]
    fn a_field_of_one_type_is_not_visible_in_another() {
        let f = scopes("class A { int only; } class B { void m() { } }");
        assert!(f.resolve("void m() {", "only").is_none());
    }

    #[test]
    fn a_block_local_shadows_a_field_of_the_same_name() {
        let f = scopes("class A { int x; void m() { { int x = 1; } } }");
        assert_eq!(
            f.kind_of("int x = 1", "x"),
            Some(BindingKind::LocalVariable)
        );
        assert_eq!(f.kind_of("void m() {", "x"), Some(BindingKind::Field));
    }

    #[test]
    fn a_local_of_an_inner_block_is_invisible_once_the_block_closes() {
        let f = scopes("class A { void m() { { int inner = 1; } after(); } }");
        assert!(f.resolve("int inner = 1", "inner").is_some());
        assert!(f.resolve("after()", "inner").is_none());
    }

    #[test]
    fn a_parameter_is_visible_throughout_its_method_and_nowhere_else() {
        let f = scopes("class A { void m(int p) { use(); } void other() { } }");
        assert_eq!(f.kind_of("use()", "p"), Some(BindingKind::Parameter));
        assert!(f.resolve("void other() {", "p").is_none());
    }

    #[test]
    fn a_constructor_parameter_is_bound_like_a_method_parameter() {
        let f = scopes("class A { A(int p) { use(); } }");
        assert_eq!(f.kind_of("use()", "p"), Some(BindingKind::Parameter));
    }

    #[test]
    fn a_varargs_parameter_binds_its_name_without_the_dots() {
        let f = scopes("class A { void m(String... rest) { use(); } }");
        let binding = f.resolve("use()", "rest").expect("bound");
        assert_eq!(binding.name, "rest");
        assert_eq!(binding.kind, BindingKind::Parameter);
    }

    #[test]
    fn an_initialiser_block_scopes_its_locals() {
        let f = scopes("class A { static { int s = 1; } { int i = 1; } void m() { } }");
        assert!(f.resolve("int s = 1", "s").is_some());
        assert!(f.resolve("int i = 1", "i").is_some());
        assert!(f.resolve("void m() {", "s").is_none());
        assert!(f.resolve("void m() {", "i").is_none());
    }

    #[test]
    fn a_for_loop_variable_is_visible_in_the_header_and_the_body_only() {
        let f = scopes("class A { void m() { for (int i = 0; i < 1; i++) { use(); } done(); } }");
        assert_eq!(f.kind_of("i < 1", "i"), Some(BindingKind::LocalVariable));
        assert!(f.resolve("use()", "i").is_some());
        assert!(f.resolve("done()", "i").is_none());
    }

    #[test]
    fn an_enhanced_for_binding_is_visible_in_the_body_only() {
        let f = scopes("class A { void m() { for (String s : all) { use(); } done(); } }");
        assert_eq!(f.kind_of("use()", "s"), Some(BindingKind::LocalVariable));
        assert!(f.resolve("done()", "s").is_none());
    }

    #[test]
    fn a_catch_parameter_is_visible_in_its_handler_only() {
        let f = scopes("class A { void m() { try { a(); } catch (Throwable e) { b(); } c(); } }");
        assert_eq!(f.kind_of("b()", "e"), Some(BindingKind::Parameter));
        assert!(f.resolve("a()", "e").is_none());
        assert!(f.resolve("c()", "e").is_none());
        assert_eq!(f.scopes.scope_at(f.after("b()")).kind, ScopeKind::Block);
    }

    #[test]
    fn a_try_resource_is_invisible_to_the_handlers() {
        let f = scopes(
            "class A { void m() { try (var r = open()) { use(); } catch (E e) { late(); } } }",
        );
        assert_eq!(f.kind_of("use()", "r"), Some(BindingKind::LocalVariable));
        assert!(
            f.resolve("late()", "r").is_none(),
            "a resource is closed before the handler runs"
        );
    }

    #[test]
    fn a_lambda_parameter_is_visible_in_the_lambda_only() {
        let f = scopes("class A { void m() { list.each(v -> use(v)); after(); } }");
        assert_eq!(f.kind_of("use(", "v"), Some(BindingKind::Parameter));
        assert!(f.resolve("after()", "v").is_none());
    }

    #[test]
    fn a_lambda_with_inferred_parameters_binds_all_of_them() {
        let f = scopes("class A { void m() { fold((a, b) -> use()); } }");
        assert!(f.resolve("use()", "a").is_some());
        assert!(f.resolve("use()", "b").is_some());
    }

    #[test]
    fn a_lambda_body_still_sees_the_scope_it_was_written_in() {
        let f = scopes("class A { int field; void m(int p) { run(() -> use()); } }");
        assert_eq!(f.kind_of("use()", "p"), Some(BindingKind::Parameter));
        assert_eq!(f.kind_of("use()", "field"), Some(BindingKind::Field));
        assert_eq!(f.scopes.scope_at(f.after("use()")).kind, ScopeKind::Lambda);
    }

    #[test]
    fn a_switch_pattern_binding_is_visible_in_its_own_arm_only() {
        let f = scopes(
            "class A { void m(Object o) { switch (o) { \
             case Integer i -> use(); default -> other(); } } }",
        );
        assert_eq!(f.kind_of("use()", "i"), Some(BindingKind::PatternBinding));
        assert!(f.resolve("other()", "i").is_none());
        assert_eq!(
            f.scopes.scope_at(f.after("use()")).kind,
            ScopeKind::SwitchCase
        );
    }

    #[test]
    fn a_colon_form_switch_label_binds_its_pattern_too() {
        let f = scopes(
            "class A { void m(Object o) { switch (o) { \
             case String s: use(); break; default: other(); } } }",
        );
        assert_eq!(f.kind_of("use()", "s"), Some(BindingKind::PatternBinding));
        assert!(f.resolve("other()", "s").is_none());
    }

    #[test]
    fn a_local_one_case_group_declares_is_in_scope_in_the_next() {
        let f = scopes(
            "class A { void m(int k) { switch (k) { \
             case 1: int count = 0; Runnable r = () -> use(); break; case 2: other(); } } }",
        );
        assert_eq!(
            f.kind_of("other()", "count"),
            Some(BindingKind::LocalVariable)
        );
        assert_eq!(
            f.kind_of("use()", "count"),
            Some(BindingKind::LocalVariable)
        );
        assert_eq!(f.kind_of("use()", "k"), Some(BindingKind::Parameter));
    }

    #[test]
    fn a_record_pattern_binds_every_component_it_deconstructs() {
        let f = scopes(
            "class A { void m(Object o) { \
             if (o instanceof Line(Point(int ax, int ay), Point p)) { use(); } } }",
        );
        for name in ["ax", "ay", "p"] {
            assert_eq!(
                f.kind_of("use()", name),
                Some(BindingKind::PatternBinding),
                "{name} should be bound by the deconstruction"
            );
        }
        assert!(
            f.resolve("use()", "Line").is_none(),
            "the leading name of a record pattern is a type, not a binding"
        );
    }

    #[test]
    fn an_instanceof_binding_is_visible_after_the_test() {
        let f = scopes("class A { void m(Object o) { if (o instanceof String s) { use(); } } }");
        assert_eq!(f.kind_of("use()", "s"), Some(BindingKind::PatternBinding));
    }

    #[test]
    fn an_anonymous_class_body_is_its_own_type_scope() {
        let f = scopes(
            "class A { void m() { run(new R() { int inner; void go() { use(); } }); after(); } }",
        );
        assert_eq!(f.kind_of("use()", "inner"), Some(BindingKind::Field));
        assert!(f.resolve("after()", "inner").is_none());
    }

    #[test]
    fn a_local_class_is_visible_from_its_declaration_to_the_end_of_the_block() {
        let f = scopes("class A { void m() { before(); class L { } after(); } }");
        assert_eq!(
            f.kind_of("after()", "L"),
            Some(BindingKind::TypeDeclaration)
        );
        assert!(f.resolve("before()", "L").is_none());
    }

    #[test]
    fn a_type_parameter_is_visible_throughout_the_declaration_that_introduces_it() {
        let f = scopes("class A<T> { void m() { use(); } <U> void g() { other(); } }");
        assert_eq!(f.kind_of("use()", "T"), Some(BindingKind::TypeParameter));
        assert_eq!(f.kind_of("other()", "U"), Some(BindingKind::TypeParameter));
        assert!(
            f.resolve("use()", "U").is_none(),
            "a method's type parameter does not escape it"
        );
    }

    #[test]
    fn a_record_component_is_bound_like_a_field() {
        let f = scopes("record P(int x) { int m() { return 0; } }");
        assert_eq!(f.kind_of("return 0", "x"), Some(BindingKind::Field));
    }

    #[test]
    fn an_enum_constant_is_bound_in_the_enum_body() {
        let f = scopes("enum E { A, B; void m() { use(); } }");
        assert_eq!(f.kind_of("use()", "A"), Some(BindingKind::Field));
        assert_eq!(f.kind_of("use()", "B"), Some(BindingKind::Field));
    }

    #[test]
    fn an_interface_constant_and_a_default_method_body_are_scoped() {
        let f = scopes("interface I { int C = 1; default void m() { int l = 1; use(); } }");
        assert_eq!(f.kind_of("use()", "C"), Some(BindingKind::Field));
        assert_eq!(f.kind_of("use()", "l"), Some(BindingKind::LocalVariable));
    }

    #[test]
    fn every_declarator_of_one_statement_is_bound() {
        let f = scopes("class A { void m() { int a = 1, b = 2; use(); } }");
        assert!(f.resolve("use()", "a").is_some());
        assert!(f.resolve("use()", "b").is_some());
    }

    #[test]
    fn a_name_bound_as_a_type_does_not_shadow_the_same_name_bound_as_a_value() {
        let f = scopes("class A { class Value { } int Value; void m() { use(); } }");
        let visible = f.visible("use()");
        assert!(visible.iter().filter(|name| **name == "Value").count() == 2);
    }

    #[test]
    fn a_name_bound_in_both_namespaces_resolves_to_the_one_that_was_asked_for() {
        let f = scopes("class A { class Value { } int Value; void m() { use(); } }");
        let at = f.after("use()");
        assert_eq!(
            f.scopes.resolve_type(at, "Value").map(|b| b.kind),
            Some(BindingKind::TypeDeclaration)
        );
        assert_eq!(
            f.scopes.resolve_value(at, "Value").map(|b| b.kind),
            Some(BindingKind::Field)
        );
        assert!(
            f.scopes.resolve(at, "Value").is_some(),
            "the namespace-blind lookup still answers, it just cannot say which"
        );
    }

    #[test]
    fn a_namespace_lookup_does_not_fall_back_to_the_other_namespace() {
        let f = scopes("class A { class OnlyAType { } void m() { use(); } }");
        let at = f.after("use()");
        assert!(f.scopes.resolve_type(at, "OnlyAType").is_some());
        assert!(
            f.scopes.resolve_value(at, "OnlyAType").is_none(),
            "a type is not a value, and answering with it would be a wrong answer"
        );
    }

    #[test]
    fn an_outer_type_is_not_hidden_by_a_nearer_variable_of_the_same_name() {
        // The field is in the innermost scope, so a namespace-blind walk stops
        // at it and never reaches the type one scope out.
        let f = scopes("class Value { } class A { int Value; void m() { use(); } }");
        assert_eq!(
            f.scopes
                .resolve_type(f.after("use()"), "Value")
                .map(|b| b.kind),
            Some(BindingKind::TypeDeclaration)
        );
    }

    #[test]
    fn enumerating_what_is_visible_lists_the_innermost_binding_of_a_name_once() {
        let f = scopes("class A { int x; void m(int y) { int x = 1; use(); } }");
        let visible = f.visible("use()");
        assert_eq!(
            visible,
            vec!["x", "y", "A"],
            "the local x hides the field, and both scopes contribute"
        );
        assert_eq!(
            f.scopes
                .visible_at(f.after("use()"))
                .iter()
                .find(|binding| binding.name == "x")
                .map(|binding| binding.kind),
            Some(BindingKind::LocalVariable)
        );
    }

    #[test]
    fn nothing_is_visible_that_has_not_been_declared() {
        let f = scopes("class A { void m() { use(); } }");
        assert!(f.resolve("use()", "nonexistent").is_none());
    }

    #[test]
    fn a_binding_reports_the_range_over_which_it_is_visible() {
        let f = scopes("class A { void m() { int x = 1; } }");
        let binding = f.resolve("int x = 1", "x").expect("bound");
        assert_eq!(binding.visible_from, f.at("x = 1"));
        assert_eq!(binding.visible_to, f.after("{ int x = 1; }"));
        assert_eq!(binding.span.start_byte, f.at("int x = 1"));
        assert_eq!(binding.name_span.end_byte, f.at("x = 1") + 1);
        assert_eq!(binding.name_span.range.start.line, 0);
    }

    #[test]
    fn a_half_typed_file_binds_whatever_parsed() {
        let f = scopes("class A { int field; void m(int p) { int local = ; } }");
        assert_eq!(f.kind_of("int local", "field"), Some(BindingKind::Field));
        assert_eq!(f.kind_of("int local", "p"), Some(BindingKind::Parameter));
        assert_eq!(
            f.kind_of("int local", "local"),
            Some(BindingKind::LocalVariable)
        );
    }

    #[test]
    fn a_file_the_grammar_cannot_recover_binds_nothing_rather_than_panicking() {
        // Recovery collapses this into a single ERROR node with no class and no
        // method in it; the only honest answer is the empty one.
        let f = scopes("class A { int field; void m(int p) { int local = ; ");
        assert!(f.resolve("int local", "p").is_none());
        assert!(f.scopes.visible_at(f.after("int local")).len() <= 1);
    }

    #[test]
    fn an_offset_past_the_end_of_the_file_answers_instead_of_panicking() {
        let f = scopes("class A { }");
        let past = f.source.len() as u32 + 1_000;
        assert_eq!(f.scopes.scope_at(past).kind, ScopeKind::CompilationUnit);
        assert!(f.scopes.resolve(past, "A").is_none());
        assert!(f.scopes.visible_at(past).is_empty());
    }

    #[test]
    fn deeply_nested_source_is_truncated_rather_than_overflowing_the_stack() {
        let depth = MAX_DEPTH as usize + 50;
        let source = format!(
            "class A {{ void m() {{ {} int deep = 1; {} }} }}",
            "{".repeat(depth),
            "}".repeat(depth)
        );
        let f = scopes(&source);
        assert!(f.scopes.resolve(f.after("class A"), "A").is_some());
    }

    /// Sources shaped like every construct the builder opens a scope for, so
    /// the equivalence test below walks shared boundaries, empty scopes,
    /// siblings and deep nesting.
    const SOURCES: &[&str] = &[
        "class A { void a() { } } class B { void b() { } }",
        "class A { int x; void m() { { int x = 1; } } }",
        "class A { void m() { for (int i = 0; i < 1; i++) { use(); } done(); } }",
        "class A { void m() { try (var r = open()) { use(); } catch (E e) { late(); } } }",
        "class A { void m() { list.each(v -> use(v)); fold((a, b) -> a); } }",
        "class A { void m(Object o) { switch (o) { case Integer i -> use(); default -> other(); } } }",
        "class A { void m(Object o) { switch (o) { case String s: use(); break; default: x(); } } }",
        "class A { void m(Object o) { if (o instanceof Line(Point(int ax, int ay), Point p)) {} } }",
        "class A { void m() { run(new R() { int inner; void go() { use(); } }); after(); } }",
        "class A { void m() { before(); class L { } after(); } }",
        "class A<T> { void m() { use(); } <U> void g() { other(); } }",
        "record P(int x) { int m() { return 0; } }",
        "enum E { A, B { void f() {} }; void m() { use(); } }",
        "class A { class Value { } int Value; void m() { use(); } }",
        "class A { int x; int x; void m() { use(); } }",
        "class A { static { int s = 1; } { int i = 1; } void m() { } }",
        "class A { int field; void m(int p) { int local = ; } }",
    ];

    /// Today's lookups, kept verbatim as the reference the indexed ones are
    /// checked against: innermost scope by a reversed linear scan of the
    /// children, then a reversed linear scan of each enclosing scope's
    /// bindings.
    fn linear_scope_index_at(tree: &ScopeTree, offset: u32) -> usize {
        let mut current = 0;
        while let Some(child) = tree.scopes[current]
            .children
            .iter()
            .rev()
            .copied()
            .find(|&child| tree.scopes[child].contains(offset))
        {
            current = child;
        }
        current
    }

    fn linear_find<'a>(
        tree: &'a ScopeTree,
        offset: u32,
        name: &str,
        namespace: impl Fn(BindingKind) -> bool,
    ) -> Option<&'a Binding> {
        let mut next = Some(linear_scope_index_at(tree, offset));
        while let Some(scope) = next {
            let found = tree.scopes[scope]
                .bindings
                .iter()
                .rev()
                .map(|&binding| &tree.bindings[binding])
                .find(|binding| {
                    binding.name == name && namespace(binding.kind) && binding.is_visible_at(offset)
                });
            if found.is_some() {
                return found;
            }
            next = tree.scopes[scope].parent;
        }
        None
    }

    #[test]
    fn binary_search_and_the_name_index_agree_with_a_linear_scan_everywhere() {
        for source in SOURCES {
            let f = scopes(source);
            let names: HashSet<&str> = f.scopes.bindings.iter().map(|b| b.name.as_str()).collect();
            for offset in 0..=source.len() as u32 + 1 {
                assert_eq!(
                    f.scopes.scope_index_at(offset),
                    linear_scope_index_at(&f.scopes, offset),
                    "scope at {offset} in {source:?}"
                );
                for name in &names {
                    let spans = |binding: Option<&Binding>| binding.map(|b| b.name_span);
                    assert_eq!(
                        spans(f.scopes.resolve(offset, name)),
                        spans(linear_find(&f.scopes, offset, name, |_| true)),
                        "{name} at {offset} in {source:?}"
                    );
                    assert_eq!(
                        spans(f.scopes.resolve_type(offset, name)),
                        spans(linear_find(&f.scopes, offset, name, BindingKind::is_type)),
                        "type {name} at {offset} in {source:?}"
                    );
                    assert_eq!(
                        spans(f.scopes.resolve_value(offset, name)),
                        spans(linear_find(&f.scopes, offset, name, |kind| !kind.is_type())),
                        "value {name} at {offset} in {source:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_duplicate_binding_in_one_scope_still_prefers_the_later_one() {
        let f = scopes("class A { int x; int x; void m() { use(); } }");
        let binding = f.resolve("use()", "x").expect("bound");
        assert_eq!(binding.name_span.start_byte, f.at("x; void"));
    }

    #[test]
    fn the_tree_remembers_the_version_it_was_built_from() {
        let document = TextDocument::new(DocumentId::new("file:///A.java"), 7, "class A {}");
        let tree = JavaParser::new()
            .expect("grammar")
            .parse(&document)
            .expect("parse");
        assert_eq!(ScopeTree::build(&document, &tree).version(), 7);
    }
}
