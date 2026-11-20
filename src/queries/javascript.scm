;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(class_declaration name: (identifier) @symbol.name) @symbol.class

(method_definition name: (property_identifier) @symbol.name) @symbol.method

;; See typescript.scm — same dedupe trick promotes arrow / function-expression `const`s
;; from kind=const to kind=function via `extract/l1.rs`.
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

;; Class fields are `field`; one holding an arrow function or function expression (a React class
;; component's `handleClick = () => …`) is promoted to `method`.
(field_definition property: (property_identifier) @symbol.name) @symbol.field

(field_definition
  property: (property_identifier) @symbol.name
  value: [(arrow_function) (function_expression)]) @symbol.method

;; CommonJS exports. `exports.x = …` and `module.exports.x = …` define `x`; `module.exports = …`
;; defines `module.exports`. Kind `const`, promoted to `function` when the value is a function.
(expression_statement
  (assignment_expression
    left: (member_expression
      object: (identifier) @_exports
      property: (property_identifier) @symbol.name)
    (#eq? @_exports "exports")) @symbol.const)

(expression_statement
  (assignment_expression
    left: (member_expression
      object: (identifier) @_exports
      property: (property_identifier) @symbol.name)
    right: [(arrow_function) (function_expression)]
    (#eq? @_exports "exports")) @symbol.function)

(expression_statement
  (assignment_expression
    left: (member_expression
      object: (member_expression
        object: (identifier) @_module
        property: (property_identifier) @_exports)
      property: (property_identifier) @symbol.name)
    (#eq? @_module "module")
    (#eq? @_exports "exports")) @symbol.const)

(expression_statement
  (assignment_expression
    left: (member_expression
      object: (member_expression
        object: (identifier) @_module
        property: (property_identifier) @_exports)
      property: (property_identifier) @symbol.name)
    right: [(arrow_function) (function_expression)]
    (#eq? @_module "module")
    (#eq? @_exports "exports")) @symbol.function)

(expression_statement
  (assignment_expression
    left: (member_expression
      object: (identifier) @_module
      property: (property_identifier) @_exports) @symbol.name
    (#eq? @_module "module")
    (#eq? @_exports "exports")) @symbol.const)

(expression_statement
  (assignment_expression
    left: (member_expression
      object: (identifier) @_module
      property: (property_identifier) @_exports) @symbol.name
    right: [(arrow_function) (function_expression)]
    (#eq? @_module "module")
    (#eq? @_exports "exports")) @symbol.function)

;; section: implementations
;;
;; `class Foo extends Bar` — JavaScript only supports single inheritance via extends.
;; No implements keyword in JS. The parent is an identifier or a member expression
;; (`React.Component`), named by its last segment.

(class_declaration
  name: (identifier) @impl.implementor
  (class_heritage
    [
      (identifier) @impl.trait_name
      (member_expression property: (property_identifier) @impl.trait_name)
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
