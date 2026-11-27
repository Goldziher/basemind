;; TSX captures. Mirrors typescript.scm because tree-sitter-typescript ships TSX as a
;; grammar superset — the same node names (function_declaration, class_declaration, …)
;; exist plus JSX nodes. Lives in its own file so JSX-specific symbol patterns
;; (`(jsx_element …)`, custom-component recognition, etc.) can be layered in without
;; touching the plain-TS captures used by `.ts` files.

;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(class_declaration name: (type_identifier) @symbol.name) @symbol.class

(abstract_class_declaration name: (type_identifier) @symbol.name) @symbol.class

(interface_declaration name: (type_identifier) @symbol.name) @symbol.interface

(type_alias_declaration name: (type_identifier) @symbol.name) @symbol.type

(enum_declaration name: (identifier) @symbol.name) @symbol.enum

;; Methods (incl. accessors) — getter/setter promotion happens in extract/l1.rs.
(method_definition name: (property_identifier) @symbol.name) @symbol.method

;; TS namespaces inside .tsx files (rare but legal).
(internal_module name: (identifier) @symbol.name) @symbol.namespace
(module name: (identifier) @symbol.name) @symbol.namespace

;; `declare function f(): void;` has no body, so it is a `function_signature`, not a declaration.
(ambient_declaration
  (function_signature name: (identifier) @symbol.name) @symbol.function)

;; Class fields are `field`; one holding an arrow function or function expression (a React class
;; component's `handleClick = () => …`) is promoted to `method` by the dedupe pass in `extract/l1.rs`.
(public_field_definition name: (property_identifier) @symbol.name) @symbol.field

(public_field_definition
  name: (property_identifier) @symbol.name
  value: [(arrow_function) (function_expression)]) @symbol.method

(abstract_method_signature name: (property_identifier) @symbol.name) @symbol.method

(interface_body
  (method_signature name: (property_identifier) @symbol.name) @symbol.method)

(interface_body
  (property_signature name: (property_identifier) @symbol.name) @symbol.field)

(lexical_declaration
  (variable_declarator name: (identifier) @symbol.name)) @symbol.const

(lexical_declaration
  (variable_declarator
    name: (identifier) @symbol.name
    value: (arrow_function))) @symbol.function

(lexical_declaration
  (variable_declarator
    name: (identifier) @symbol.name
    value: (function_expression))) @symbol.function

;; `var` declarations: kind `variable`, promoted to `function` like a `const` when it holds a function.
(variable_declaration
  (variable_declarator name: (identifier) @symbol.name)) @symbol.variable

(variable_declaration
  (variable_declarator
    name: (identifier) @symbol.name
    value: [(arrow_function) (function_expression)])) @symbol.function

;; section: implementations
;;
;; Mirrors typescript.scm — TSX shares the same grammar node names for class heritage.

;; class extends: `Base`, `Base<T>`, or a member expression (`React.Component`) named by its last
;; segment, as a dotted Python base is.
(class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (extends_clause
      value: [
        (identifier) @impl.trait_name
        (member_expression property: (property_identifier) @impl.trait_name)
      ]))) @impl.range

(abstract_class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (extends_clause
      value: [
        (identifier) @impl.trait_name
        (member_expression property: (property_identifier) @impl.trait_name)
      ]))) @impl.range

;; class implements (one match per implemented type); a generic type (`Repo<T>`) is named by its
;; type name.
(class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (implements_clause
      [
        (type_identifier) @impl.trait_name
        (generic_type name: (type_identifier) @impl.trait_name)
      ]))) @impl.range

(abstract_class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (implements_clause
      [
        (type_identifier) @impl.trait_name
        (generic_type name: (type_identifier) @impl.trait_name)
      ]))) @impl.range

;; interface extends
(interface_declaration
  name: (type_identifier) @impl.implementor
  (extends_type_clause
    [
      (type_identifier) @impl.trait_name
      (generic_type name: (type_identifier) @impl.trait_name)
    ])) @impl.range

;; section: imports

(import_statement) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (member_expression property: (property_identifier) @call.callee)) @call.range

;; section: routes
;;
;; HTTP routes and the mounts that prefix them (ADR-0048, `src/extract/http.rs`).
;; Express: `app.get("/users/:id", auth, handler)`: a verb on a router, a literal path starting with
;; `/` and at least one more argument; the last one is the handler. `axios.post("/users", body)` is a
;; request, and a last argument that cannot be a function (`{ params }`) rules a route out.

(call_expression
  function: (member_expression
    object: (identifier) @route.receiver
    property: (property_identifier) @route.method)
  arguments: (arguments . [(string) (template_string)] @route.path (_) @route.handler .)
  (#any-of? @route.method "get" "post" "put" "delete" "patch" "head" "options" "all")
  (#not-eq? @route.receiver "axios")
  (#match? @route.path "^['\"`]/")) @route.range

;; `app.use("/api", router)` mounts `router` under `/api`.
(call_expression
  function: (member_expression
    object: (identifier) @mount.parent
    property: (property_identifier) @_use)
  arguments: (arguments . (string) @mount.path (identifier) @mount.name .)
  (#eq? @_use "use"))

;; section: http_calls
;;
;; Literal-URL requests: `fetch(url, init)`, `axios(config)`, `axios.get(url)`. A `.get(..)` on any
;; other receiver counts only with an absolute http(s) URL (`src/extract/http.rs`).

(call_expression
  function: (identifier) @http.method
  arguments: (arguments) @http.args
  (#any-of? @http.method "fetch" "axios")) @http.range

(call_expression
  function: (member_expression
    object: (_) @http.receiver
    property: (property_identifier) @http.method)
  arguments: (arguments) @http.args
  (#any-of? @http.method "get" "post" "put" "delete" "patch" "head" "options")) @http.range

;; section: docs

((comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
