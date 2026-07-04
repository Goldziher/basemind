;; section: locals
;;
;; Intra-file scope resolution for Go. Standard tree-sitter `locals` capture convention:
;; `@local.scope` marks a lexical scope, `@local.definition[.*]` a binding, `@local.reference`
;; a use. Adapted from tree-sitter-go's upstream `queries/locals.scm`; kept minimal but correct
;; (function/method/block scopes, parameter + short-var + var/const defs, identifier refs).
;; Consumed by `crate::extract::locals` — the resolver binds each reference to the nearest
;; enclosing same-named definition.

;; scopes
(function_declaration) @local.scope
(method_declaration) @local.scope
(func_literal) @local.scope
(block) @local.scope
(if_statement) @local.scope
(for_statement) @local.scope
(expression_switch_statement) @local.scope
(type_switch_statement) @local.scope

;; definitions
(function_declaration name: (identifier) @local.definition.function)
(parameter_declaration name: (identifier) @local.definition.parameter)
(variadic_parameter_declaration name: (identifier) @local.definition.parameter)
(short_var_declaration left: (expression_list (identifier) @local.definition.var))
(var_spec name: (identifier) @local.definition.var)
(const_spec name: (identifier) @local.definition.var)

;; references
(identifier) @local.reference
