;; section: symbols

(function_item name: (identifier) @symbol.name) @symbol.function

;; Bodiless declarations: trait methods without a default body and `extern` block functions.
(function_signature_item name: (identifier) @symbol.name) @symbol.function

(struct_item name: (type_identifier) @symbol.name) @symbol.struct

(enum_item name: (type_identifier) @symbol.name) @symbol.enum

(union_item name: (type_identifier) @symbol.name) @symbol.struct

(trait_item name: (type_identifier) @symbol.name) @symbol.trait

(type_item name: (type_identifier) @symbol.name) @symbol.type

(const_item name: (identifier) @symbol.name) @symbol.const

(static_item name: (identifier) @symbol.name) @symbol.const

(mod_item name: (identifier) @symbol.name) @symbol.module

(macro_definition name: (identifier) @symbol.name) @symbol.macro

;; `impl Foo { … }` — the captured @symbol.name is the type identifier; for trait impls
;; (`impl Trait for Foo`) tree-sitter's `type:` field still points at `Foo`, so the name is
;; the implementing type. Agents looking for "where is Foo implemented" find it here.
(impl_item type: (type_identifier) @symbol.name) @symbol.impl
(impl_item type: (scoped_type_identifier name: (type_identifier) @symbol.name)) @symbol.impl
(impl_item type: (generic_type type: (type_identifier) @symbol.name)) @symbol.impl
(impl_item type: (generic_type type: (scoped_type_identifier name: (type_identifier) @symbol.name))) @symbol.impl

;; section: imports

(use_declaration) @import.range

(extern_crate_declaration name: (identifier) @import.module) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (field_expression field: (field_identifier) @call.callee)) @call.range
(call_expression
  function: (scoped_identifier name: (identifier) @call.callee)) @call.range
(call_expression
  function: (generic_function function: (identifier) @call.callee)) @call.range
(call_expression
  function: (generic_function
    function: (field_expression field: (field_identifier) @call.callee))) @call.range
(call_expression
  function: (generic_function
    function: (scoped_identifier name: (identifier) @call.callee))) @call.range
(macro_invocation macro: (identifier) @call.callee) @call.range

;; Macro arguments stay an unparsed `token_tree`, so a call there is recognised lexically: an
;; identifier directly followed by a parenthesised token tree. `l2::build_call` keeps only those
;; inside a `macro_invocation` and drops definitions (`fn name(`, `struct Name(`).
(token_tree
  (identifier) @call.lexical_callee
  .
  (token_tree) @call.lexical_args)

;; section: implementations
;;
;; Captures `impl Trait for Type` (trait impls) and `impl Type { … }` (inherent impls).
;; For trait impls both @impl.trait_name and @impl.implementor are captured from a single
;; match so `build_implementation` does not need to walk ancestors.
;; Inherent impls (`impl Foo { … }`) have no trait — we emit only the type as @impl.implementor
;; and omit the trait_name so `build_implementation` skips the match (trait_name is required).

;; The trait is keyed by its last path segment, so `impl crate::a::Observer for X` and
;; `impl<T> Extend<T> for Vec<T>` both index under the bare trait name. The self type is any type:
;; `l1::implementor_name` names a path or generic type by its last segment (`Vec`) and any other type
;; (`&mut T`, `(IpAddr, u16)`, `()`, `[u8]`, `*const T`, `str`) by its text as written.
(impl_item
  trait: [
    (type_identifier) @impl.trait_name
    (scoped_type_identifier name: (type_identifier) @impl.trait_name)
    (generic_type type: [
      (type_identifier) @impl.trait_name
      (scoped_type_identifier name: (type_identifier) @impl.trait_name)
    ])
  ]
  type: (_) @impl.implementor) @impl.range

;; section: routes
;;
;; HTTP routes (ADR-0048, `src/extract/http.rs`). axum: `.route("/users/:id", get(show).post(update))`;
;; the method router names a method and a handler per link. The route starts at `route`, not at the
;; start of the `Router::new()` chain it sits in.

(call_expression
  function: (field_expression field: (field_identifier) @route.anchor)
  arguments: (arguments
    . [(string_literal) (raw_string_literal)] @route.path
    . (_) @route.method_router)
  (#eq? @route.anchor "route")) @route.range

;; section: http_calls
;;
;; Literal-URL requests: `reqwest::get(url)`, `client.get(url)`, `client.request(Method::PUT, url)`.
;; Through a receiver other than `reqwest` the URL must be absolute http(s).

(call_expression
  function: (scoped_identifier
    path: (identifier) @http.receiver
    name: (identifier) @http.method)
  arguments: (arguments) @http.args
  (#eq? @http.receiver "reqwest")
  (#eq? @http.method "get")) @http.range

(call_expression
  function: (field_expression
    value: (_) @http.receiver
    field: (field_identifier) @http.method)
  arguments: (arguments) @http.args
  (#any-of? @http.method "get" "post" "put" "delete" "patch" "head" "request")) @http.range

;; section: docs

((line_comment) @doc.text
 (#match? @doc.text "^///"))
((block_comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
