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
