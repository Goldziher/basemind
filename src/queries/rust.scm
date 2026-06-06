;; section: symbols

(function_item name: (identifier) @symbol.name) @symbol.function

(struct_item name: (type_identifier) @symbol.name) @symbol.struct

(enum_item name: (type_identifier) @symbol.name) @symbol.enum

(union_item name: (type_identifier) @symbol.name) @symbol.struct

(trait_item name: (type_identifier) @symbol.name) @symbol.trait

(type_item name: (type_identifier) @symbol.name) @symbol.type

(const_item name: (identifier) @symbol.name) @symbol.const

(static_item name: (identifier) @symbol.name) @symbol.const

(mod_item name: (identifier) @symbol.name) @symbol.module

(macro_definition name: (identifier) @symbol.name) @symbol.macro

;; section: imports

(use_declaration) @import.range

(extern_crate_declaration name: (identifier) @import.module) @import.range

;; section: calls

(call_expression function: (identifier) @call.callee) @call.range
(call_expression
  function: (field_expression field: (field_identifier) @call.callee)) @call.range
(call_expression
  function: (scoped_identifier name: (identifier) @call.callee)) @call.range
(macro_invocation macro: (identifier) @call.callee) @call.range

;; section: docs

((line_comment) @doc.text
 (#match? @doc.text "^///"))
((block_comment) @doc.text
 (#match? @doc.text "^/\\*\\*"))
