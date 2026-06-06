;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(method_declaration name: (field_identifier) @symbol.name) @symbol.method

(type_declaration (type_spec name: (type_identifier) @symbol.name)) @symbol.type

(const_declaration (const_spec name: (identifier) @symbol.name)) @symbol.const

(var_declaration (var_spec name: (identifier) @symbol.name)) @symbol.const

;; section: imports

(import_declaration) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (selector_expression field: (field_identifier) @call.callee)) @call.range

;; section: docs

(comment) @doc.text
