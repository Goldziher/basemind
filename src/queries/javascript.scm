;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(class_declaration name: (identifier) @symbol.name) @symbol.class

(method_definition name: (property_identifier) @symbol.name) @symbol.method

(lexical_declaration
  (variable_declarator name: (identifier) @symbol.name)) @symbol.const

;; section: imports

(import_statement) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (member_expression property: (property_identifier) @call.callee)) @call.range

;; section: docs

((comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
