;; TSX captures. Mirrors typescript.scm because tree-sitter-typescript ships TSX as a
;; grammar superset — the same node names (function_declaration, class_declaration, …)
;; exist plus JSX nodes. Lives in its own file so JSX-specific symbol patterns
;; (`(jsx_element …)`, custom-component recognition, etc.) can be layered in without
;; touching the plain-TS captures used by `.ts` files.

;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(class_declaration name: (type_identifier) @symbol.name) @symbol.class

(interface_declaration name: (type_identifier) @symbol.name) @symbol.interface

(type_alias_declaration name: (type_identifier) @symbol.name) @symbol.type

(enum_declaration name: (identifier) @symbol.name) @symbol.enum

(method_definition name: (property_identifier) @symbol.name) @symbol.method

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

;; section: imports

(import_statement) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (member_expression property: (property_identifier) @call.callee)) @call.range

;; section: docs

((comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
