# Typhoon Implementation Plan

This document outlines the step-by-step implementation of the Typhoon language as defined in Section 17 of the language specification.

## Phase 1: Lexer and Parser
**Goal:** Parse all Typhoon syntax and produce a concrete AST with source spans.

- [x] Implement Lexer
    - [x] Basic tokens (keywords, operators, literals)
    - [x] String interpolation (tokenize as sequence of string parts and expression spans)
    - [x] Doc comments
- [x] Implement Parser
    - [x] Operator precedence table (Pipe `|>` lowest, `?` highest after field access)
    - [x] Disambiguation of `{ ...x, f: v }` (merge expression) vs `{ stmt; expr }` (block)
    - [x] AST with source spans for error reporting
    - [x] Postfix calls on any expression (`expr(args...)`) enabling `x.method(...)`
    - [x] Parse `interface` / `impl Trait for Type { ... }` / `extend Type { ... }`
- [x] **Milestone:** All example programs in `spec.md` parse without error.

## Phase 2: Name Resolution and Type Inference
**Goal:** Resolve every identifier to its declaration and infer all types via bidirectional Hindley-Milner.

- [ ] Name Resolution
    - [x] Build scope tree
    - [x] Resolve all identifiers to canonical `DeclId`
    - [x] Introduce `DeclId` and `ScopeId` interners (arena indices)
    - [x] Symbol tables per scope (map `String` → `DeclId`)
    - [x] Handle shadowing and duplicate declaration errors
    - [x] Resolve `use` paths and populate namespace imports
    - [x] Resolve `struct`/`enum`/`newtype` type names in type annotations
    - [ ] Resolve member names for field access and enum variants (compiler currently types some field access without resolver-member linking)
    - [ ] Emit source-span-aware errors (unknown name, ambiguous path, private access)
    - [x] Tests: scope shadowing, unresolved names, `use` glob, path segments
- [ ] Type Inference
    - [x] Implement bidirectional HM inference
    - [x] Handle generics and interface bounds
    - [x] `Result`/`Option` desugaring
    - [ ] Type representation: `TyVar`, `TyCon`, `TyApp`, `TyFn`, `TyTuple`, `TyArray`
    - [x] Unification with occurs check and union-find
    - [x] Generalization at `let` bindings
    - [x] Instantiation at identifier use sites
    - [x] Expected-type propagation (bidirectional) for literals and blocks
    - [x] Numeric literal defaulting rules (Int32, Float32) with explicit `as`
    - [ ] Constraint solving for interface bounds (trait-like)
    - [ ] Structural typing rules for `struct` initialization and `enum` variants
    - [x] Type checking for `if`, `match`, `return`, and block trailing expression
    - [x] Struct field access typing (`x.field`) from declared struct field types
    - [x] Array indexing typing `a[i] -> Option<T>` for both fixed arrays and `Array<T>`
    - [x] Method-call typing `x.method(args...)` via mangled function symbols (`__ty_method__Type__method`)
    - [x] Built-in `Array<T>.push(val) -> Unit` typing (in-place)
    - [x] Tests: inferred let types, function calls, polymorphic `Option`/`Result`
- [ ] Desugaring
    - [x] Desugar `?` operator (verify compatible return type)
    - [x] Eliminate `|>` by rewriting to direct calls before IR lowering
    - [ ] Desugar `match` arms into core pattern forms (for later liveness)
    - [x] Desugar string interpolation into `Buf` builder calls
    - [x] Track desugared spans for error mapping
    - [x] Declaration renaming/desugaring supports `interface` / `impl` / `extend` blocks (methods desugar like functions)
- [ ] **Milestone:** All example programs type-check. Invalid programs produce clear type errors.

### Phase 2 Status
- Resolver: Scope tree + `DeclId`/`ScopeId` interners and per-scope symbol tables are implemented in `src/resolver.rs`, covering functions/params/`let`/`use` plus generic type params and annotation type names.
- Type Checker: `src/type_inference.rs` implements HM-style inference with unification (occurs check), generalization/instantiation, `if`/`match`/`if let` checking, and `Result`/`Option` constructor support used by desugaring.
- Desugar: `src/desugar.rs` rewrites `|>`, `?`, and string interpolation into core calls while preserving spans for diagnostics.
- Tests: `cargo test` exercises parser/resolver/type-inference/desugar behavior for generics, constructors, and control-flow typing.

## Phase 3: Liveness Checker
**Goal:** Enforce linear type rules and annotate every binding with its consumption point.

- [ ] Implement Linear Type Rules
    - [x] Maintain live set per scope (stack of `LiveSetId`s)
    - [x] Track consumption at assignment (`let b = a`)
    - [x] Track consumption at function calls
    - [x] Track consumption in merge expressions
    - [x] Track consumption in `conc` block captures and channel sends
    - [x] Track consumption during pattern matching (match arms, destructuring)
    - [ ] Track consumption when calling generic functions (monomorphized types)
    - [x] Model `ref` types as shared, escaping the linear live set
    - [x] Record spans of consumption and creation to improve diagnostics
    - [ ] Integrate with resolver/type-inference results (`DeclId` → `InferType`)
