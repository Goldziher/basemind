;; section: symbols
;;
;; Shared by every shell the `bash` grammar parses (`*.sh`, `*.bash`, `*.zsh`). Functions are
;; declarations wherever they sit outside another function; variables are only those assigned at
;; the top level of the script (a `local` or an assignment inside a function is implementation).

;; `name() { ... }` and `function name { ... }`
(function_definition name: (word) @symbol.name) @symbol.function

;; `NAME=value`
(program
  (variable_assignment name: (variable_name) @symbol.name) @symbol.variable)

;; `A=1 B=2` on one line is a `variable_assignments` run.
(program
  (variable_assignments
    (variable_assignment name: (variable_name) @symbol.name) @symbol.variable))

;; `export NAME=value`, `readonly NAME=value`, `declare -x NAME=value`
(program
  (declaration_command
    (variable_assignment name: (variable_name) @symbol.name) @symbol.variable))
