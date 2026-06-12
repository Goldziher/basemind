;; section: symbols

(function_declaration name: (identifier) @symbol.name) @symbol.function

(class_declaration name: (type_identifier) @symbol.name) @symbol.class

(interface_declaration name: (type_identifier) @symbol.name) @symbol.interface

(type_alias_declaration name: (type_identifier) @symbol.name) @symbol.type

(enum_declaration name: (identifier) @symbol.name) @symbol.enum

;; All methods captured as `method`. Getter/setter accessors are detected from the source
;; bytes in extract/l1.rs::detect_accessor (querying the `kind` keyword via tree-sitter
;; query predicates proved unreliable across grammar versions).
(method_definition name: (property_identifier) @symbol.name) @symbol.method

;; TypeScript namespaces. `namespace Foo {…}` lowers to `internal_module`; ambient
;; `module "foo" {…}` (in .d.ts files) lowers to `module`.
(internal_module name: (identifier) @symbol.name) @symbol.namespace
(module name: (identifier) @symbol.name) @symbol.namespace

;; Generic `const X = …` capture — kind `const`. The more specific arrow/function-expression
;; pattern below also fires and is promoted by the dedupe pass in `extract/l1.rs` to kind
;; `function`, so `const Foo = () => …` and `const Bar = function() {…}` end up searchable
;; as functions rather than constants.
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
;; `class Foo extends Bar` — extends_clause.value is the parent expression.
;; `class Foo implements Bar, Baz` — implements_clause children are type nodes.
;; `interface Foo extends Bar` — extends_type_clause.type children.
;; One @impl.trait_name per parent yields one Implementation record per inheritance edge.

;; class extends
(class_declaration
  name: (type_identifier) @impl.implementor
  (class_heritage
    (extends_clause
      value: (identifier) @impl.trait_name))) @impl.range

;; class implements (one pattern per implemented type)
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
