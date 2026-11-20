;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(method_declaration name: (field_identifier) @symbol.name) @symbol.method

(type_declaration (type_spec name: (type_identifier) @symbol.name)) @symbol.type

;; `type A = B` is a `type_alias`, not a `type_spec`.
(type_declaration (type_alias name: (type_identifier) @symbol.name)) @symbol.type

;; The method set of a named interface.
(type_spec
  type: (interface_type
    (method_elem name: (field_identifier) @symbol.name) @symbol.method))

(const_declaration (const_spec name: (identifier) @symbol.name)) @symbol.const

(var_declaration (var_spec name: (identifier) @symbol.name)) @symbol.const

;; section: implementations
;;
;; Go uses structural typing — interface satisfaction is implicit and not encoded in syntax.
;; Struct embedding (`type Foo struct { Bar }`) is composition, not inheritance, and is
;; excluded from the Implementation model to avoid false positives. This section is
;; intentionally empty; the query returns no results for Go source files.

;; section: imports

(import_declaration) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (selector_expression field: (field_identifier) @call.callee)) @call.range

;; section: routes
;;
;; HTTP routes and the mounts that prefix them (ADR-0048, `src/extract/http.rs`).
;; gin: `r.GET("/users/:id", auth, handler)`; the last argument is the handler.

(call_expression
  function: (selector_expression
    operand: (_) @route.receiver
    field: (field_identifier) @route.method)
  arguments: (argument_list . (interpreted_string_literal) @route.path (_) @route.handler .)
  (#any-of? @route.method "GET" "POST" "PUT" "DELETE" "PATCH" "HEAD" "OPTIONS" "Any")
  (#match? @route.path "^\"/")) @route.range

;; `v1 := r.Group("/v1")` mounts `v1` under `/v1` of `r`.
(short_var_declaration
  left: (expression_list . (identifier) @mount.name)
  right: (expression_list
    . (call_expression
      function: (selector_expression
        operand: (identifier) @mount.parent
        field: (field_identifier) @_group)
      arguments: (argument_list . (interpreted_string_literal) @mount.path)))
  (#eq? @_group "Group"))

;; section: http_calls
;;
;; Literal-URL requests through net/http: `http.Get(url)`, `http.Post(url, ..)`,
;; `http.NewRequest("GET", url, body)`. Through any other receiver the URL must be absolute.

(call_expression
  function: (selector_expression
    operand: (_) @http.receiver
    field: (field_identifier) @http.method)
  arguments: (argument_list) @http.args
  (#any-of? @http.method "Get" "Head" "Post" "PostForm" "NewRequest" "NewRequestWithContext")) @http.range

;; section: docs

(comment) @doc.text
