```text
NISSE 1.0 — SOURCE INDEX LANGUAGE ADDITIONS
=============================================

STATUS
------

This document is a frozen implementation contract for extending Nisse's
existing source index.

It adds first-class source indexing support for:

    Python
    Bash
    Lua
    HTML

The implementation must preserve the existing indexing architecture,
content-addressed cache semantics, snapshot isolation, worktree behavior,
and retrieval contracts.

This document does not authorize redesign of the source index.

The implementation agent is expected to be Pi.

IMPORTANT:

Assume the implementation agent may be relatively weak.

Therefore:

- follow the explicit language contracts below;
- do not invent additional language semantics;
- do not replace the existing LanguageIndexer abstraction;
- do not redesign the existing Tree-sitter integration;
- do not change existing language behavior unless necessary to fix a
  demonstrated regression caused by this addition.


======================================================================
0. MANDATORY IMPLEMENTATION LOOP
======================================================================

IMPLEMENT THIS ADDITION AS ONE COMPLETE CHANGESET.

Pi MUST perform this exact sequence:

    1. INSPECT CURRENT REPOSITORY STATE
    2. INSPECT THE EXISTING SOURCE INDEX IMPLEMENTATION
    3. IMPLEMENT THIS DOCUMENT ONLY
    4. RUN ALL RELEVANT TESTS
    5. REVIEW THE IMPLEMENTATION AGAINST THIS DOCUMENT
    6. FIX ALL DISCOVERED DEVIATIONS
    7. RUN TESTS AGAIN
    8. REVIEW AGAINST THIS DOCUMENT AGAIN
    9. COMMIT THE COMPLETED ADDITION WITH GIT
   10. VERIFY THE WORKING TREE
   11. REPORT
   12. STOP

Do not continue into unrelated work after the commit.


----------------------------------------------------------------------
0.1 START OF IMPLEMENTATION
----------------------------------------------------------------------

Inspect:

    git status --short
    git log --oneline --decorate -20

Inspect the existing source-index implementation, especially:

- LanguageIndexer abstraction;
- language registry;
- path/language detection;
- Tree-sitter parser construction;
- ParsedArtifact;
- symbol representation;
- reference representation;
- import/dependency representation;
- source skeleton generation;
- content-key handling;
- parser/index versioning;
- SQLite persistence;
- incremental refresh tests;
- snapshot/worktree isolation tests.

Use the existing architecture.

Do not create a parallel index implementation.


----------------------------------------------------------------------
0.2 PRE-EXISTING CHANGES
----------------------------------------------------------------------

Never destroy, overwrite, stage, or commit unrelated user changes.

If unrelated uncommitted changes exist:

- leave them untouched;
- do not include them in this commit;
- work around them where safe.

If implementation cannot proceed without ambiguously mixing with unrelated
changes:

- stop;
- report the exact conflict;
- do not guess.


----------------------------------------------------------------------
0.3 TESTING
----------------------------------------------------------------------

Run all existing source-index tests.

Also run all new tests required by this document.

Where applicable run:

    cargo fmt --check
    cargo clippy --all-targets --all-features -- -D warnings
    cargo test --all-targets --all-features
    git diff --check

The default test suite must not require Internet access.

All grammar dependencies required for normal builds must be declared through
the repository's normal dependency mechanism.


----------------------------------------------------------------------
0.4 MANDATORY SELF-REVIEW
----------------------------------------------------------------------

After tests pass:

1. Re-check every requirement in this document.

2. Inspect every changed file.

3. Verify:
   - all four languages are registered;
   - path/shebang detection is correct;
   - symbol extraction follows the contracts below;
   - import/dependency extraction follows the contracts below;
   - references are bounded and useful;
   - source skeletons are useful;
   - content-addressed caching still works;
   - existing languages are unchanged;
   - snapshot/worktree isolation remains intact;
   - no unsupported extra language scope was introduced.

4. Fix every known deviation.

5. Re-run all tests.

6. Perform the review again.

Do not commit while a known deviation remains.


----------------------------------------------------------------------
0.5 COMMIT
----------------------------------------------------------------------

Before committing:

    git status --short
    git diff
    git diff --cached

Stage only files related to this addition.

Commit exactly:

    git commit -m "nisse: add python bash lua and html indexing"

Do not amend an earlier commit.

After committing:

    git status --short

Verify no changes belonging to this addition remain uncommitted.

Report:

- commit hash;
- files changed;
- grammar dependencies added;
- tests executed;
- self-review result;
- acceptance result;
- unrelated working-tree changes left untouched.

Then STOP.


======================================================================
1. ADDITION SCOPE
======================================================================

Add first-class indexing for:

    Python
    Bash
    Lua
    HTML

The existing index must continue supporting all previously implemented
languages.

The complete language set after this addition includes the existing language
set plus these four languages.

Do not remove or weaken any existing language support.


======================================================================
2. ARCHITECTURAL INVARIANTS
======================================================================

The following invariants are mandatory.

1. Each new language uses the existing LanguageIndexer abstraction.

2. Each new language participates in the existing language registry.

3. Tree-sitter remains the baseline parser.

4. Parsed artifacts remain content-addressed.

5. Parser/index version remains part of cache interpretation.

6. A Git blob already parsed with the same language/index version is reusable
   across branches and linked worktrees.

7. Dirty/untracked content uses the existing non-Git content-key mechanism.

8. Path-to-content membership remains snapshot/worktree-specific.

9. Shared cache contents do not prove snapshot membership.

10. Retrieval APIs do not require special-case code for these languages
    beyond ordinary language-index metadata.

11. Existing languages must retain their existing behavior.

12. No continuously running language server is introduced.

13. No LLM is used during indexing.

14. Indexing must be deterministic for identical input bytes and index
    version.

15. Syntax errors must not cause the entire project refresh to fail when
    Tree-sitter can still produce a partial tree.

16. One bad file must not prevent other files from being indexed.


======================================================================
3. GRAMMAR SELECTION
======================================================================

Use established Tree-sitter grammars.

Preferred grammar sources:

Python:

    tree-sitter/tree-sitter-python

Bash:

    tree-sitter/tree-sitter-bash

Lua:

    tree-sitter-grammars/tree-sitter-lua

HTML:

    tree-sitter/tree-sitter-html


Use Rust-compatible grammar bindings compatible with the Tree-sitter version
already used by Nisse.

IMPORTANT:

Do not downgrade the existing Tree-sitter runtime merely to accommodate one
grammar package.

If a published grammar crate has an incompatible Rust dependency declaration
but the grammar itself is compatible with the Tree-sitter language ABI:

- use a compatible released binding if available; or
- pin an appropriate upstream Git revision through Cargo if necessary.

Keep the workaround local to that grammar dependency.

Do not fork or copy grammar source into Nisse unless there is no reasonable
dependency-based alternative.

Document any nonstandard dependency pin in a short source comment or project
documentation.


======================================================================
4. LANGUAGE DETECTION
======================================================================

Language detection must use the existing detection architecture.

Add the following mappings.


----------------------------------------------------------------------
4.1 PYTHON
----------------------------------------------------------------------

Recognize by extension:

    .py
    .pyi

Also recognize extensionless executable/text files whose first line is a
Python shebang.

Accept common forms such as:

    #!/usr/bin/python
    #!/usr/bin/python3
    #!/usr/bin/env python
    #!/usr/bin/env python3
    #!/usr/bin/env -S python ...
    #!/usr/bin/env -S python3 ...

Do not attempt arbitrary shell parsing of the shebang.

Implement bounded, deterministic recognition of common Python interpreter
forms.


----------------------------------------------------------------------
4.2 BASH
----------------------------------------------------------------------

Recognize by extension:

    .sh
    .bash

Also recognize extensionless files with a shell shebang identifying:

    bash
    sh

Examples:

    #!/bin/bash
    #!/usr/bin/bash
    #!/bin/sh
    #!/usr/bin/env bash
    #!/usr/bin/env sh
    #!/usr/bin/env -S bash ...

The index language ID remains:

    bash

even when the shebang is `sh`.

Do not introduce a separate POSIX-sh indexer in this addition.

Do not classify arbitrary extensionless files as Bash without a recognized
shebang.


----------------------------------------------------------------------
4.3 LUA
----------------------------------------------------------------------

Recognize:

    .lua

Also recognize extensionless files whose first line clearly identifies a Lua
interpreter, for example:

    #!/usr/bin/lua
    #!/usr/bin/lua5.4
    #!/usr/bin/env lua

Do not add Luau as part of this addition.

Do not add a separate LuaJIT-specific language ID.


----------------------------------------------------------------------
4.4 HTML
----------------------------------------------------------------------

Recognize:

    .html
    .htm

Language ID:

    html

Do not add:

    JSX
    TSX
    Vue
    Svelte
    Astro
    XML

as part of this addition.

Existing JavaScript/TypeScript support remains responsible for standalone JS
and TS files.


======================================================================
5. COMMON PARSED ARTIFACT CONTRACT
======================================================================

Each new language must populate the existing ParsedArtifact fields where the
language provides meaningful data.

At minimum attempt to produce:

    language
    content_key
    symbols[]
    imports/dependencies[]
    references[]
    source spans
    compact source skeleton

Do not invent fake semantic precision.

When a language construct cannot be resolved statically:

- record only the syntactic relationship that is actually known;
- do not guess runtime targets.


======================================================================
6. PYTHON INDEXING CONTRACT
======================================================================

Language ID:

    python


----------------------------------------------------------------------
6.1 PYTHON SYMBOLS
----------------------------------------------------------------------

Index as symbols:

- module-level function definitions;
- async function definitions;
- class definitions;
- methods inside classes;
- async methods inside classes.

Where the existing symbol model supports nesting/qualified names, use
qualified names.

Examples:

    parse_packet
    Client
    Client.connect
    Client.__init__


Decorators are not separate symbols.

Nested functions MAY be indexed if the existing index architecture naturally
supports nested symbols.

They are not required for acceptance.

Do not treat every variable assignment as a symbol.

Module-level named constants/assignments may be indexed only if the existing
symbol abstraction already has a natural variable/constant category.

Do not introduce a new general variable-symbol system solely for Python.


----------------------------------------------------------------------
6.2 PYTHON IMPORTS
----------------------------------------------------------------------

Extract syntactic imports from:

    import foo
    import foo.bar
    import foo as bar

    from foo import x
    from foo.bar import x
    from . import x
    from ..foo import x


Preserve enough information to represent:

    imported module
    relative import level where applicable
    imported member where available


Do not attempt to execute Python import resolution.

Do not import modules.

Do not inspect the user's Python environment.


----------------------------------------------------------------------
6.3 PYTHON REFERENCES
----------------------------------------------------------------------

Extract useful identifier references on a best-effort syntactic basis.

Prioritize:

- function calls;
- method/attribute calls where name is available;
- referenced class/function identifiers.

Example:

    parse_packet(data)

may create a reference to:

    parse_packet


For:

    client.connect()

it is acceptable to record:

    connect

as an unresolved/member-style reference.

Do not claim that the target class is statically resolved unless Nisse
actually knows it.


----------------------------------------------------------------------
6.4 PYTHON SKELETON
----------------------------------------------------------------------

A Python source skeleton should preserve enough structure for retrieval while
removing large function bodies.

Prefer inclusion of:

- imports;
- decorators associated with indexed definitions;
- function/async function signatures;
- class declarations;
- method signatures;
- concise nesting structure.

Example conceptual skeleton:

    import asyncio
    from protocol import Packet

    class Client:
        def __init__(self, transport): ...
        async def connect(self): ...
        def parse(self, data): ...

    def make_client(config): ...


Do not include complete large function bodies in the skeleton.


----------------------------------------------------------------------
6.5 PYTHON TEST FIXTURES
----------------------------------------------------------------------

Include fixtures covering:

- simple function;
- async function;
- class;
- methods;
- decorator;
- import;
- from import;
- relative import;
- function call;
- syntax-error/partial file;
- `.pyi`;
- extensionless Python shebang.


======================================================================
7. BASH INDEXING CONTRACT
======================================================================

Language ID:

    bash


----------------------------------------------------------------------
7.1 BASH SYMBOLS
----------------------------------------------------------------------

Index shell function definitions.

Support common forms:

    function foo {
        ...
    }

    function foo() {
        ...
    }

    foo() {
        ...
    }


Symbol name:

    foo


Do not index every shell variable assignment as a global symbol.

Do not attempt runtime command discovery.


----------------------------------------------------------------------
7.2 BASH IMPORTS / DEPENDENCIES
----------------------------------------------------------------------

Treat static `source` / dot commands as file dependencies when the target is
a literal path.

Examples:

    source ./lib/common.sh
    . ./lib/common.sh

Index:

    ./lib/common.sh


For dynamic expressions:

    source "$DIR/common.sh"
    . "${BASE}/foo.sh"

record the textual dependency only if the existing dependency model supports
unresolved textual targets.

Do not guess the resolved filesystem path.

Never execute shell expansion.


----------------------------------------------------------------------
7.3 BASH REFERENCES
----------------------------------------------------------------------

On a best-effort basis, extract command/function invocation names where they
are syntactically clear.

Example:

    validate_config "$file"

may produce reference:

    validate_config


Avoid treating shell keywords as ordinary references.

Examples that should not become normal function references:

    if
    then
    fi
    for
    while
    do
    done
    case
    esac
    function


External command names may be retained as unresolved command references if
the existing reference representation supports this naturally.

Do not attempt to determine whether a command is:

- builtin;
- shell function;
- executable;
- alias.

That is runtime shell semantics.


----------------------------------------------------------------------
7.4 BASH SKELETON
----------------------------------------------------------------------

Skeleton should retain:

- shebang;
- static source/dot dependencies;
- function signatures/names;
- concise top-level structure where useful.

Example:

    #!/usr/bin/env bash
    source ./lib/common.sh

    validate() { ... }

    main() { ... }


Do not retain entire large function bodies.


----------------------------------------------------------------------
7.5 BASH TEST FIXTURES
----------------------------------------------------------------------

Cover:

- `foo()`;
- `function foo`;
- static `source`;
- static `. file`;
- dynamic source path;
- function invocation;
- shell keyword handling;
- `.sh`;
- `.bash`;
- extensionless bash shebang;
- extensionless sh shebang;
- partial/broken shell syntax.


======================================================================
8. LUA INDEXING CONTRACT
======================================================================

Language ID:

    lua


----------------------------------------------------------------------
8.1 LUA SYMBOLS
----------------------------------------------------------------------

Index function definitions.

Support common forms such as:

    function foo()
    end

    local function foo()
    end

    function module.foo()
    end

    function object:method()
    end


Use the syntactic qualified form when available:

    foo
    module.foo
    object:method


Do not introduce type inference.

Table assignments are not automatically symbols.

A table assigned at module scope may be indexed only if the existing symbol
model already has an appropriate variable/module symbol category.

Do not create new general table-analysis infrastructure for this addition.


----------------------------------------------------------------------
8.2 LUA IMPORTS / DEPENDENCIES
----------------------------------------------------------------------

Recognize common literal `require` calls:

    require("foo")
    require 'foo'
    local foo = require("foo")


Record dependency:

    foo


Do not evaluate:

    require(prefix .. ".foo")

Do not execute package searchers or inspect runtime `package.path`.


----------------------------------------------------------------------
8.3 LUA REFERENCES
----------------------------------------------------------------------

Extract useful call references on a best-effort syntactic basis.

Examples:

    foo()

reference:

    foo


    module.foo()

reference:

    module.foo

where syntactically available.


    object:method()

reference:

    object:method

or equivalent existing member-reference representation.


Do not claim runtime dispatch resolution.


----------------------------------------------------------------------
8.4 LUA SKELETON
----------------------------------------------------------------------

Skeleton should preserve:

- literal require statements;
- local/global function declarations;
- qualified/method function names;
- concise top-level module structure.

Example:

    local protocol = require("protocol")

    local function decode(data)
        ...
    end

    function M.send(packet)
        ...
    end


Large bodies are replaced by compact placeholders.


----------------------------------------------------------------------
8.5 LUA TEST FIXTURES
----------------------------------------------------------------------

Cover:

- global function;
- local function;
- dotted function;
- colon method;
- literal require with parentheses;
- literal require without parentheses;
- dynamic require;
- function calls;
- `.lua`;
- Lua shebang;
- partial/broken syntax.


======================================================================
9. HTML INDEXING CONTRACT
======================================================================

Language ID:

    html


HTML is structurally different from a programming language.

Do not force it into programming-language semantics.

Index useful document structure and static resource relationships.


----------------------------------------------------------------------
9.1 HTML SYMBOLS
----------------------------------------------------------------------

Create symbols for elements with an explicit:

    id

attribute.

Example:

    <section id="checkout">

symbol:

    checkout


The symbol span should cover the element start tag or the smallest useful
Tree-sitter node containing the ID.

Also index custom-element tag names when they contain a hyphen.

Example:

    <user-profile>

may produce symbol:

    user-profile


Ordinary standard tags such as:

    div
    span
    p
    body

must not become symbols merely because they occur.


Do not create a symbol for every CSS class.

Class attributes are too numerous/noisy for the primary symbol table.


----------------------------------------------------------------------
9.2 HTML DEPENDENCIES
----------------------------------------------------------------------

Extract static local/resource references where syntactically clear.

At minimum recognize:

    <script src="...">
    <link href="...">

Also recognize:

    <img src="...">

as a resource dependency if the existing dependency model supports generic
resource dependencies.


Preserve literal target text.

Examples:

    /js/app.js
    ./styles/main.css
    images/logo.svg


Do not fetch URLs.

Do not resolve HTTP resources.

Do not execute template syntax.

For clearly external URLs such as:

    https://...
    http://...

it is acceptable either to:

- record as external resource dependency; or
- omit from code relation traversal;

but behavior must be deterministic and tested.

Prefer preserving it as a dependency marked external if the existing model
supports such metadata.


----------------------------------------------------------------------
9.3 HTML REFERENCES
----------------------------------------------------------------------

Index static intra-document fragment references when literal.

Example:

    <a href="#checkout">

reference:

    checkout


The reference should be able to relate to:

    id="checkout"

when both exist in the same indexed document.


Other HTML references are best-effort and should remain conservative.

Do not attempt JavaScript execution or DOM analysis.


----------------------------------------------------------------------
9.4 HTML STRUCTURAL METADATA
----------------------------------------------------------------------

Where the existing artifact model permits it without architectural changes,
retain useful metadata for:

    title
    id-bearing elements
    custom elements
    script src
    link href


Do not create a new HTML-only database.


----------------------------------------------------------------------
9.5 INLINE SCRIPT AND STYLE
----------------------------------------------------------------------

Do not recursively parse inline:

    <script>...</script>

as JavaScript as part of this addition.

Do not parse inline:

    <style>...</style>

as CSS as part of this addition.

Do not add CSS indexing.

The HTML parser may preserve these regions as structural nodes/text, but
nested language injection is outside this contract.


----------------------------------------------------------------------
9.6 HTML SKELETON
----------------------------------------------------------------------

HTML skeleton should preserve useful structure while removing large text and
content bodies.

Prefer retaining:

- doctype;
- document title;
- elements with IDs;
- custom elements;
- script src elements;
- link href elements;
- concise ancestor structure needed to understand them.

Example conceptual skeleton:

    <!doctype html>
    <html>
      <head>
        <title>Checkout</title>
        <link href="styles/main.css">
        <script src="js/app.js"></script>
      </head>
      <body>
        <main id="checkout">
          ...
        </main>
        <user-profile>
          ...
        </user-profile>
      </body>
    </html>


Do not retain large prose bodies solely for the skeleton.

Full-text retrieval may handle textual content separately if the existing
index architecture already indexes text chunks.


----------------------------------------------------------------------
9.7 HTML TEST FIXTURES
----------------------------------------------------------------------

Cover:

- `.html`;
- `.htm`;
- id symbol;
- multiple IDs;
- custom element;
- ordinary tags not becoming symbols;
- CSS classes not flooding symbol table;
- script src;
- link href;
- img src where supported;
- fragment href -> ID reference;
- external URL;
- inline script;
- inline style;
- malformed/incomplete HTML.


======================================================================
10. LANGUAGE REGISTRY
======================================================================

Update the existing language registry so all four languages are ordinary
registered LanguageIndexer implementations.

Registry tests must prove:

    Python -> python indexer
    Bash   -> bash indexer
    Lua    -> lua indexer
    HTML   -> html indexer


Registry ordering must be deterministic.

Do not special-case these languages outside the normal registry except for
bounded file/shebang detection.


======================================================================
11. SHEBANG DETECTION
======================================================================

Implement shebang recognition through one shared deterministic helper if the
existing architecture does not already have one.

Read only the bounded beginning of the file needed for the first line.

Do not read the entire file merely to detect the language.

Detection order must be deterministic.

Extension-based detection should normally win when the extension is already
known.

Shebang detection primarily handles extensionless files.

If extension and shebang contradict each other:

    explicit recognized extension wins

unless the existing language detection contract already defines another
behavior.

Document and test whichever existing behavior is preserved.


======================================================================
12. PARSE ERROR BEHAVIOR
======================================================================

Tree-sitter may produce useful trees for incomplete/broken source.

For all four languages:

- retain useful extracted symbols/imports/references where possible;
- record parse-error metadata if the existing ParsedArtifact supports it;
- do not fail the entire indexing refresh merely because one file contains
  syntax errors.

Catastrophic parser failure for one file should produce a per-file indexing
error/diagnostic and allow other files to continue.


======================================================================
13. SOURCE SKELETON SIZE
======================================================================

Use the existing skeleton/context size policy.

Do not introduce unbounded skeletons.

Skeleton generation must avoid copying full large:

- Python functions;
- Bash functions;
- Lua functions;
- HTML text bodies.

The skeleton should be substantially smaller than the original for files with
large bodies while preserving useful searchable structure.


======================================================================
14. REFERENCES ARE BEST-EFFORT
======================================================================

For Python, Bash, and Lua, references are syntactic/best-effort.

Do not implement:

- Python type inference;
- Python runtime import resolution;
- Bash execution simulation;
- Bash PATH resolution;
- Lua runtime table resolution;
- Lua module loader execution.

For HTML, references are static structural references only.

The index must prefer:

    known syntactic fact

over:

    guessed semantic resolution.


======================================================================
15. DEPENDENCY RELATIONSHIPS
======================================================================

When the existing relation graph can map a dependency to a project file
deterministically, it may create a resolved project edge.

Examples where deterministic mapping MAY be possible:

Python:
    relative/local imports according to an existing explicit project mapping

Bash:
    literal relative `source ./foo.sh`

HTML:
    literal relative `script src="./app.js"`

Lua:
    literal require only if an existing deterministic project-module mapping
    is available


IMPORTANT:

Do not introduce complex language-specific package/environment resolution in
this addition.

Unresolved textual dependencies are acceptable and preferable to guessed
edges.


======================================================================
16. CONTENT-ADDRESSED CACHE BEHAVIOR
======================================================================

New languages must participate in existing cache semantics.

Required tests:

1. Initial file parse creates an artifact.

2. Re-indexing unchanged content reuses the artifact.

3. Editing content produces a different content key and new parse.

4. Reverting to previous tracked blob reuses the old artifact.

5. Same Git blob visible in another worktree reuses shared parsed artifact.

6. Worktree path mapping remains worktree/snapshot-specific.


Perform these tests for at least:

    Python
    Bash
    Lua
    HTML

It is acceptable to use a compact representative fixture for each.


======================================================================
17. EXISTING LANGUAGE REGRESSION PROTECTION
======================================================================

Run the full existing language-index test suite.

At minimum verify previously implemented languages still:

- register;
- detect;
- parse;
- extract symbols;
- reuse cached artifacts.

Do not change existing language semantics merely to make the new languages
fit.

If a common helper must change:

- preserve existing output;
- add regression coverage.


======================================================================
18. RETRIEVAL INTEGRATION
======================================================================

The new ParsedArtifacts must automatically participate in the existing
retrieval/index storage path.

Verify that indexed data from each new language can be found by ordinary
existing retrieval mechanisms.

Required retrieval-level checks:

Python:

    query a function/class name and retrieve the Python artifact.

Bash:

    query a shell function name and retrieve the Bash artifact.

Lua:

    query a Lua function/method name and retrieve the Lua artifact.

HTML:

    query an element ID/custom-element name and retrieve the HTML artifact.


Do not add four independent retrieval implementations.


======================================================================
19. TEST FIXTURE MINIMUMS
======================================================================

Create representative fixture files.

Suggested layout:

    tests/fixtures/index/python/
    tests/fixtures/index/bash/
    tests/fixtures/index/lua/
    tests/fixtures/index/html/

or follow the repository's existing fixture convention.


Minimum fixture behaviors:

PYTHON
------

    class
    method
    async method/function
    imports
    relative import
    function calls
    .pyi
    shebang
    malformed source


BASH
----

    functions
    source
    dot-source
    calls
    shebang
    dynamic source expression
    malformed source


LUA
---

    local function
    global function
    dotted function
    colon method
    require
    call
    shebang
    malformed source


HTML
----

    ids
    custom element
    script src
    link href
    fragment reference
    normal tags
    classes
    inline script/style
    malformed HTML


======================================================================
20. EXPLICIT NON-GOALS
======================================================================

This addition does NOT add:

    CSS indexing
    XML indexing
    JSX
    TSX
    Vue
    Svelte
    Astro
    Jinja
    Django templates
    ERB
    PHP
    Luau
    PowerShell
    Fish
    Zsh-specific indexing
    Python environment introspection
    language servers
    type inference
    runtime module resolution
    embedded JavaScript parsing inside HTML
    embedded CSS parsing inside HTML


Do not implement these opportunistically.


======================================================================
21. REQUIRED FINAL TEST MATRIX
======================================================================

Before self-review is considered complete, verify:

+-----------------------------+--------+------+-----+------+
| Capability                  | Python | Bash | Lua | HTML |
+-----------------------------+--------+------+-----+------+
| Language detection          | YES    | YES  | YES | YES  |
| Ordinary extension          | YES    | YES  | YES | YES  |
| Shebang where applicable    | YES    | YES  | YES | N/A  |
| Tree-sitter parsing         | YES    | YES  | YES | YES  |
| Symbol extraction           | YES    | YES  | YES | YES  |
| Dependency/import extraction| YES    | YES  | YES | YES  |
| Best-effort references      | YES    | YES  | YES | YES  |
| Source skeleton             | YES    | YES  | YES | YES  |
| Malformed source tolerated  | YES    | YES  | YES | YES  |
| Cache reuse                 | YES    | YES  | YES | YES  |
| Retrieval integration       | YES    | YES  | YES | YES  |
+-----------------------------+--------+------+-----+------+


Also verify:

    existing language tests pass
    linked-worktree tests pass
    snapshot-isolation tests pass


======================================================================
22. ACCEPTANCE CRITERIA
======================================================================

This addition is complete only when ALL of the following are true.

1. Python is a first-class registered indexed language.

2. Bash is a first-class registered indexed language.

3. Lua is a first-class registered indexed language.

4. HTML is a first-class registered indexed language.

5. Detection behavior follows this document.

6. Each language produces useful symbols.

7. Each language produces useful dependency/import data.

8. Each language produces conservative best-effort references.

9. Each language produces bounded source skeletons.

10. Malformed files do not abort project-wide refresh.

11. Content-addressed reuse works for all four languages.

12. Linked-worktree cache reuse remains correct.

13. Snapshot isolation remains correct.

14. Existing supported languages have no regression.

15. Existing retrieval can retrieve indexed artifacts from all four languages.

16. No LLM is involved in indexing.

17. No daemon is introduced.

18. No unsupported language scope has been added.

19. Rust quality gates pass.

20. Full self-review against this document passes.

21. The dedicated Git commit exists.


======================================================================
23. FINAL SELF-REVIEW CHECKLIST
======================================================================

Before committing, answer each item by inspecting the actual implementation.

LANGUAGE SUPPORT
----------------

[ ] Python registered.
[ ] Bash registered.
[ ] Lua registered.
[ ] HTML registered.


DETECTION
---------

[ ] Python extensions work.
[ ] Python shebang works.
[ ] Bash extensions work.
[ ] Bash/sh shebang works.
[ ] Lua extension works.
[ ] Lua shebang works.
[ ] HTML extensions work.
[ ] Contradictory extension/shebang behavior is deterministic.


PYTHON
------

[ ] Functions indexed.
[ ] Async functions indexed.
[ ] Classes indexed.
[ ] Methods indexed.
[ ] Imports indexed.
[ ] Relative imports represented.
[ ] Useful calls become references.
[ ] Skeleton is bounded.


BASH
----

[ ] Function definitions indexed.
[ ] Static source/dot dependencies indexed.
[ ] Dynamic source not falsely resolved.
[ ] Useful command/function calls represented.
[ ] Keywords are not noisy references.
[ ] Skeleton is bounded.


LUA
---

[ ] Global functions indexed.
[ ] Local functions indexed.
[ ] Dotted functions indexed.
[ ] Colon methods indexed.
[ ] Literal require indexed.
[ ] Dynamic require not falsely resolved.
[ ] Calls represented conservatively.
[ ] Skeleton is bounded.


HTML
----

[ ] IDs indexed as symbols.
[ ] Custom elements indexed appropriately.
[ ] Ordinary tags do not flood symbols.
[ ] CSS classes do not flood symbols.
[ ] Script src indexed.
[ ] Link href indexed.
[ ] Fragment references indexed.
[ ] Inline JS is not recursively indexed.
[ ] Inline CSS is not recursively indexed.
[ ] Skeleton is bounded.


ARCHITECTURE
------------

[ ] Existing LanguageIndexer abstraction used.
[ ] Existing registry used.
[ ] Existing ParsedArtifact used.
[ ] Existing SQLite/index storage path used.
[ ] Existing retrieval path used.
[ ] Existing content-key behavior preserved.
[ ] No parallel indexing subsystem created.
[ ] No language server added.
[ ] No LLM indexing added.


CORRECTNESS
-----------

[ ] Syntax errors are file-local.
[ ] Existing language tests pass.
[ ] Cache reuse tests pass.
[ ] Worktree isolation passes.
[ ] Snapshot isolation passes.
[ ] `git diff --check` passes.
[ ] `cargo fmt --check` passes.
[ ] clippy passes with warnings denied.
[ ] full tests pass.


If any item fails:

    fix it
    run tests again
    repeat this checklist

Only then commit.


======================================================================
24. FINAL COMMIT
======================================================================

Commit:

    nisse: add python bash lua and html indexing

After commit:

    git status --short

Verify no addition-related change remains uncommitted.

Report:

- commit hash;
- language grammar dependencies used;
- detection support added;
- extracted symbol/import/reference behavior;
- test commands;
- self-review result;
- acceptance result.

Then STOP.


END OF NISSE 1.0 SOURCE INDEX LANGUAGE ADDITIONS
```
