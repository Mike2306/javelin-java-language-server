# Test fixtures

Java sources used by integration tests. Keep them small and keep each one
focused on a single property, so a failure names the cause.

- `java/Simple.java` — a well-formed compilation unit.
- `java/Broken.java` — deliberately unparseable; proves error recovery.
- `java/Unicode.java` — non-ASCII content; proves UTF-16 position mapping.
- `java/Generics.java` — generic class and method, bounds and wildcards.
- `java/Lambdas.java` — lambdas with and without parameter types, method references.
- `java/Records.java` — record components, compact constructor, nested record.
- `java/Nested.java` — nested, inner, local and anonymous classes.
- `java/Annotations.java` — an annotation type, and annotated members.
- `java/Enums.java` — enum constants, a constant body, and ordinary members.
- `java/Javadoc.java` — doc comments on a type, a method and a field.
- `java/Shadowing.java` — one name bound as a field, a parameter, a block-local and a lambda parameter.

Larger, generated sources belong in `../projects/` (workspace-shaped inputs) or
`../../benchmarks/fixtures/` (performance inputs), not here.
