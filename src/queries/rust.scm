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

;; `impl Foo { … }` — the captured @symbol.name is the type identifier; for trait impls
;; (`impl Trait for Foo`) tree-sitter's `type:` field still points at `Foo`, so the name is
;; the implementing type. Agents looking for "where is Foo implemented" find it here.
(impl_item type: (type_identifier) @symbol.name) @symbol.impl
(impl_item type: (scoped_type_identifier name: (type_identifier) @symbol.name)) @symbol.impl
(impl_item type: (generic_type type: (type_identifier) @symbol.name)) @symbol.impl

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
