;; section: symbols
;;
;; Top-level declarations of a stylesheet: variables (`$name: value`), mixins, functions and
;; placeholder selectors (`%name { ... }`). The signature is the header up to the opening brace
;; (`@mixin button($color: red)`).

;; `$name: value;`
(stylesheet
  (declaration (property_name) @symbol.name) @symbol.variable
  (#match? @symbol.name "^\\$"))

;; `@mixin name($args) { ... }`
(stylesheet
  (mixin_statement name: (identifier) @symbol.name) @symbol.macro)

;; `@function name($args) { ... }`
(stylesheet
  (function_statement name: (identifier) @symbol.name) @symbol.function)

;; `%name { ... }`, one symbol per placeholder of the selector list.
(stylesheet
  (rule_set
    (selectors (placeholder) @symbol.name)) @symbol.class)
