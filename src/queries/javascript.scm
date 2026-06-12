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

;; section: implementations
;;
;; `class Foo extends Bar` — JavaScript only supports single inheritance via extends.
;; No implements keyword in JS. One pattern captures the parent identifier.

(class_declaration
  name: (identifier) @impl.implementor
  (class_heritage
    (identifier) @impl.trait_name)) @impl.range

;; section: imports

(import_statement) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (member_expression property: (property_identifier) @call.callee)) @call.range

;; section: docs

((comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
