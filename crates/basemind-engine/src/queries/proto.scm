;; section: symbols
;;
;; Protocol Buffers. Every declaration is indexed wherever it nests, under its own unqualified
;; name (`Line` of `Invoice.Line`); the signature is the header up to the opening brace or the
;; semicolon (`rpc Create(Invoice) returns (Invoice)`, `enum Kind`).

;; `package acme.billing.v1;`, named by its dotted path.
(package (full_ident) @symbol.name) @symbol.namespace

;; `message Invoice {}`, including nested messages.
(message
  (message_name (identifier) @symbol.name)) @symbol.struct

;; `enum Status {}` and each of its values.
(enum
  (enum_name (identifier) @symbol.name)) @symbol.enum

(enum_field
  (identifier) @symbol.name) @symbol.enum_variant

;; `service Billing {}` and its `rpc` methods; the signature carries the request and response types.
(service
  (service_name (identifier) @symbol.name)) @symbol.interface

(rpc
  (rpc_name (identifier) @symbol.name)) @symbol.method

;; `oneof payment {}` inside a message.
(oneof
  (identifier) @symbol.name) @symbol.type

;; `extend google.protobuf.MessageOptions {}`, named by the extended type.
(extend
  (full_ident) @symbol.name) @symbol.impl

;; File-level `option java_package = "...";`; options inside messages, enums and rpcs are not.
(source_file
  (option (identifier) @symbol.name) @symbol.variable)

;; section: imports
;;
;; `import "acme/common.proto";`, also `import public` and `import weak`. The module is the quoted
;; path, which is relative to a proto source root.

(import
  path: (string (string_content) @import.module)) @import.range
