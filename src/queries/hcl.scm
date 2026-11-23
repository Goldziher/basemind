;; section: symbols
;;
;; Shared by `hcl` (`*.hcl`, Terragrunt) and `terraform` (`*.tf`, `*.tfvars`), which parse to the
;; same block and attribute shapes. Only top-level blocks are declarations; the signature is the
;; block header (`resource "aws_s3_bucket" "logs"`), cut at the opening brace.

;; `resource "<type>" "<name>" {}` and `data "<type>" "<name>" {}`, named by the second label.
;; A label is a quoted string or a bare identifier (`resource aws_s3_bucket logs {}`).
(config_file
  (body
    (block
      . (identifier) @_kind
      . [(string_lit) (identifier)]
      . [(string_lit (template_literal) @symbol.name) (identifier) @symbol.name]) @symbol.class
    (#any-of? @_kind "resource" "data")))

;; `variable "<name>" {}`
(config_file
  (body
    (block
      . (identifier) @_kind
      . [(string_lit (template_literal) @symbol.name) (identifier) @symbol.name]) @symbol.variable
    (#eq? @_kind "variable")))

;; `output "<name>" {}`
(config_file
  (body
    (block
      . (identifier) @_kind
      . [(string_lit (template_literal) @symbol.name) (identifier) @symbol.name]) @symbol.const
    (#eq? @_kind "output")))

;; `module "<name>" {}`, and Terragrunt `dependency "<name>" {}` and `include "<name>" {}`.
(config_file
  (body
    (block
      . (identifier) @_kind
      . [(string_lit (template_literal) @symbol.name) (identifier) @symbol.name]) @symbol.module
    (#any-of? @_kind "module" "dependency" "include")))

;; `provider "<name>" {}`
(config_file
  (body
    (block
      . (identifier) @_kind
      . [(string_lit (template_literal) @symbol.name) (identifier) @symbol.name]) @symbol.namespace
    (#eq? @_kind "provider")))

;; Blocks without a label of their own: `terraform {}` and Terragrunt `include {}`, named by the
;; block type.
(config_file
  (body
    (block . (identifier) @symbol.name) @symbol.namespace
    (#eq? @symbol.name "terraform")))

(config_file
  (body
    (block . (identifier) @symbol.name . (block_start)) @symbol.module
    (#eq? @symbol.name "include")))

;; Each entry of a `locals {}` block.
(config_file
  (body
    (block
      . (identifier) @_kind
      (body (attribute . (identifier) @symbol.name) @symbol.variable))
    (#eq? @_kind "locals")))

;; Terragrunt `inputs = { ... }`.
(config_file
  (body
    (attribute . (identifier) @symbol.name) @symbol.variable
    (#eq? @symbol.name "inputs")))