- [ ] Implement Automatic Drop Insertion
    - [ ] Insert drops for remaining live bindings at end of scope
    - [ ] Drop-insertion must respect `@repr(C)` / FFI boundaries
    - [ ] Emit drops for `match`/`if` tails that exit early
    - [ ] Support `Drop` trait hooking for standard library types
- [ ] Handle Special Bindings
    - [x] Exempt `let mut` from liveness tracking (free at scope exit)
    - [ ] Support `static`/`const` globals as always-live
    - [ ] Track renames/aliases created by `let alias = original`
- [ ] Conditional Liveness
    - [x] Ensure all branches of `if`/`match` consume the same live bindings
    - [x] Validate loops (`while`, `for`) maintain live-set invariants across iterations
    - [ ] Ensure early `return`/`break`/`continue` consume pending bindings
    - [ ] Emit actionable diagnostics describing which binding was prematurely consumed or forgotten
    - [x] Generate test suite covering linear violations: conditional moves, `conc` capture misuse, channels
    - [ ] Provide regression harness that runs against `spec.md` examples for `conc`/`merge`
- [ ] **Milestone:** Ownership violations caught with clear error messages. Test suite covers conditional moves, loop moves, and captures.

### Phase 3 Status
- Live sets: `LiveSet`/`LiveBinding` arenas track `let` bindings, parameters, and temporary expressions (`src/liveness.rs`).
- Analyzer: `LiveAnalyzer` walks functions, records consumption on identifier uses, and emits drop notes for unconsumed bindings; `drops` also notes origin context.
- Branches/loops: conditional branches (`if`, `match`, `if let`) now enforce consistent consumption across branches; loops are validated to preserve the entry live set.
- `conc` support: parser, resolver, and type checker accept `conc { ... }` blocks; the liveness analyzer treats `conc` captures as consuming bindings from the parent scope.
- Tests: regression cases show dropped unused parameters, detect double consumption, flag inconsistent conditional consumption, reject loop-consumption patterns, and validate `conc` capture consumption.

### Phase 3 Plan
- Step 1: Define `LiveSet`, `LiveBinding`, and `LiveSetId` arenas plus `LiveSetStack` to mirror the current scope tree (`DeclId` → `InferType` connections will be reused from Phase 2).
- Step 2: Instrument the AST walker (reusing the resolver) so each `let`, `match`, `return`, `conc`, and channel send records consumption/creation spans and updates the live set state.
- Step 3: Emit drop instructions for any live binding that survives to the end of a scope, paying attention to `@repr(C)`/FFI boundaries and the `Drop` trait hook for stdlib types.
- Step 4: Build diagnostics/tests covering conditional moves, loops, `conc` captures, and channel ownership failures, using `spec.md` examples as regression harnesses.

# Phase 1-3 Limitations
A few catches / limitations right now:

  - Resolver doesn’t yet resolve member names for field access or enum variants, and diagnostics are still string-based rather than structured error types.
  - Type inference doesn’t yet implement interface-bound constraint solving or structural typing rules for `struct` init / general enums (beyond `Option`/`Result` patterns).
  - Liveness doesn’t yet integrate with resolver/type-inference IDs, and its drop handling is diagnostics-oriented (not an inserted/typed drop IR).

## Phase 4: LLVM Code Generation
**Goal:** Produce correct native binaries for all example programs.

- [ ] Lower AST to LLVM IR
    - [ ] Struct lowering (sorted by alignment unless `@repr(C)`)
    - [x] `alloca` for size-stable `let` bindings
    - [x] Heap lowering via `ty_alloc`/`ty_free`/`ty_realloc` runtime API (malloc-backed for now; swapped to slab in Phase 5)
    - [x] Mixed arrays: fixed `[N x T]` for literals, widen to `%struct.TyArray*` (`Array<T>`) for `let mut` / annotated `Array<T>`
    - [x] `match` lowering (switch for integers, decision trees for structures)
    - [x] Function prolog/epilog generation (stack frame layout)
    - [ ] Generic monomorphization
    - [x] Call lowering with ABI usage and argument passing/promotion
    - [ ] Pointer provenance metadata for `ref` vs linear pointers
    - [ ] Inline `@derive` generated helpers (Eq, Hash, Display)
- [ ] Optimizations and Annotations
    - [ ] Add `noalias` to non-`ref` pointers
    - [ ] Add `nonnull` to non-optional pointers
    - [ ] Overflow behavior (`nsw`/`nuw` in debug, wrapping in release)
    - [ ] Tail-call optimization hints for recursive functions
    - [ ] Loop unrolling hints for `for` over `[T]`
    - [ ] Emit debug metadata for source spans
