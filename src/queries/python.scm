;; section: symbols

(function_definition name: (identifier) @symbol.name) @symbol.function

(class_definition name: (identifier) @symbol.name) @symbol.class

(assignment
  left: (identifier) @symbol.name
  right: (_)) @symbol.const

;; section: imports

(import_statement) @import.range
(import_from_statement) @import.range

;; section: calls

(call function: (identifier) @call.callee) @call.range
(call function: (attribute attribute: (identifier) @call.callee)) @call.range

;; section: docs

(comment) @doc.text
