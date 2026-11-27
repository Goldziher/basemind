;; section: locals
;;
;; Intra-file scope resolution for Python. Standard tree-sitter `locals` capture convention:
;; `@local.scope` marks a lexical scope, `@local.definition[.*]` a binding, `@local.reference`
;; a use. Adapted from tree-sitter-python's upstream `queries/locals.scm`; kept minimal but
;; correct (function/lambda/module scopes, parameter + assignment defs, identifier refs).
;; Consumed by `crate::extract::locals` — the resolver binds each reference to the nearest
;; enclosing same-named definition.

;; scopes
(module) @local.scope
(function_definition) @local.scope
(lambda) @local.scope

;; definitions
(parameters (identifier) @local.definition.parameter)
(default_parameter name: (identifier) @local.definition.parameter)
(typed_parameter (identifier) @local.definition.parameter)
(typed_default_parameter name: (identifier) @local.definition.parameter)
(function_definition name: (identifier) @local.definition.function)
(assignment left: (identifier) @local.definition.var)
(for_statement left: (identifier) @local.definition.var)

;; references
(identifier) @local.reference