- [ ] **Milestone:** Programs compile to native binaries and produce correct output.

### Phase 4 Status
- Control flow: `if`/`else`, `if let`, and `while` now emit labeled basic blocks with conditional `br`, and `match` lowers to real branch trees with payload binding.
- Values/ABI: size-stable locals use `alloca`, struct/ADT values lower through SSA aggregates, constructors emit `insertvalue`, and call lowering now honors the declared symbol names and return conventions.
- Codegen binary: `src/main.rs` now drives lexing → parsing → resolution → typing → liveness → codegen, writes `.ll`, invokes `clang` with separate IR/C modes, and links `main` correctly on Windows.
- Methods/arrays: method calls `x.method(...)` lower to `@__ty_method__Type__method(...)`, `Array<T>.push` lowers to runtime `ty_array_push`, and indexing `a[i]` lowers to `ty_array_get_ptr` plus LLVM `Option<T>` construction.
- Tests: `cargo test` passes, and a smoke compile of a minimal Typhoon program reaches a native executable.

## Phase 5: Slab Allocator and Scheduler
**Goal:** Replace `malloc`/`free` placeholders with production runtime.

- [ ] Implement Per-Task Slab Allocator
    - [ ] Bump allocator
    - [ ] Size-class free list
    - [ ] Virtual memory reservation (`mmap`/`VirtualAlloc`)
    - [ ] Integration with LLVM IR (heap type lowering uses allocator interfaces)
    - [x] (v0) Runtime heap API + array helpers are `malloc`-backed (`ty_alloc`/`ty_realloc`/`ty_free`, `ty_array_from_fixed`, `ty_array_push`, `ty_array_get_ptr`)
- [ ] Implement M:N Scheduler
    - [ ] Work-stealing deques (one OS thread per core)
    - [ ] Stackful coroutines (64 KB initial, grows on fault)
    - [ ] Cooperative yielding at I/O and channel blocks
    - [ ] Preemptive yielding via `SIGPROF`
    - [ ] Task-local slabs recycled per coroutine to avoid cross-thread locking
    - [ ] Scheduler API exposed to language runtime (`spawn`, `await`, `conc`)
- [ ] Implement Channels
    - [ ] `chan<T>` as bounded ring buffer with coroutine waitlists
    - [ ] Support `select`/`recv` semantics with fairness hints
    - [ ] Linear ownership of channel tokens (send consumes, recv produces)
- [ ] **Milestone:** `conc` and `chan` examples run correctly under concurrent load. Benchmarked favorably against `jemalloc`.

## Phase 6: IO and Networking
**Goal:** Working HTTP server with zero-copy parsing and capability model.

- [ ] Implement IO Driver (Rust FFI Bridge)
    - [ ] `io_uring` (Linux)
    - [ ] `kqueue` (macOS)
    - [ ] `IOCP` (Windows)
    - [ ] Async-safe handles consumable per task (`Network` capability token)
- [ ] Integration with Scheduler
    - [ ] I/O operations transparently yield coroutines
    - [ ] Polling driver uses scheduler waitlists for read/write readiness
- [ ] Capability Model
    - [ ] Generate `Network` token at `main` entry point
    - [ ] Enforce token linearity during `net.listen` / `net.accept`
- [ ] Networking Implementation
    - [ ] `LinearSocket` with single-owner semantics
    - [ ] Zero-copy HTTP/1.1 resumable parser (`StrView` pointers into slab)
    - [ ] Header linear scan
    - [ ] TLS handshake offloaded to Rust or OS primitives (optional phase)
    - [ ] Back-pressure via channel-based request queue
- [ ] **Milestone:** HTTP server handles 10,000 concurrent connections. Benchmarked against Go and Rust.

## Phase 7: Standard Library
**Goal:** Provide essential built-in types and utilities.

- [ ] Tier 1: Core (Global)
    - [ ] `Str`, `Buf`, `[T]`, `Map<K,V>`, `Set<T>`, `Option<T>`, `Result<T,E>`
    - [ ] `std::io` (read, write, print, scan)
    - [ ] `assert!`/`assert_eq!` macros in `test`
    - [ ] Expose `StrView` for zero-copy parsing helpers
- [ ] Tier 2: Standard (Explicit Import)
    - [ ] `ref T`
    - [ ] `std::math`, `std::time`, `std::fmt`, `std::fs`, `std::process`
    - [ ] Add `std::net` wrappers around `LinearSocket` for convenience
- [ ] Tier 3: Ecosystem (Packages)
    - [ ] `json`, `http` (client), `test`, `@derive`
    - [ ] Guidelines for publishing packages via `typhoon.toml`
- [ ] **Milestone:** Complete and usable standard library for practical application development.
