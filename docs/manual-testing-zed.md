# The editor pass

A pass against the real `javelin` binary, over real pipes, at the end of every
phase — before calling that phase done. It used to be a person opening Zed and
typing; it is two test binaries now, and this file is the record of *why* each
check exists rather than the instructions for performing one. The file keeps
its name because several documents link to it by path.

## Why this exists

The in-process test suite covers a lot, but two classes of failure are
invisible to it.

**Transport and lifecycle bugs.** The integration tests use
`lsp_server::Connection::memory()`, which has no I/O threads. A server that
holds its connection across `IoThreads::join()` passes every in-process test and
then hangs forever on `exit`. That exact bug shipped in the Phase 0 scaffold and
was only caught by spawning the real binary. `crates/cli/tests/stdio_lifecycle.rs`
now guards it, but the general lesson holds: the harness is not the client.

**Whether the client actually uses what we advertise.** Capabilities are
negotiated. A server can advertise incremental sync and UTF-16 positions and
still be sent something else — and a feature it does *not* advertise is one no
editor will ever ask for, however well its handler works. The suite's editor
refuses to send what the handshake did not permit, so that second failure
happens at the point of *asking*, and `crates/cli/tests/reachability.rs`
asserts the two directions against each other. What neither can do is prove the
model matches what Zed really sends; that is the third entry under
[What the suite does not prove](#what-the-suite-does-not-prove).

## Running it

```bash
cargo test -p javelin --test editor_pass
cargo test -p javelin --test reachability
```

Thirty-four checks in the first and two in the second. Both run the real
`javelin` binary over real pipes with a client that obeys the handshake, and
neither needs a JDK installed:
every check that needs a classpath builds a synthetic one — a handful of source
files in a zip, read in milliseconds and identical on every machine
(`crates/cli/tests/harness/jdk.rs`). Both binaries compile the shared harness,
whose own unit tests run alongside the checks, so the counts the run prints —
45 and 13 — are larger than the two above.

Two of the thirty-four carry a number rather than a behaviour, and between them
they are what the manual pass's "check the numbers against the budget" step
became. `the_first_parse_and_every_reparse_stay_inside_their_budgets` asserts
both: the 10 ms first parse, and a reparse p95 inside the same, over
twenty-five keystrokes each of which waits for its own reparse.

The other one is `#[ignore]`d. `the_measured_numbers_are_recorded` reads the
machine's *own* JDK, which costs seconds and depends on what is installed, so
it follows the rule in `CLAUDE.md` for tests that do. `scripts/check.sh` — the
local gate — runs it with `-- --ignored --exact`, and it rewrites
[`editor-pass-results.md`](editor-pass-results.md) whole. A plain `cargo test`
skips it and leaves `docs/` clean. CI runs `cargo test --workspace
--all-targets` rather than the script, so the recording happens on a
developer's machine and nowhere else.

## The demo project

`tests/projects/zed-demo/` is checked in and deliberately small:

- `Greeter.java` — generics, lambdas, a record with a compact constructor, a
  nested interface, a local that shadows a field, a `var` local, an unqualified
  call, and non-ASCII content (`Grüße`, `🚀`) so UTF-16 position mapping is
  exercised rather than assumed. Its *text* is what the checks name now, not
  its line numbers: `Editor::find` locates a position by a snippet, and two
  helpers match a whole statement and a whole method body, so rewording a line
  a test quotes fails that test with a panic naming the snippet it could not
  find. Adding to the file is free; rewording what is already there is not.
- `Broken.java` — unparseable on purpose. Opening it must degrade that one file,
  not the session.
- `pom.xml` — so build-system detection has something to find.

## What to expect per phase

The lifecycle checks stay the same. What changes is what the editor can do
between them. The third column names the test that performs each check; the
reason it exists is the second column and the test's own comment, which is
where whoever it fails on will be standing.

| Phase | New in the editor | How to check |
|---|---|---|
| **0 — Foundation** | Nothing. Every request is answered `MethodNotFound`, so Zed simply offers nothing. Highlighting comes from Zed's Java extension, not from us. | The lifecycle tests, which every later phase inherits: `opening_a_well_formed_file_parses_it_without_error_recovery`, `a_burst_of_keystrokes_reparses_the_version_the_buffer_ended_at`, `an_edit_arriving_after_the_previous_reparse_landed_is_parsed_incrementally`, `a_broken_file_degrades_alone_and_leaves_the_session_up`, `a_shutdown_leaves_no_process_behind`, and both tests in `reachability.rs` — `every_method_the_router_answers_is_advertised_and_the_reverse` and `the_negotiated_sync_kind_is_the_one_the_editor_can_speak`. The manual pass's one "type a character" step is **two** tests here, and the split is a real property rather than duplication: a burst of keystrokes leaves exactly *one* reparse behind, because the pool cancels every superseded job, so `incremental=true` is only assertable on an edit that arrives after the previous reparse has landed. The burst test asserts the version the buffer ended at; the waited-for one asserts the incremental path. |
| **1 — Interactive Java** | Outline, hover, definition and completion for anything declared in the open file. | `the_outline_nests_the_record_and_the_nested_interface`, `definition_on_a_shadowing_lambda_parameter_lands_on_the_parameter_not_the_field`, `completion_in_a_method_body_offers_locals_and_members_and_nothing_from_java_util` |
| **2a — Semantic core: member resolution** | Completion, definition and hover after a `.`, whenever the open file declares the receiver's type. **No semantic diagnostics** — they were cut from Phase 2 and return in Phase 3, so `Broken.java` must still produce none. **No resolved type is rendered anywhere**: hover shows the declaration's source text, as it did in Phase 1. | `completing_after_this_offers_instance_members_and_no_statics`, `completing_after_the_class_name_offers_statics_and_no_instance_members`, `a_receiver_whose_type_is_outside_the_file_offers_nothing_rather_than_locals`, `definition_on_a_field_assignment_lands_on_the_field_not_the_parameter`, `hover_on_a_field_renders_its_declaration_source_rather_than_a_resolved_type` |
| **2b — `var` and the unqualified call** | Two widenings of what 2a already does. A local declared `var` now has a type — its initialiser's — so completion, definition and hover after a `.` work through one. And definition and hover on an unqualified call land on the method, when exactly one of that name fits the call's arity; completion is untouched by that second one, because an unqualified prefix has been offered from local scope since Phase 1. Still no diagnostics, and still nothing from an import or the JDK. | `a_var_local_has_its_initialisers_type_for_member_completion`, `definition_through_a_var_local_reaches_the_method`, `definition_on_an_unqualified_call_lands_on_the_method`, `hover_on_an_unqualified_call_renders_its_signature`, `a_jdk_name_answers_nothing_at_every_position_before_the_classpath` |
| **2c — overload resolution** | Definition and hover on a call now choose between two methods that share a name and an arity, when the arguments' types tell them apart — and refuse, rather than guess, when they do not. The refusal is the larger change: a *qualified* ambiguous call used to land on whichever member of that name was declared first, consulting neither arity nor namespace, so three of the checks opposite are regression tests against a wrong jump rather than tests of a new answer. Completion after a `.` gains nothing of its own, but it now works through a receiver whose type came from an overloaded call. Still no diagnostics, and still nothing from an import or the JDK: an overload pair differing only in a JDK parameter type answers nothing, which is Phase 3's classpath rather than a bug here. | `an_overloaded_call_picks_the_candidate_its_argument_type_allows`, `an_overload_pair_a_jdk_type_cannot_separate_answers_nothing`, `a_field_and_a_method_of_one_name_are_told_apart_by_the_namespace_asked_for`, `hover_agrees_with_definition_at_every_overloaded_position`, `completion_through_an_overloaded_call_lists_one_entry_per_member`. The scratch edit the manual pass prescribed — and told you to revert — is `with_overloads`, typed into the buffer and thrown away with the session. |
| **3a — the classpath floor** | A name from outside the open file resolves — its *name*, not its members. Definition and hover on a call now separate two overloads that differ only in a JDK parameter type, which 2c refused, and a file-local type whose supertype is outside the file is separable too. **Nothing new can be looked up *on* an external type**: completion after a `.` on a `String` is still empty, and hover still renders declaration source text rather than a resolved type. **Still no diagnostics** — those are 3c, so `Broken.java` must still produce none. And the classpath arrives *late*: the first session on a machine spends about eleven seconds reading `src.zip` on a background thread, and every answer during that time is 2c's answer. The editor staying useful throughout is the check that matters most. | `the_outline_comes_back_before_the_classpath_read_finishes`, `an_overload_pair_a_jdk_type_can_separate_resolves_once_the_classpath_arrives`, `of_two_applicable_overloads_the_most_specific_is_picked` (the refusal it pinned until Tier 1's slice 8), `a_supertype_outside_the_file_no_longer_aborts_the_walk`. The manual row's caveat — that the two positive checks had only ever been run against a hand-built world of four names — is answered twice over: by the Zed walk of 2026-09-08 against a real `src.zip`, and by these tests against the synthetic JDK, which carries real supertype links. The row's "completion on a `String` must be empty" check is **not** here: 3b made it false, and it lives on in the 3b row as the opposite assertion. |
| **3b — workspace sources and external members** | Members of an external type, cross-file navigation, references, workspace symbols. **Nine slices have landed and the row is not signed off**: the pass is owed at the end of the phase, not after a slice. The addressing slice changed no answer at all. The external-members slice makes a `.` on a classpath receiver, hover on one of its members, and a local class inheriting an external supertype's members answerable, and the definition slice turns the last refusal of the three into an answer: go-to-definition on a classpath member lands in the JDK's own source now, one `src.zip` entry extracted at a time under the cache root and opened as an ordinary `file:`. Two things about that copy, so neither reads as a bug: it is writable and an edit to it is compiled by nothing, and the *first* jump of a session pays about 24 ms opening the archive, where every later jump into the same file is microseconds. Definition on an external *type* name still answers nothing — a fact records no span for a type's own name — and so does everything cross-file. The workspace-index slice changed no answer either, and the workspace-symbol slice changed the first one that is not about the open buffer: **`Ctrl-T` finds a type in a file the editor never opened**, listed by its package or its outer type, and the jump lands on the type's own name. Three things about it so none reads as a bug. It offers **types only** — the index stores no members, so a method or field name finds nothing. It describes **what is on disk at the last walk**: a class typed into a buffer and not saved is absent, and one whose declaration has been moved down the file is offered at the line it used to be on, which is the only *wrong* answer in the slice and is `ponytail:`-marked on the handler. And a match is a substring of the *simple* name ignoring ASCII case, with prefix matches first and 500 results at most — Zed re-ranks the list against its own fuzzy matcher, so the order it shows is its own. The cross-file-name slice is the one that resolves rather than lists: **a name the open file does not declare can now mean a sibling source file**, so ctrl-clicking `Greeter` in `Reception.java` lands on `Greeter.java`'s own name and hovering it reads `class com.example.Greeter`. Four things about that so none reads as a bug. It answers about a **name**, not a member — `g.greet()` for a cross-file `Greeter g` still finds nothing, because the index stores no members. Hover shows the **kind and the fully-qualified name and nothing else**, because that is all the index holds; it is not a truncated view of the declaration. Two files declaring one fully-qualified name answer **nothing**, which is the ambiguity rule and not a lookup failure. And the jump's **line is as old as the last walk**, the same staleness `Ctrl-T` has and for the same reason — a declaration moved down its file sends the caret to the line it used to be on. A JDK type's own name still answers nothing at all. The references slice answers **find-references**, by definition's own rule — a token is listed exactly when ctrl-clicking it would land on the target. On a type, `Shift-F12` lists every file that names it, including files never opened: references to `Greeter` from `Greeter.java` list its two uses in `Reception.java`. On anything else — a local, a field, a method — it lists the uses in the open file only, including `this.count` and method call sites, and a call two overloads both fit is listed under neither. Four things so none reads as a bug: a member used from *another* file is not listed yet, because the index stores no members; the segments of a qualified name such as `com.example.Greeter` and import lines are not listed, because definition answers nothing there either; another open file's unsaved edits are not seen, since its rows are as old as the last walk; and a constructor's own name lists nothing but itself, because `new Greeter(…)` resolves to the type. The workspace-members slice answers the cross-file `.`: in `Reception.java`, ctrl-clicking `greet` in `greeter.greet()` lands on `greet`'s own name in `Greeter.java`, hovering it reads `String greet()` — built from the index, so no parameter names and no Javadoc, exactly as a JDK member hovers — and typing `greeter.` offers `Greeter`'s members. An unqualified call to an inherited method answers the same way, and `Ctrl-T` for `greet` now lists `greet`, `greetAll`, `onGreet` and `Reception`'s private field `greeter` beside `Greeter`, each under the type that declares it — a private member is listed because readers navigate to private helpers, though no other file can select it. Two things so neither reads as a bug: a member's line is as old as the last walk, like everything else the index says, and an enum's or a record's member list is marked incomplete because `values()` and a record's accessors are members no file writes. The member-references slice answers who *uses* a member from another file: `Shift-F12` on `greet` in `Greeter.java` lists `greeter.greet()` in `Reception.java`, a file never opened, beside `main`'s own call. The workspace worker resolves every member selection in the background, so three things so none reads as a bug: a call made into the JDK — `s.length()` — is listed in the open file only, because JDK calls are not recorded; a bare inherited *field* in a subclass is not a use, because ctrl-clicking it lands nowhere either; and — until the first release's slice 3 — references to an interface method did not list calls resolved to an override. Just after an edit above a declaration, its other files' uses can be missing for the quarter-second before the index catches up. **Hover reads differently on the two halves, by design rather than by omission**: a member the open file declares hovers as its own source text plus whatever Javadoc sits above it, while a classpath member hovers as a signature built from facts — no prose, and no parameter names, because a type fact stores neither. `length()` hovering as `int length()` rather than as the JDK's source is the correct answer, not a truncated one. **Two ceilings a reader will meet and should not file as bugs**, and one that has closed: an external type's type arguments used to be dropped, and the first release's slice 1 substitutes them — see its row. An external type's *nested* types are facts of their own rather than members, so `Map.` offers `Map`'s static methods with no `Entry` among them, `ponytail:`-marked in `crates/semantic/src/members.rs`. The other is the one that is a *wrong* answer rather than a missing one: **accessibility is not modelled**, so a `String` receiver also offers `clone` and `finalize`, `java.lang.Object`'s two `protected` methods, which are not callable on a `String` from another package — `private` is the only accessibility `collect_members` in `crates/classpath/src/sources.rs` drops, and the file-local walk does not check it either. Deliberate, and `ponytail:`-marked there. | `completing_on_an_external_receiver_offers_its_members`, `hover_on_an_external_member_renders_facts_with_no_prose`, `definition_on_an_external_member_lands_in_the_jdks_own_sources`, `a_local_class_extending_an_external_one_answers_with_both_halves_in_one_list`, `a_dot_on_a_string_literal_offers_strings_members` (once the refusal pinned here, flipped by Tier 1's slice 8), `a_symbol_picker_finds_a_type_in_a_file_the_editor_never_opened`, `a_name_that_means_the_file_next_door_jumps_into_it_and_hovers_as_it`, `an_external_type_name_jumps_to_its_own_name_in_the_jdks_sources` (once the refusal pinned here, flipped by Tier 1's first slice), `references_to_a_type_find_a_file_the_editor_never_opened`, `a_cross_file_member_completes_hovers_and_jumps`, `references_to_a_method_find_a_call_in_a_file_the_editor_never_opened`. The literal one was the refusal easiest to mistake for a missing member: `"x".length()` answered nothing because `infer.rs` typed a literal from its form and never asked the world; Tier 1's slice 8 types it as `java.lang.String`. The unsaved-buffer one pins a ceiling rather than a feature: it is the check that fails the day the index starts listening for a save, which is the point at which the handler's `ponytail:` is stale. The last two are the cross-file pair: one proves the answer, and the one beside it used to pin the refusal that was easiest to mistake for it — an external *type* name resolved with nowhere to jump. Tier 1's first slice closed it. The member-references one waits for the walk's second publish rather than its first, because member references land only once every file is resolved. |
| **5a — Reading aids** | Four requests that need nothing beyond the open file's `FileSyntax` and `ScopeTree` — no index, no classpath, and they landed after 3b's six slices rather than before them despite the phase number. Folding ranges fold a class or method body, a multi-line block comment, and and the file's imports from the first to the last as one — blank lines between import groups included, since a run is a question about the AST rather than about line numbers, and a fold that would hide no line is not offered at all. Selection ranges expand outward from an identifier through the named-node ancestor chain to the root. Document highlight lights up every occurrence `ScopeTree` binds to the same name as the one under the cursor, and nothing else — **clicking a method's or a constructor's own name highlights nothing**, because neither is bound by `ScopeTree`; matching a call to its declaration is overload resolution, not a scope lookup, and that refusal is `ponytail:`-marked in `crates/parser/src/queries.rs`. `semanticTokens/full` colours comments, literals and keywords from the tree and identifiers from `ScopeTree`, and **an identifier that resolves to nothing gets no token at all** rather than a guessed colour, the same "ambiguity answers nothing" rule as everywhere else in the engine. Its own benchmark misses the 50 ms budget at `Tier::Large` (78.29 ms) and `Tier::Huge` (5.47 s), which [`performance.md`](performance.md) traces to `ScopeTree::find`'s linear per-scope scan rather than to the response's size — a `semanticTokens/full/delta` would not move it. Because that cost falls on the single-threaded request loop, where a `$/cancelRequest` cannot reach the request already running, the handler refuses a document over 128 KiB and answers an empty token list — the client keeps its own TextMate colouring, which is plainer rather than wrong. Parameter inlay hints did **not** ship in this phase: they need a parameter name on `MemberFacts`, which is a classpath change rather than a free reading of the open file, so they stay owed rather than moving here. None of this is signed off — the four checks pass in the automated pass and there is no row below for this phase yet, because sign-off is a human opening Zed. | `folding_ranges_are_offered_for_a_class_and_the_methods_inside_it`, `selection_ranges_expand_outward_from_an_identifier`, `clicking_a_local_highlights_every_use_of_it_and_nothing_else`, `semantic_tokens_are_offered_for_the_open_file_and_decode_to_the_declared_legend` |
| **3c — semantic diagnostics** | Unresolved *types* reported as warnings, for the first time in the project's life — and only where the world is whole — and, since the second slice, a *method* a certain receiver has no member of, under the code `unresolved-method`. **Two slices have landed and the row is not signed off.** A project with a build file is diagnosed only with `build.importOnStartup: true` and a resolution that succeeded, so the demo project, which has a `pom.xml`, gets **no diagnostics at all** under the default settings, `Broken.java` included; a project with no build file is judged once the JDK and the walk are in — and only by a client that watches the files, which Zed does and which the harness's editor now declares, as Zed does. Fields, unqualified calls, method references and qualified names are not diagnosed, a project with Lombok is diagnosed for types only, a project compiling at a level below the JDK's has no call on a JDK receiver judged, and editing a build file turns diagnostics off until the server restarts. | `a_hallucinated_method_is_flagged_in_a_loose_project_and_fixing_it_clears_the_flag`: `name.lenth()` on a JDK `String` beside a real `name.length()` is one warning on `lenth`'s range naming `java.lang.String`, and typing the missing `g` publishes an empty list. `a_hallucinated_type_is_flagged_in_a_loose_project_and_fixing_it_clears_the_flag`: a valid file is judged and published an **empty** list — the pass condition this row was held back for — a `List<Greter>` beside a real `Greeter` and a JDK `String` is one warning on `Greter`'s range, and typing the missing `e` publishes an empty list again. Every gate and exemption has an in-process test in `crates/server/src/diagnostics.rs`. |
| **3d — the inverse edge (re-indexing landed first)** | The index follows the workspace: a save, a change on disk the editor watches, and an open buffer's unsaved text are re-indexed after a quarter-second pause; closing a buffer puts the file back to what is on disk, and deleting or renaming a folder re-walks the workspace so its types go with it. The rest of the phase has landed too: **`Ctrl-F12`/Go to Implementation on an interface lists its implementations** in files never opened, on a method its overrides, and the type and call hierarchies expand one level at a time — callers grouped under the method each call is written inside, a field initialiser under its field. Three more things so none reads as a bug: a JDK or JAR supertype is missing from the type hierarchy, because it has no file to show; calls into the JDK are not outgoing calls; and a generic method's override is matched by name and arity where the interface's parameter is a type variable. Three things so none reads as a bug: an answer asked within the pause after a keystroke is from before it; a change on disk reaches the index only through a client that registers a file watcher for this server — Zed and VS Code do — so an editor that does not sees a `git checkout` only as files are saved; and `Ctrl-T` finding a type that exists only in an unsaved buffer is the feature, not a leak. | `a_type_declared_only_in_an_unsaved_buffer_is_offered_until_the_buffer_closes` (the flip of the check that pinned the old ceiling), `a_file_created_and_deleted_on_disk_follows_the_watcher`, `a_declaration_moved_on_disk_is_jumped_to_at_its_new_line`, `implementations_and_callers_are_found_in_files_the_editor_never_opened`. |
| **4 — Build systems** | Dependencies on the classpath. **The first slice has landed and the row is not signed off.** A type a dependency declares completes and hovers once Maven or Gradle has answered, which on a cold project is seconds to minutes and on a warm one is a read of the cached classpath; until then, and with no tool on `PATH`, every answer is 3b's. Three things so none reads as a bug: ctrl-click on a dependency's member lands nowhere, because a class file has no text — until Tier 1's slice 4, and then only where a `-sources.jar` is on disk; its hover has no parameter names and no javadoc, for the same reason; and none of it happens until **`build.importOnStartup: true`** is set, because running the build tool runs the project's own code — its wrapper, its build script, its plugins — and a reading tool does not do that unasked. An argument of a dependency's type also excludes no overload: dependency facts answer names and members but never subtyping, because a JAR can be a stale installed copy of the workspace's own module. (A JDK argument does exclude a dependency's parameter type, since Tier 5's slice 3: nothing above a JDK type is a JAR's.) | `a_type_from_a_dependency_jar_completes_and_hovers` drives a fake `mvnw` naming a committed JAR, so the real spawn, parse and cache path runs without the network; `the_jdk_answers_while_the_build_tool_is_still_running` pins the other half of the row — a JDK member completes while a build tool that never finishes is running, and the tool does not outlive the server. Both are Unix-only, because the fake wrapper is a shell script. |
| **v1 — the first release** | Coverage for the shapes a reviewer meets most, in four slices. **All four slices have landed and the row is not signed off** — the walk owed for it, and for every row from 3b on, is [the first-release walk](#the-first-release-walk--owed-for-a-human). A JDK or JAR member's type arguments are substituted: `names.get(0).length()` on a `List<String>`, `counts.get(k).intValue()` on a `Map<String, Integer>`, `maybe.get().length()` on an `Optional<String>`, and the `get` a local `class Names extends ArrayList<String>` inherits all hover, jump into `String.java` and complete, and `names.get(0).lenth()` is flagged `unresolved-method`. Three things so none reads as a bug: hover still renders the member as declared, `E get(int)`, not as `String get(int)`; a raw `List`'s `get(0)` answers nothing at all — never `Object` — and is never flagged; and a `Names extends ArrayList<String>` declared in *another* file still answers `E`. Slice 2 types an implicit lambda parameter: in `names.forEach(x -> x.length())`, `names.stream().filter(y -> …).map(z -> z.substring(0, 1))` and `all.forEach(g -> g.greet())`, hover, ctrl-click and completion on the parameter's members work, and `Shift-F12` on `greet` lists the call inside the lambda. Three things so none reads as a bug: what `map(…)` returns is not inferred, so a `.` after `map(z -> …)` answers nothing; a lambda assigned to a variable, `Function<String, Integer> f = s -> s.length()`, is not typed, though one with a written parameter type, `(String s) -> …`, is; and a call on a lambda parameter is never flagged `unresolved-method`, by design. Slice 3 makes references follow an override: `Shift-F12` on an interface method lists the calls through every implementation's override too, and on an override the calls through the interface — never a sibling implementation's. Every entry looks the same, because LSP has no way to say "via an override"; the call hierarchy's incoming calls still list only calls to the exact method. The first session after upgrading re-reads the JDK, about eleven seconds, because the fact cache's schema moved — to 16 then, and to 17 with Tier 1. | `a_type_argument_carries_a_chain_through_a_jdk_member`, `completing_after_a_generic_jdk_members_call_offers_the_type_arguments_members`, `a_hallucinated_method_on_a_type_argument_is_flagged`, `an_implicit_lambda_parameter_takes_its_type_from_the_functional_interface`, `references_follow_an_override_both_ways_and_skip_its_siblings`. |
| **T1 — Tier 1 reading gaps** | The reading gaps a developer hits in real code, one slice at a time; **not signed off** — the walk owed for it is [the Tier 1 walk](#the-tier-1-walk--owed-for-a-human). Slice 1: **ctrl-click on a JDK type's own name** — `String` in `String s` — lands on `String`'s name in the extracted `String.java`, and hovering it reads `class java.lang.String`. Two things so neither reads as a bug: a dependency JAR's type hovers the same way but still has nowhere to jump, because a class file has no source (until the `-sources.jar` slice); and the first session after upgrading re-reads the JDK, about eleven seconds, because the fact cache's schema moved to 17. Slice 2: **an annotation processor's output is indexed** — `Ctrl-F12` on a MapStruct mapper interface lists its `*MapperImpl` from `build/generated/sources/annotationProcessor` (or Maven's `target/generated-sources/annotations`), and ctrl-click on a name meaning it opens it. Three things so none reads as a bug: what is there is whatever the last build left, so a mapper changed since the last build shows the old impl; no squiggle ever appears in a generated file, nor for a call on a receiver whose type reaches one, by design; and a protobuf or OpenAPI generator's output is still not indexed. Slice 3: **what Lombok generates is a member** — in a file using a `@Data`, `@Value`, `@Getter`/`@Setter`, `@With` or `@Builder` class, `car.getName()` completes, hovers as `String getName()` and ctrl-clicks onto the field `name`, and `Car.builder().name("x").build()` is followed hop by hop, `builder()` and `build()` landing on the class's name. `Shift-F12` on the field lists every getter, setter and builder call of it. Four things so none reads as a bug: a getter *is* its field for navigation, so references on `getName()` list `setName(…)` calls too; `@Slf4j`'s `log`, `@Singular`, `@SuperBuilder`, `@Delegate` and a `@Builder` on a constructor generate nothing here yet; a `lombok.config` that renames accessors (`lombok.accessors.fluent`, `lombok.getter.noIsPrefix`, …) turns all of it off below it; and `unresolved-method` still never fires in a project that uses Lombok. The first session after upgrading re-reads the JDK, about eleven seconds, because the cache's schema moved to 18. Slice 4: **a dependency's `-sources.jar`, where one is already on disk, is a jump target** — ctrl-click on a JAR member or type opens the extracted source from the cache's `jar-sources` directory, on the name. Three things so none reads as a bug: nothing is downloaded, so a dependency whose sources your build or IDE never fetched still jumps nowhere; an overload the sources cannot tell apart from a sibling of the same arity by its parameter types' simple names jumps nowhere rather than to the wrong one; and hover is unchanged, still without parameter names or javadoc. Slice 5: **parameter names** — hover on a JDK member reads `String substring(int beginIndex, int endIndex)`, and Zed's inlay hints show `beginIndex:` and `endIndex:` before the arguments of `s.substring(0, 3)`, for a call to a method of the open file, of the workspace, of the JDK, or of a dependency whose `-sources.jar` is on disk. Three things so none reads as a bug: an argument already spelled like its parameter — `x` for `x`, `getName()` for `name` — gets no hint; a call two overloads both fit gets none, as definition answers nothing there; and a dependency read without its sources has no names, so no hints. Inlay hints are off in Zed by default (`"inlay_hints": { "enabled": true }`). The first session after upgrading re-reads the JDK once more, because the schema moved to 19. Slice 6: **Javadoc in hover** — hovering a JDK member, a workspace member or type in another file, or a dependency's member whose `-sources.jar` is on disk shows its Javadoc as Markdown under the signature: `{@code}` as code, `@param` as a list, `<p>` as a paragraph. Three things so none reads as a bug: a file edited since the last walk shows no Javadoc for the quarter-second the index is behind, rather than a neighbour's; `{@inheritDoc}` is dropped, not filled from the overridden method; and a Lombok getter shows its field's Javadoc, as Lombok copies it. The schema moved to 20, so the JDK is re-read once more. Slice 7: **a generic method's result is inferred from its arguments** — `Optional.of(s)` is an `Optional<String>`, `List.of(a, b)` a `List` of their type, and `names.stream().map(z -> z.trim())` a `Stream<String>`, so the chain after it, and the next lambda's parameter, hover, jump and complete. Three things so none reads as a bug: arguments of different types — `List.of("a", 1)` — or any argument whose type is not known infer nothing rather than their common supertype, a block lambda (`z -> { return …; }`) types nothing, and `List.of(array)` answers nothing; a method of the workspace's own types is not inferred; and `unresolved-method` never judges a call on an inferred receiver, by design. The schema moved to 21. Slice 8: **the most specific overload is picked, a string literal is a `String`, and a qualified name is a name** — ctrl-click on `log("x")` beside `log(String)` and `log(Object)` lands on `log(String)`, `f(1)` beside `f(int)` and `f(long)` on `f(int)`; `"x".` completes `String`'s members and `"x".lenth()` is flagged; each segment of `java.util.List`, `Map.Entry` and an import jumps and hovers, references on a type list its qualified uses and import lines, and `com.example.Missing` is flagged as an unresolved type. Three things so none reads as a bug: an overload only boxing or varargs decide — `f(Integer)` beside `f(Object)` at `f(1)` — still answers nothing; `java.util.List.of()` in an expression jumps nowhere from `List`; and a qualified name with a type in it — `Map.Entryy`, `Outer.Missing` — is never flagged, because an inherited member type could be meant. The schema moved to 23, so the JDK is re-read once more. Slice 9: **each module is judged against its own classpath** — in a multi-module Maven or Gradle build, importing a type that only *another* module's dependency holds is an `unresolved-type` warning, where the union of every module's classpath used to call it known. Three things so none reads as a bug: navigation still reads the union, so that import still jumps and hovers; a Maven module's main and test sources are one module, so a test library imported in main code is not flagged; and a file no module claims — a directory outside every source set, a Maven module with `pom` packaging — gets no squiggles at all. No schema change. Slice 10: **a hallucinated field and an unqualified call are flagged, and a call in a nested class reaches the class around it** — `name.lenght` on a `String` is an `unresolved-field` warning and `helpr()` that no type around it declares is an `unresolved-method` one, and ctrl-click on `outer()` inside a nested class lands on the outer class's method. Three things so none reads as a bug: a field through a type name the file itself declares is never flagged, nor is anything inside an enum, a record, an anonymous or a local class; a method reference and a bare field name are not judged; and a project whose JDK is newer than its Java level has no unqualified call judged in a type that extends a JDK class. Schema version 24, so the JDK is re-read once. | `an_external_type_name_jumps_to_its_own_name_in_the_jdks_sources`, `a_mapper_interface_is_implemented_by_the_processors_output`, `a_lombok_getter_and_builder_are_followed_into_the_type_that_generates_them`, `a_jar_member_jumps_into_its_sources_jar`, `a_calls_arguments_are_hinted_with_their_parameters_names`, `a_jdk_member_hovers_with_its_javadoc`, `a_lambdas_result_types_the_generic_call_it_is_passed_to`, `a_qualified_name_jumps_and_the_most_specific_overload_is_picked`, `a_dot_on_a_string_literal_offers_strings_members`, `a_type_only_another_modules_jar_holds_is_flagged`, `a_hallucinated_field_and_unqualified_call_are_flagged_and_a_nested_call_reaches_out`. |
| **5b — Authoring features** | Rename, code actions, auto-import and the rest of an author's verbs, out of version 1 — see [`roadmap.md`](roadmap.md). | Not written. Semantic tokens shipped in 5a above rather than here, and type and call hierarchy moved to 3d; extending `SURFACE` in `reachability.rs` is still the rule for whatever lands in this row. |
| **6 — Optimisation** | Nothing new; everything faster. | Not written. The comparison is against the previous phase's numbers rather than against a budget, and a large real project rather than the demo. |

Whatever the phase, the invariant holds: **the editor must be useful before the
slow work finishes.** If a feature appears only after a pause, that is a bug
even when the feature works.
`the_outline_comes_back_before_the_classpath_read_finishes` is the one
test that asserts it directly; every other test that installs a classpath
waits for `classpath ready` first, which is the opposite question.

## What the suite does not prove

Four things. The first three lost their only check when the human stopped
opening Zed, and are stated here rather than buried because the decision to
accept them is the substance of retiring the pass.

1. **Zed's rendering.** An answer that is correct on the wire and drawn wrongly
   in the editor is invisible to any harness.
2. **The extensions.** `extensions/zed` and `extensions/vscode` are launched by
   nothing in CI. The Zed one is real since the first release — it finds the
   binary, passes `initialization_options` and the binary's `env` — and its
   wasm build is checked by hand (`cargo build --release --target
   wasm32-wasip2` in `extensions/zed`), but only Zed proves it launches: that
   is step 0 of the first-release walk below.
3. **Client-model drift.** If Zed changes its trigger logic and the model does
   not, the suite goes on passing.
4. **The client model is pinned to nothing.** The design answered (3) with a
   captured Zed session, checked in and asserted against. It was not done: no
   such fixture exists and no test reads one. So the model in
   `crates/cli/tests/harness/editor.rs` is written from the LSP specification
   and from reasoning about what a client does with a capability, and it is
   pinned to one moment of Zed's behaviour no more than it is pinned to Zed's
   future. Capturing that trace is the work that would turn (3) from open into
   merely dated.

## Walking it by hand in Zed

Needed for one thing now: looking at what a real editor does — a rendering
question, or capturing the trace item (4) above names. The suite answers
everything else faster and on every machine.

Follow [`extensions/zed/README.md`](../extensions/zed/README.md) to build the
server, install Zed's Java extension, and install this repository's extension as
a dev extension.

Then create the local Zed settings for the demo project. They are gitignored
because they need an absolute path to your build:

```bash
mkdir -p tests/projects/zed-demo/.zed
cat > tests/projects/zed-demo/.zed/settings.json <<EOF
{
  "languages": {
    "Java": { "language_servers": ["javelin", "!jdtls"] }
  },
  "lsp": {
    "javelin": {
      "binary": {
        "path": "$PWD/target/release/javelin",
        "arguments": ["serve", "--log-level", "debug", "--log-file", "/tmp/javelin-zed.log"]
      }
    }
  }
}
EOF
```

`"!jdtls"` disables the Java extension's own language server, so you are looking
at Javelin and not at Eclipse JDTLS. Without it you cannot tell which one
answered.

Then `cargo build --release`, `zed tests/projects/zed-demo`, and
`tail -f /tmp/javelin-zed.log`. The three log lines a session opens with are
`starting language server`, `initialized` and `parsed document`; the gap
between the first two is the results table's startup column, and `duration_us`
on the third is its first parse. Read the note under that table before reading
anything into the first of those.

### Troubleshooting

**No log file.** Zed did not launch the binary. Usually the path in
`.zed/settings.json` is wrong or the release build does not exist.

**The log shows an old version string.** You are running a stale binary. Rebuild
and restart the language server, not just Zed.

**Answers you did not implement.** jdtls is still enabled. Confirm `"!jdtls"` is
in the project's `.zed/settings.json`, and that you are in the demo project
rather than another window.

**Orphan `javelin serve` processes.** Either a shutdown bug or leftovers from a
previous run. Note the start times before blaming the current build:

```bash
ps -o pid,lstart,command -p $(pgrep -d, -f 'javelin serve')
```

**`zed: install dev extension` fails to compile.** The extension targets
`wasm32-wasip2`. `rustup target add wasm32-wasip2`.

## The first-release walk — owed, for a human

Nothing from Phase 3b onwards has been walked in an editor: every row from 3b
to the first release passes in the automated pass and none is signed off.
This is that walk, once, for all of them, against **a real Maven project you
trust** — one with dependencies, ideally a service with a few hundred files —
because a toy project cannot show a dependency type resolving or a
diagnostic staying silent where it should. For step 8 to show method
warnings, pick a project whose Java level (`maven.compiler.release`,
`java.version`) equals the JDK's feature version, that uses no Lombok,
Manifold or AspectJ, and run `mvn generate-sources` first if it generates
code: otherwise the server is correctly silent on JDK-receiver and member
diagnostics, and those parts of step 8 are N/A, not failures. Each step says what to do and what
counts as a pass; tick it in the table at the end, and note anything else you
see. "Unknown" below means Zed shows nothing, which is the correct answer for
every refusal listed.

**0. Install and launch.** Follow the [README quick start](../README.md#quick-start-in-zed)
with these settings in the project's `.zed/settings.json`:

```json
{
  "languages": { "Java": { "language_servers": ["javelin", "!jdtls"] } },
  "lsp": {
    "javelin": {
      "binary": {
        "path": "/absolute/path/to/target/release/javelin",
        "arguments": ["serve", "--log-level", "debug", "--log-file", "/tmp/javelin-zed.log"]
      },
      "initialization_options": { "build": { "importOnStartup": true } }
    }
  }
}
```

`zed <project>`, `tail -f /tmp/javelin-zed.log`. **Pass:** the log shows
`initialized`, then `dependencies: asking the build tool`, and within the
build tool's own time `dependencies ready jars=N … complete=true`; and
`classpath ready` twice — `source="src.zip"` (about eleven seconds the first
time on a machine since the fact cache's schema moved to 23; `source="cache"`
on a later session) and then `source="dependencies"`; and `workspace ready`. Record the
`starting → initialized` gap, the first `parsed document`'s `duration_us`, and
both `classpath ready` durations for the results table.

**1. The open file, before anything has landed.** Open a large file at once.
**Pass:** the outline (`outline: toggle`), folding and semantic colouring are there
immediately, before `classpath ready`; typing stays smooth.

**2. Definition and hover.** Ctrl-click and hover on: a local, a field, a
method of the same class, a type in another file of the project, a method of
a type in another file, a JDK method (`s.length()` — lands in an extracted
`String.java`), and a dependency *member* (e.g. `mapper.writeValueAsString(x)`)
— whose hover shows a signature without parameter names and **no** jump
unless its `-sources.jar` is on disk (Tier 1's slice 4). Hover and ctrl-click on a
dependency or JDK *type name* (`ObjectMapper` in `ObjectMapper m`, `String`
in `String s`) answer nothing, also by design. **Pass:** every jump lands on the declaration's own name.

**3. Slice 1 — generics through the JDK.** Find or type, in a method body,
`List<String> names = …; names.get(0).length();`, a `Map<String, X>` with
`m.get(k).someMethodOfX()`, and an `Optional<X>` with `o.get().someMethod()`.
**Pass:** hover, ctrl-click and completion after the second `.` all work; a
raw `List raw; raw.get(0).` offers nothing (never `Object`'s methods); hover
on `get` reads `E get(int)`, as declared.

**4. Slice 2 — lambda parameters.** On `list.forEach(x -> x.foo())`,
`stream.filter(x -> …)` and `stream.map(x -> x.bar())` over a typed list or
stream from the project. **Pass:** hover, ctrl-click and completion on
`x.` work inside the lambda; after `.map(…)` the next `.` offers nothing (the
return type is not inferred); a lambda assigned to a variable,
`Function<A, B> f = a -> a.`, offers nothing.

**5. References (`editor: find all references`).** On a type, a method and a field the project
declares, each used from files you have not opened. **Pass:** the list holds
the uses in unopened files; `includeDeclaration` behaves as Zed shows it.

**6. Slice 3 — references through overrides.** Pick an interface the project
implements more than once, and a method of it called both through the
interface and through an implementation. **Pass:** references on the
interface method list both kinds of call; references on one implementation's
method list the calls through the interface and its own, and **not** the
calls through a sibling implementation.

**7. Implementations, hierarchies, symbols.** On that interface:
`editor: go to implementation` lists every implementation, and
`project symbols: toggle` finds a type and a method by part of its name.
The type and call hierarchies: if your Zed has a command for them, the type
hierarchy expands both ways and a method's incoming calls list its callers
grouped by method (calls through an override are not among them — by
design); if it has none, record N/A — both are covered over real pipes by
`implementations_and_callers_are_found_in_files_the_editor_never_opened`.

**8. Diagnostics.** With `importOnStartup` on and `dependencies ready ...
complete=true`: open a few ordinary files — **pass:** no warnings at all.
Then type a call to a method that does not exist on a JDK type — on a
declared `String s`, `s.lenth();`; a string literal's type is not known —
and on a project type, and a type name that does not exist (`Strng x;`).
**Pass:** a yellow warning on exactly the misspelt name, gone when you fix
it. **Also pass:** with `importOnStartup` *off* (restart the server after
changing it), the same project shows **no** diagnostics at all, deliberately.

**9. Following the workspace.** Add a method to a project type in one file,
save, and call it from another open file: completion offers it and no
warning appears. `git checkout` another branch with Zed open: navigation
follows within a second or two.

**10. Shutdown.** Quit Zed. **Pass:** `pgrep -f 'javelin serve'` finds
nothing.

**T2.7 Certain workspace chains.** Declare `class Entity { int size() {
return 1; } }` and `class Repo { Entity find() { return null; } }`, each in
its own file of a project with no build file. In a third file, write
`repo.find().nope()` and `repo.find().size()` for a `Repo repo`. **Pass:**
`nope` is underlined with a warning naming `Entity`, and `size` is not.

**T2.8 Class literals and for-each `var`.** With an `Entity` declaring
`size()`, write `for (var e : entities) { e.size(); }` for a
`List<Entity> entities`, and `Entity.class.cast(null).size()`. **Pass:**
ctrl-click on both `size` calls lands in `Entity.java`, and hover on
either `size` answers.

**T2.9 Annotation element names.** Declare `@interface Column { String
name(); }` in its own file. In another file, declare a field annotated
`@Column(name = "n") String name;`. **Pass:** ctrl-click on the key `name`
lands on `String name();` in `Column.java`, not on the field. Hover on the
key reads `String name()`.

**T3.1 Type inlay hints.** Turn inlay hints on in Zed. In a file of a
project with no build file, write `var t = s.substring(0, 1);` for a
`String s`, `names.stream().filter(n -> n.isEmpty())` for a
`List<String> names`, and a chain `s.trim()` / `.substring(0, 1)` /
`.length()` broken over three lines. **Pass:** `: String` appears after
`t` and after `n`, and after the first line of the chain and not after the
second, whose type is the same. A `var x = new StringBuilder();` shows no
type hint.

**T3.2 Go to type definition.** In the same file, with
`class Greeter {}` in a sibling file of the same package and a parameter
`Greeter g`, use Zed's "go to type definition" on `t`, and on a use of
`g`. **Pass:** the first lands on `class String` in the JDK's extracted
`String.java`, the second on `Greeter`'s name in its own file. On a `List<String>`
it lands on `List`, and on an `int` it does nothing.

**T3.3 Constructors as targets.** In a sibling file declare
`class Greeter { Greeter(String name) {} Greeter(String name, int times) {} }`.
In the open file write `new Greeter("a", 1)`. **Pass:** ctrl-click on
`Greeter` there lands on the two-parameter constructor, hover reads
`Greeter(String name, int times)`, and the arguments are hinted `name:` and
`times:`. Find references on that constructor lists the `new`, and on
`Greeter(String name)` does not. Find references on the class name still
lists the `new`.

**T3.4 Generator output.** In a project with no build file, put
`package com.acme; public final class Order { public String getId() { return null; } }`
at `build/generated/source/proto/main/java/com/acme/Order.java`, and in
`src/main/java/com/acme/Use.java` write `Object m(Order order) { return order.getId(); }`.
**Pass:** ctrl-click on `Order` and on `getId` lands in the generated
file, and `Ctrl-T` for `Order` lists it. Copy `Order.java` to
`src/main/java/com/acme/` and save: ctrl-click on `Order` now lands on the
hand-written copy.

**T3.5 Semantic tokens for a range.** Generate a file over 640 KiB, such as
a class with 20 000 one-line methods `int m0() { return 0; }`, and open it.
**Pass:** the methods' names and the `return` keywords are coloured as in a
small file, scrolling to the end colours the last screen within a moment,
and typing stays responsive. Note in the table whether Zed asks for `range`
at all (the server's `debug` log names each request). If it asks only for
`full`, the file keeps its TextMate colouring, and that is the finding.

**T3.6 The type hierarchy into the JDK.** In a project with no build file,
or with `build.importOnStartup` on, in `src/main/java/com/acme/Names.java` write
`public class Names extends java.util.ArrayList<String> implements Runnable { public void run() {} }`,
and open the type hierarchy on `Names`.
**Pass:**
- its supertypes are `ArrayList` and `Runnable`, each opening at its name
  in the JDK's source;
- `ArrayList` expands to `AbstractList`, `List`, `RandomAccess`,
  `Cloneable` and `Serializable`;
- `Runnable`'s subtypes list `Names`.

**T4.1 Method references.** In a project with no build file, write
`entries.stream().map(Map.Entry::getKey).map(k -> k.length())` for a
`List<Map.Entry<String, Integer>> entries`. **Pass:** hover on `length`
reads `int length()`, and ctrl-click on it lands in the JDK's `String.java`.

**T4.2 Block lambda returns.** Write
`names.stream().map(n -> { var t = n.trim(); return t; }).map(s -> s.length())`
for a `List<String> names`. **Pass:** hover on the second lambda's `s` and
on `length` answer, and ctrl-click on `length` lands in `String.java`.

**T4.3 Collectors.** Write
`var set = names.stream().collect(Collectors.toSet());` and then
`set.iterator().next().length()`. **Pass:** `set` is hinted `: Set<String>`,
and ctrl-click on `length` lands in `String.java`.

**T4.4 Bare inherited fields.** Declare
`public class Counted { protected int count; }` and, in another file,
`public class Tally extends Counted { int next() { return count + 1; } }`.
**Pass:** ctrl-click on `count` in `Tally.java` lands on the field in
`Counted.java`, and hover reads it. Close `Tally.java`, then find references
on `count` in `Counted.java`: the bare use in `Tally.java` is listed.

**T4.5 References on a JAR method.** With `build.importOnStartup` on, in a
project whose dependency declares a method — `Box.names()` in the suite's
`acme.jar`, or any library's — call it from two files and open only one.
**Pass:** find references on the call lists both files' calls.

**T4.6 Explicit constructor calls.** Declare
`class Greeter { Greeter(String name) {} Greeter(String name, int times) {} }`,
`class Loud extends Greeter { Loud() { super("a", 1); } }` in another file,
and `enum Day { MON(8); Day(int hours) {} }`. **Pass:** ctrl-click on
`super` lands on the two-parameter constructor in `Greeter.java`, and hover
on it reads `Greeter(String name, int times)`. Find references on that
constructor lists the `super` call. `8` in `MON(8)` is hinted `hours:`.

**T4.7 `@SuperBuilder` and `@Singular`.** With Lombok on the classpath,
declare `@SuperBuilder public abstract class Vehicle { private int wheels; }`
and `@SuperBuilder public class Truck extends Vehicle { @Singular private List<String> axles; }`,
each in its own file, and in a third write
`Truck.builder().axle("a").wheels(4).build()`. **Pass:** ctrl-click on
`wheels` lands on the field in `Vehicle.java`, on `axle` on `axles` in
`Truck.java`, and hover on `build` answers. No hint names a
`TruckBuilderImpl`.

**T5.1 Downloaded sources.** With `build.importOnStartup` on, in a project
using a library whose `-sources.jar` is not on disk, find one with
`ls ~/.m2/repository/org/slf4j/slf4j-api/*/`. Open a file calling
`LoggerFactory.getLogger(…)` and wait for `dependencies: sources fetched` in
the log. **Pass:** ctrl-click on `getLogger` opens `LoggerFactory.java` from
the cache's `jar-sources` directory, on the method's name. Hover shows its
parameter names and Javadoc, and the `-sources.jar` now sits beside the JAR.
Restart Zed: the log shows no second fetch, and the jump still lands.

**T5.2 `case` constants.** Open a file whose `switch` over an enum declared
in another file has a `case` label, ideally in a class that declares a field
of the same name. **Pass:** ctrl-click on the label opens the enum's file on
the constant, and hover shows it. Shift-F12 on the constant lists the label.

**T5.3 slf4j's overloads.** Open a file that logs with a format and two
values, `LOG.info("{} {}", a, b)`, and one with three values. **Pass:** hover
on `info` shows `info(String format, Object arg1, Object arg2)` for the
first and `info(String, Object...)` for the second, never an overload taking
a `Marker`, and ctrl-click lands on that overload in slf4j's `Logger.java`.

**T5.4 Explicit type arguments.** Open a file that starts a chain with
`Foo.<Bar>builder()` on a generic `@Builder` class, such as
`Report.<Entry>builder().items(…)`. **Pass:** ctrl-click on
the setter after `builder()` lands on the field of its name, and so does
each setter after it. Hover on `build()` shows the declared `Foo<T> build()`,
not `Foo<Bar>`: hover renders a member as declared.

| Step | Pass? | Notes |
|---|---|---|
| 0 — install and launch | | |
| 1 — the open file first | | |
| 2 — definition and hover | | |
| 3 — generics through the JDK | | |
| 4 — lambda parameters | | |
| 5 — references | | |
| 6 — references through overrides | | |
| 7 — implementations, hierarchies, symbols | | |
| 8 — diagnostics | | |
| 9 — following the workspace | | |
| 10 — shutdown | | |

## The Tier 1 walk — owed, for a human

The same setup as [the first-release walk](#the-first-release-walk--owed-for-a-human)
— its step 0, on the same project — and then one step per Tier 1 slice as it
lands.

**T1.1 External type names.** In any file, ctrl-click `String` in a
declaration such as `String s`. **Pass:** Zed opens `String.java` from the
cache's `jdk-sources` directory with the caret on the class's own name, and
hovering `String` reads `class java.lang.String`. Hover a type from a
dependency JAR: it reads `class <its fqn>`, and ctrl-click on it does nothing.

**T1.2 Processor output.** After a build has run (`./gradlew compileJava`),
open a MapStruct mapper interface and press `Ctrl-F12` on its name. **Pass:**
the list holds its `*MapperImpl` under `build/generated/sources/annotationProcessor`,
and choosing it opens the impl on the class's name; `Ctrl-F12` on one of the
mapper's methods lands on the impl's override. Open the impl: it shows no
squiggle, even after typing a nonsense type name into it.

**T1.3 Lombok members.** Open a service that reads a `@Data` or `@Value`
DTO or entity — for example, any service class calling a getter.
Hover a getter call, then ctrl-click it; type `dto.` and look at the list;
and on a `Foo.builder()…build()` chain, ctrl-click each hop. **Pass:** the
hover reads the getter's signature, the jump lands on the field's name in
the DTO, the list offers the getters and setters, each builder hop lands on
the field it sets, and `builder()` and `build()` land on the class's name.
`Shift-F12` on the DTO's field lists the getter calls in the service.

**T1.4 Sources JARs.** In IntelliJ or with `./gradlew` and the `idea`
plugin's `downloadSources`, fetch a dependency's sources once, so a
`-sources.jar` sits in `~/.gradle/caches/modules-2/files-2.1`. Restart Zed,
ctrl-click a method of that dependency — `objectMapper.readValue(…)` — and
its type name. **Pass:** Zed opens the dependency's own `.java` file from
the cache's `jar-sources` directory, on the method's or the class's name. A
dependency with no sources on disk still jumps nowhere.

**T1.5 Parameter names.** Turn on inlay hints in Zed's settings
(`"inlay_hints": { "enabled": true }`). In a service, look at a call to a JDK
method with arguments — `s.substring(0, 3)`, `list.add(0, x)` — and at one
to a method of the project. **Pass:** each argument is preceded by its
parameter's name, except where the argument already spells it; hovering the
JDK method reads its signature with parameter names.

**T1.6 Javadoc in hover.** Hover `s.substring(0, 3)`, a method of another
file of the project that has a Javadoc comment, and that file's class name.
**Pass:** each popup shows the signature and, under it, the comment as
formatted text — code in backticks, a parameter list, paragraphs — with no
`/**`, `*` or HTML tags left in it.

**T1.7 Generic inference.** Find a stream pipeline —
`list.stream().map(x -> x.getName()).filter(n -> n.isEmpty())` — and hover
`n`, then ctrl-click `isEmpty`. Do the same after `Optional.of(value)`.
**Pass:** `n` is the type `map`'s lambda returns and `isEmpty` jumps into
`String.java`; `Optional.of(value).get()` is `value`'s type.

**T1.8 Most specific, literals, qualified names.** Ctrl-click a call to an
overloaded method where both overloads fit and one is narrower —
`log("x")` beside `log(String)` and `log(Object)`, `sb.append('c')`, a
`f(Dog)` beside `f(Animal)`. Type `"x".` and look at the completion list.
Ctrl-click each segment of a written-out `java.util.List`, of `Map.Entry`,
and of an import. Write `com.example.Missing m;` in a class body.
**Pass:** each call lands on the narrower overload; `"x".` offers `length`;
each segment lands on its type, a package segment nowhere; references on a
type list its fully-qualified uses and its import line; and
`com.example.Missing` is a warning, while `java.util.HashMap.Entry` is not.

**T1.9 A classpath per module.** In a module whose build does not depend on
some library another module of the build does use, import a type from that
library. Then import the same type in a file of the module that depends on
it. **Pass:** the first import is an `unresolved-type` warning and still
ctrl-clicks into the library; the second is silent. A file outside every
module's source directories shows no warnings at all.

**T1.10 Fields and unqualified calls.** Write `name.lenght` for a `String
name`, and `helpr()` in a class that declares no `helpr`. Then, inside a
nested class, call a method only the outer class declares and ctrl-click it.
**Pass:** `lenght` is an `unresolved-field` warning naming
`java.lang.String`, `helpr` an `unresolved-method` warning, and the
ctrl-click lands on the outer class's method. `Map.Entry` written as an
expression is not flagged.

**T2.1 Own build output.** In a Gradle multi-module project with
`build.importOnStartup` on and a module whose `build/libs` JAR is on another
module's classpath, call a method that does not exist on a type of the first
module, from the second. **Pass:** the call is an `unresolved-method`
warning, where before it was silent; a type also held by a JAR outside the
project stays silent.

**T2.2 Members under Lombok.** In a project with Lombok on the classpath,
call `car.getNmae()` on a `@Data class Car { String name; }`, and
`b.anything()` on a `@SuperBuilder` class. **Pass:** the first is an
`unresolved-method` warning, `car.getName()` is silent, the second is
silent, and in a class annotated `@ExtensionMethod` no call is flagged.

**T2.3 Implicit enum and record members.** Open a file calling
`entry.date()` on a `record Entry(String date)` and `Day.valueOf("MON")` on
an `enum Day`, and add `day.nope()`. **Pass:** ctrl-click on `date` lands on
the component in `Entry.java`, hover on it reads `String date()`,
`Day.valueOf` is silent, and `day.nope()` is an `unresolved-method` warning.

**T2.4 Static imports navigated.** In a file with `import static
java.util.List.of;` and `import static` of a workspace constant, call
`of("a")` and use the constant as `MAX.length()`. **Pass:** ctrl-click on
`of` lands in `List.java`, ctrl-click on `MAX` lands on the constant's
declaration, hover on `length` answers, and references on the constant list
the use from its declaring file.

**T2.5 Workspace generics.** Declare `interface Repo<T> { Optional<T>
find(); }`, `interface EntityRepo extends Repo<Entity> {}` and an `Entity`
with a method `name()`, each in its own file. In another file, write
`repo.find().get().name()` for a `Repo<Entity> repo`, and the same for an
`EntityRepo`. **Pass:** ctrl-click on `name` lands in `Entity.java` both
times, and hover on `find` answers.

**T2.6 Lombok's log field.** In a class annotated `@Slf4j`, with
`org.slf4j:slf4j-api` on the classpath, write `log.info("x")`. **Pass:**
ctrl-click on `log` lands on the class's annotations, hover on `log` names
`org.slf4j.Logger`, and ctrl-click on `info` lands in `Logger`. With
`lombok.log.fieldName = LOG` in a `lombok.config` beside the class, `LOG`
answers in the same three ways.

| Step | Pass? | Notes |
|---|---|---|
| T1.1 — external type names | | |
| T1.2 — processor output | | |
| T1.3 — Lombok members | | |
| T1.4 — sources JARs | | |
| T1.5 — parameter names | | |
| T1.6 — Javadoc in hover | | |
| T1.7 — generic inference | | |
| T1.8 — most specific, literals, qualified names | | |
| T1.9 — a classpath per module | | |
| T1.10 — fields and unqualified calls | | |
| T2.1 — own build output | | |
| T2.2 — members under Lombok | | |
| T2.3 — implicit enum and record members | | |
| T2.4 — static imports navigated | | |
| T2.5 — workspace generics | | |
| T2.6 — Lombok's log field | | |
| T2.7 — certain workspace chains | | |
| T2.8 — class literals and for-each `var` | | |
| T2.9 — annotation element names | | |
| T3.1 — type inlay hints | | |
| T3.2 — go to type definition | | |
| T3.3 — constructors as targets | | |
| T3.4 — generator output | | |
| T3.5 — semantic tokens for a range | | |
| T3.6 — the type hierarchy into the JDK | | |
| T4.1 — method references | | |
| T4.2 — block lambda returns | | |
| T4.3 — collectors | | |
| T4.4 — bare inherited fields | | |
| T4.5 — references on a JAR method | | |
| T4.6 — explicit constructor calls | | |
| T4.7 — `@SuperBuilder` and `@Singular` | | |
| T5.1 — downloaded sources | | |
| T5.2 — `case` constants | | |
| T5.3 — slf4j's overloads | | |
| T5.4 — explicit type arguments | | |

## Recording the result

A phase's *checks* are automated: the two test binaries run them on every gate,
and nothing here is performed by hand. A phase's *row in the table below* is
not — it is a startup, first-parse, reparse and `classpath ready` figure
measured by a person in a real editor on a named date, added by hand alongside
the Criterion output in [`performance.md`](performance.md).
Editor-observed latency and benchmark latency answer different questions, and
only the first is what a user experiences.

Three files hold numbers now and they are three different kinds.
[`editor-pass-results.md`](editor-pass-results.md) is rewritten whole by
`scripts/check.sh` from `the_measured_numbers_are_recorded` and committed, which
is why its figures are deliberately coarse: whole-unit upper bounds, because a
precise figure in a file the gate regenerates would report "the numbers moved"
on every single run and teach its reader to ignore the one time they did. It is
also a debug build, being what `cargo test` builds.
[`performance.md`](performance.md) is where precise, release-build figures live.
The table below is neither — it is a hand-measured record of walks in a real
editor on named dates, so it is never regenerated and never edited. A row is
added when a phase is signed off; the rows above it stay as they were measured.

So the two halves of a phase's sign-off are answered in different places: its
checks pass or fail on the gate, and its row here is a hand-measured number
nothing regenerates.

**Open, and the maintainer has not decided it: whether a phase should still
require a hand-measured row at all**, now that every check it used to accompany
runs automatically. Until that is settled, the rows below stand and the ones
already measured are not to be touched.

| Phase | Pass run | starting → initialized | First parse | Reparse per keystroke | `classpath ready` |
|---|---|---|---|---|---|
| **0 — Foundation** | yes | 21 ms | 767 µs | n/a — no incremental reparsing yet | n/a — no classpath |
| **1 — Interactive Java** | yes — 2026-09-03 | 0.8–1.3 ms | 346–935 µs | median 334 µs, p95 557 µs | n/a — no classpath |
| **2a — Member resolution** | yes — 2026-09-03 | 0.8–1.3 ms | 346–935 µs | median 334 µs, p95 557 µs | n/a — no classpath |
| **2b — `var` and the unqualified call** | yes — 2026-09-03 | 0.4–0.6 ms | 183–341 µs | median 316 µs, p95 430 µs | n/a — no classpath |
| **2c — overload resolution** | yes — 2026-09-04 | 88 ms on a cold editor launch, 1–3 ms of it Javelin's | 2.4 ms, with the scratch edit applied | median 724 µs, 5 samples | n/a — no classpath |
| **3a — the classpath floor** | yes — 2026-09-08 in Zed, after a pipe walk on 2026-09-07 | 66 ms on a cold `zed <project>` launch, 0.7 ms on an `editor: restart language server`, 2.4 ms launching into a Zed that was already up | 339 µs–1.02 ms, with the scratch edit applied (`recovered=true`, from its dangling `.`) | 244–801 µs, 8 samples, every one `incremental=true`, no `incremental=false` line at all | cold 13 043 ms and 11 012 ms `source="src.zip"`, warm **43 ms** `source="cache"`, 24 467 facts — three Zed sessions |
| **3b–3d, 4, 5a and the first release** | | | | | |

Phase 3a's row is filled and **the phase is signed off.** It was walked twice:
over real pipes on 2026-09-07 against the release binary, and in Zed on
2026-09-08. All eleven feature checks passed on the first attempt both times.
The numbers in the row are the Zed ones; the pipe figures are kept below,
labelled, rather than averaged in.

**What the Zed walk added, and it is the whole reason it was owed.** The pipe
walk proved the four features answer correctly; it could not prove Zed can
*invoke* them, which is exactly the gap that shipped completion after a `.`
dead for a whole phase in 2a. It can: every answer in the table above was
obtained from Zed's own request traffic, so `capabilities.rs` being unchanged
from 2c is now a proven sufficiency rather than an argument for one. And the
startup column is once again what it is in the rows above it — a client-inclusive
gap between the server's first and third log lines — so 3a is comparable with
2c's 88 ms again, where the pipe-measured 9.1 ms cold / 15.1 ms warm was not.

Three Zed sessions, and the shape of them matters for reading the row. The first
was a cold `zed tests/projects/zed-demo` launch with the classpath cache deleted:
66 ms to `initialized`, `classpath ready` after 13 043 ms of `src.zip`, and all
eight of the row's reparse samples, since it is the session the feature checks
were typed in. The second was an `editor: restart language server` with the
cache deleted again — 0.7 ms to `initialized`, 11 012 ms of `src.zip`, and the
row's fastest first parse at 407 µs. The third was a fresh launch against the
cache the second session left behind: `classpath ready` in
**43 ms** `source="cache"`, the same 24 467 facts, and 2.4 ms to `initialized`
because Zed was already up and sent `initialize` at once — which is the closest
thing in the row to Javelin's own share of startup, and it agrees with the 1–3 ms
2c measured three ways. 43 ms against 13 043 ms is a 300× difference and the
cache's entire justification. It is three times the pipes' 14–18 ms; both are
tens of milliseconds, which is what the check asks for.

`shutdown`/`exit` completed cleanly on the first two sessions. The third logged
no `shutdown requested` at all — quitting Zed took the process down without an
LSP shutdown — and left no orphan behind (`pgrep -fl 'javelin serve'` printed
nothing), so this is the client's choice of exit, not our shutdown path. The
path itself is exercised by the two sessions that did request it, and by
`crates/cli/tests/stdio_lifecycle.rs`.

The pipe walk was itself run twice: once at `0f50abc` and again after the
whole-branch review's fix wave (`88056e6`), which changed `TypeWorld::is_subtype`
and marked an enum's, a record's and an annotation type's closure incomplete. All
eleven checks passed both times. The re-run was owed rather than optional, because
`Visit` in the demo file is a *record*: the `accept` check's exclusion could
plausibly have depended on the behaviour the fix changed. It does not — that
exclusion is decided by the file-local walk, which the fix did not touch — but
that was worth measuring rather than reasoning about.

What both walks established, per check: definition on `greeter.mark(1)` lands on
`mark(int count)` — the check the row warned might fail against a real `src.zip`
rather than the hand-built world it was written against, so the JDK's own
`java.lang.Integer` closure is complete enough to answer. Definition on
`greeter.accept(new Tally())` lands on `accept(Tally tally)`, closing the
supertype-closure refusal. `widen` answers nothing, as most-specific selection
requires. `greeting.` offers an empty list. `System.out.println` answers nothing
at every position in it. No `publishDiagnostics` was sent in any session, from
either `Greeter.java` or `Broken.java`. Hover agreed with definition at each
position and still rendered declaration source text rather than a resolved type.
And before `classpath ready` appeared, the outline, completion and definition
inside the file all worked and `mark` answered nothing — the editor stays useful
for the eleven seconds the read takes, which is the check that matters most.

The Phase 1 and Phase 2a passes were walked in one sitting, so those two rows
share their numbers; the feature checks were run per phase. Ranges cover four
server sessions on `Greeter.java` (57 lines). Reparse is 81 `incremental=true`
samples, min 146 µs, max 655 µs.

Phase 2b's row was measured on its own pass against its own binary. Two server
sessions on the same `Greeter.java`, 37 `incremental=true` samples, min 161 µs,
max 462 µs, and **no `incremental=false` line at all** — every edit in the pass
reparsed from the installed tree rather than falling back to a full parse.
`Broken.java`'s first parse is excluded from the range, as in the rows above; it
recovers in 33–38 µs because it is a handful of lines.

Every 2b check in the table above passed: completion at `greeter.` offered
`Greeter`'s instance members where 2a offered nothing, definition on `greet`
landed on line 25, definition and hover on the unqualified `timesSeen()` landed
on line 35 and rendered `public int timesSeen()`, and `System.out.println` still
answered nothing at every position in it.

The numbers are lower than Phase 2a's across all three columns. Nothing in 2b
set out to make anything faster, and one session against another on a 57-line
file is not evidence that it did — read this as machine noise on an already
sub-millisecond path, not as an improvement to defend.

**The startup column measures the client as much as the server, and Phase 2c's
pass is what established that.** These numbers are the gap between the server's
own first and third log lines. They exclude process spawn, which the log cannot
see — but they *include* the time the client spends before it sends
`initialize`, because the server spends that time blocked on a read. So the
column is not comparable across rows measured under different client
conditions, and the sub-millisecond figures in the Phase 1, 2a and 2b rows are
`editor: restart language server` — an editor that is already up and sends
`initialize` at once — while Phase 2c's 88 ms is a cold `zed <project>` launch,
where Zed has its own startup to do first. Neither is wrong; they answer
different questions, and the row that looks 150x worse is the more honest one
about what a user waits for.

Javelin's own share of that 88 ms is 1–3 ms, measured rather than assumed:
driving the same release binary over pipes answers 1.1 ms with an empty
capabilities object, 2.7 ms with a full editor-shaped one, and 1.5 ms from a
binary inode the page cache has never seen. Nothing in the handshake scales
with what the client sends, and the 100 ms budget is met with room.

Two behaviours the in-process tests describe were seen live rather than
inferred: one `incremental=false` fallback per burst of fast typing, preceded by
`edits do not describe the tree on hand` (the batching ceiling in
`crates/server/src/state.rs`, firing once in ~74 keystrokes, not on every one),
and `answering from the previous view while a reparse is in flight
answering=44 buffer=45` — a request served from the last coherent view instead
of blocking.

Phase 2c's pass was walked in two halves on the same day, and the split is
worth knowing because the two halves disagreed about one number. Every one of
the eleven feature checks its row above prescribes was first run over real
pipes against the release binary — definition and hover choosing
`note(int count)` for `note(1)` and `note(Visit visit)` for
`note(new Visit(…))`, both refusing `mark(1)` because `String` is the JDK's,
both separating the field `tally` from the method `tally(int extra)`, and
completion at `greeter.note(1).` offering exactly the eleven instance members
and no static one — and then walked again in Zed, where all eleven behaved the
same. That is the half only a real editor can answer, and the half that caught
2a's bug: whether the popup opens on the `.` and the jump moves the cursor at
all.

The numbers in the row are Zed's, from one cold-launch session with the scratch
edit applied. That is why the first-parse cell reads 2.4 ms rather than the
sub-millisecond figures above it: the scratch edit contains a deliberate
dangling `.`, so the parse ran with error recovery on a file a dozen lines
longer. `Broken.java` recovered in 248 µs in the same session. The reparse cell
is five `incremental=true` samples, 435–1213 µs, and — as in 2b — **no
`incremental=false` line at all**.

The pipe-driven half also measured the reparse path over four sessions on the
*unmodified* `Greeter.java` with 160 human-paced single-character `didChange`
notifications, which is what makes it comparable to 2b's row: median 495 µs,
p95 998 µs, first parse 188–457 µs, no fallbacks. It reported the handshake as
0.4–2.0 ms, and Zed reported 88 ms — see the note above for why both are right.

The reparse median is 495 µs against 2b's 316 µs, and that is the machine, not
the phase. The same harness was run against a release build of `04321c6` — the
commit 2c branched from — on the same machine under the same load, and it
answered median 482 µs, p95 936 µs, first parse 386–744 µs. Phase 2c's median
sits 2.7% above its own baseline's, which is noise, and both sit well above
2b's recorded figure because this pass ran with a Zed session and two 1 GB-heap
JDTLS JVMs alive. Read the 2b→2c difference as load; read the A/B as the
evidence that 2c did not cost anything measurable on the reparse path. Note that
`reparsed document`'s `duration_us` covers `Analysis::warm`, so it includes the
`FileSyntax` and `ScopeTree` rebuild and not only the tree reparse — which is
why the A/B was worth running rather than reasoning about.

## What the pass caught

Completion after `.` was unreachable from Zed. `capabilities.rs` advertised
`trigger_characters: None`, carrying a Phase 1 comment that a member selection
"needs the receiver's type, which is not something this phase knows". Phase 2a
made that false and the handshake was never updated, so a client that only asks
after an identifier character never sent the request. Member resolution worked;
nothing could reach it.

Every in-process test calls the handler directly, so all 388 of them passed
against a feature no editor could invoke. This is the second bug of exactly that
shape — after the Phase 0 `IoThreads::join()` deadlock — and the reason the two
classes of failure at the top of this file are checked at all. They cost two
minutes of a person's time per phase when this was a walk; they cost a test run
now, which is why the walk could be retired without dropping them.

Phase 2a still shipped ahead of Phase 1's pass rather than waiting for it. That
broke the rule this document exists to enforce, and the debt is recorded here
rather than quietly carried forward. Every row through 3a is now filled, and 3a's
was measured on its own walk rather than borrowed from 2c's row — a startup
figure copied across a phase boundary is not a measurement of anything. Its
`classpath ready` cell carries both numbers the cache exists to contrast, a cold
one and a warm one; benchmarked, they are 11 175 ms and 15–38 ms, and in Zed they
are 13 043 ms and 43 ms. Phase 3b's row is the next one owed: its checks run on
every gate, and the row itself is still a figure someone measures in a real
editor — subject to the open question above about whether that is still
required.

**Both of those numbers have since moved, and a walker should expect it rather
than treat it as a regression.** 3b's external-members slice put ~208 000
member rows in the fact cache beside its ~24 000 type rows, and over real pipes
on 2026-09-09 the cold read measured 11 334 ms and the warm read **103–139 ms**
against 3a's 15–38 ms, with the cache file at 27.7 MB against 5.7 MB. The warm
figure is still inside the "warm startup in hundreds of milliseconds" target,
but the margin is ~4× narrower than 3a's — 139 ms at its worst where 3a's was
38 ms — and it is a margin of a few times rather than a comfortable one. That
shrinking, not the absolute number, is the point of saying so here. Nothing
waits on it either way: the read starts from `initialized`, on a detached
thread.
Numbers and method: [`performance.md`](performance.md).
