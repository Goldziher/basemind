;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(method_declaration name: (field_identifier) @symbol.name) @symbol.method

(type_declaration (type_spec name: (type_identifier) @symbol.name)) @symbol.type

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

;; section: docs

(comment) @doc.text
