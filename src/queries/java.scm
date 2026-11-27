;; Java. An override replaces the TSLP `tags.scm` fallback for every section. The calls and
;; implementations sections below are the adapted upstream `tags.scm` patterns, unchanged, and the
;; symbols section extends them with the declarations the fallback missed; the parity test in
;; `src/extract/java_parity_tests.rs` holds the override to everything the fallback extracted.

;; section: symbols

(class_declaration
  name: (identifier) @symbol.name) @symbol.class

(method_declaration
  name: (identifier) @symbol.name) @symbol.method

(interface_declaration
  name: (identifier) @symbol.name) @symbol.interface

(enum_declaration
  name: (identifier) @symbol.name) @symbol.enum

(record_declaration
  name: (identifier) @symbol.name) @symbol.class

;; `@interface Marker {…}` is an interface; its elements (`String value() default "";`) are methods.
(annotation_type_declaration
  name: (identifier) @symbol.name) @symbol.interface

(annotation_type_element_declaration
  name: (identifier) @symbol.name) @symbol.method

(constructor_declaration
  name: (identifier) @symbol.name) @symbol.constructor

;; A record's compact canonical constructor: `Point { … }`.
(compact_constructor_declaration
  name: (identifier) @symbol.name) @symbol.constructor

;; One symbol per declarator (`int a, b;` declares two). A `static final` field is promoted to
;; `const` in `extract/l1.rs`; an interface's fields are constants by definition.
(field_declaration
  declarator: (variable_declarator name: (identifier) @symbol.name)) @symbol.field

(constant_declaration
  declarator: (variable_declarator name: (identifier) @symbol.name)) @symbol.const

;; section: calls

(method_invocation
  name: (identifier) @call.callee
  arguments: (argument_list) @call.range)

;; section: imports
;;
;; Plain, static and wildcard imports. `@import.module` is the qualified name without the wildcard
;; (`java.util` for `import java.util.*;`) and keeps a static import's member (`a.B.m`).

(import_declaration [(scoped_identifier) (identifier)] @import.module) @import.range

;; section: implementations

(type_list
  (type_identifier) @impl.trait_name) @impl.range

;; section: routes
;;
;; HTTP routes (ADR-0048, `src/extract/http.rs`). Spring MVC: a `@GetMapping("/{id}")` (or
;; `@RequestMapping(path = .., method = ..)`) method is the handler; a class-level
;; `@RequestMapping("/owners")` prefixes every route in the class body.

(method_declaration
  (modifiers
    [
      (annotation name: (identifier) @route.method arguments: (annotation_argument_list) @route.args)
      (marker_annotation name: (identifier) @route.method)
    ])
  name: (identifier) @route.handler
  (#any-of? @route.method "GetMapping" "PostMapping" "PutMapping" "DeleteMapping" "PatchMapping" "RequestMapping"))
  @route.range

(class_declaration
  (modifiers
    (annotation name: (identifier) @_mapping arguments: (annotation_argument_list) @mount.args))
  body: (class_body) @mount.scope
  (#eq? @_mapping "RequestMapping"))
