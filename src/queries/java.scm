;; Java. An override replaces the TSLP `tags.scm` fallback for every section, so the symbols, calls
;; and implementations sections below are the adapted upstream `tags.scm` patterns, unchanged; the
;; parity test in `src/extract/java_parity_tests.rs` holds them to what the fallback extracted.

;; section: symbols

(class_declaration
  name: (identifier) @symbol.name) @symbol.class

(method_declaration
  name: (identifier) @symbol.name) @symbol.method

(interface_declaration
  name: (identifier) @symbol.name) @symbol.interface

;; section: calls

(method_invocation
  name: (identifier) @call.callee
  arguments: (argument_list) @call.range)

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
