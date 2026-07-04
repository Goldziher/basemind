;; section: locals
;;
;; Intra-file scope resolution for TSX. Mirrors typescript-locals.scm — tree-sitter-typescript
;; ships TSX as a grammar superset sharing the same node names (statement_block, arrow_function,
;; required_parameter, variable_declarator, …). Lives in its own file so JSX-specific scope
;; patterns can be layered in without touching the plain-TS captures. Standard tree-sitter
;; `locals` capture convention; consumed by `crate::extract::locals`.

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
