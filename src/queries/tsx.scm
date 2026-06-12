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

;; Methods (incl. accessors) — getter/setter promotion happens in extract/l1.rs.
(method_definition name: (property_identifier) @symbol.name) @symbol.method

;; TS namespaces inside .tsx files (rare but legal).
(internal_module name: (identifier) @symbol.name) @symbol.namespace
(module name: (identifier) @symbol.name) @symbol.namespace

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

;; section: implementations
;;
;; Mirrors typescript.scm — TSX shares the same grammar node names for class heritage.

;; class extends
(class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (extends_clause
      value: (identifier) @impl.trait_name))) @impl.range

;; class implements
(class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (implements_clause
      (type_identifier) @impl.trait_name))) @impl.range

;; interface extends
(interface_declaration
  name: (type_identifier) @impl.implementor
  (extends_type_clause
    (type_identifier) @impl.trait_name)) @impl.range

;; section: imports

(import_statement) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (member_expression property: (property_identifier) @call.callee)) @call.range

;; section: docs

((comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
