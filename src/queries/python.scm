;; section: symbols
;;
;; The plain `function_definition` / `class_definition` patterns match BOTH bare definitions
;; and the inner definition of a `decorated_definition`. To capture decorator metadata we add
;; a second pair of patterns that target `decorated_definition` and capture each decorator
;; AND the inner symbol node — using the inner node so the (start_byte, name) dedupe key
;; aligns with the plain pattern. The dedupe pass in extract/l1.rs merges decorators across
;; the two matches.

(function_definition name: (identifier) @symbol.name) @symbol.function

(class_definition name: (identifier) @symbol.name) @symbol.class

;; Decorated function — `@symbol.function` capture is on the inner function_definition so
;; start_byte matches the bare pattern above; decorators are surfaced as a separate capture.
(decorated_definition
  (decorator) @symbol.decorator
  definition: (function_definition name: (identifier) @symbol.name) @symbol.function)

;; Decorated class — same shape.
(decorated_definition
  (decorator) @symbol.decorator
  definition: (class_definition name: (identifier) @symbol.name) @symbol.class)

(assignment
  left: (identifier) @symbol.name
  right: (_)) @symbol.const

;; section: imports

(import_statement) @import.range
(import_from_statement) @import.range

;; section: implementations
;;
;; `class Foo(Bar, Baz):` — one match per base class. The @impl.implementor capture
;; is on the class_definition node's name field and @impl.trait_name on each argument.

(class_definition
  name: (identifier) @impl.implementor
  superclasses: (argument_list
    (identifier) @impl.trait_name)) @impl.range

;; Dotted base: `class Foo(some.Bar):`
(class_definition
  name: (identifier) @impl.implementor
  superclasses: (argument_list
    (attribute attribute: (identifier) @impl.trait_name))) @impl.range

;; section: calls

(call function: (identifier) @call.callee) @call.range
(call function: (attribute attribute: (identifier) @call.callee)) @call.range

;; section: docs

(comment) @doc.text
