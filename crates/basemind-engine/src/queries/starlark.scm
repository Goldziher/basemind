;; section: symbols
;;
;; Starlark (`BUILD`, `BUILD.bazel`, `WORKSPACE`, `MODULE.bazel`, `*.bzl`, `*.star`). Only top-level
;; statements are declarations; what a macro or function does inside its body is not.

;; `def macro(name, srcs = []):`
(module
  (function_definition
    name: (identifier) @symbol.name) @symbol.function)

;; `DEPS = [...]` and `_private = 1`
(module
  (expression_statement
    (assignment
      left: (identifier) @symbol.name) @symbol.variable))

;; A rule invocation is a build target named by its `name` argument: `py_library(name = "lib")` is
;; the symbol `lib`, its signature is the rule (`py_library`). A target assigned to a variable
;; (`x = my_rule(name = "t")`) is a target too.
(module
  (expression_statement
    (call
      function: (identifier)
      arguments: (argument_list
        (keyword_argument
          name: (identifier) @_arg
          value: (string (string_content) @symbol.name)))) @symbol.class
    (#eq? @_arg "name")))

(module
  (expression_statement
    (assignment
      right: (call
        function: (identifier)
        arguments: (argument_list
          (keyword_argument
            name: (identifier) @_arg
            value: (string (string_content) @symbol.name)))) @symbol.class)
    (#eq? @_arg "name")))

;; section: imports
;;
;; `load("//tools:defs.bzl", "my_rule", helper = "helper_fn")`; the module is the label.

(module
  (expression_statement
    (call
      function: (identifier) @_fn
      arguments: (argument_list . (string (string_content) @import.module))) @import.range
    (#eq? @_fn "load")))
