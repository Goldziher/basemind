;; section: locals
;;
;; Intra-file scope resolution for TypeScript. Standard tree-sitter `locals` capture convention:
;; `@local.scope` marks a lexical scope, `@local.definition[.*]` a binding, `@local.reference` a
;; use. Adapted from tree-sitter-javascript's upstream `queries/locals.scm` (TS is a grammar
;; superset) with the TS-specific `required_parameter` / `optional_parameter` param nodes added.
;; Consumed by `crate::extract::locals` — the resolver binds each reference to the nearest
;; enclosing same-named definition. Precise TS resolution goes through the oxc engine; this path
;; only answers "is this name local to the file?".

;; scopes
(statement_block) @local.scope
(function_expression) @local.scope
(arrow_function) @local.scope
(function_declaration) @local.scope
(method_definition) @local.scope

;; definitions
(required_parameter pattern: (identifier) @local.definition.parameter)
(optional_parameter pattern: (identifier) @local.definition.parameter)
(variable_declarator name: (identifier) @local.definition.var)

;; references
(identifier) @local.reference
