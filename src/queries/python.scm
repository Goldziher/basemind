;; section: symbols
;;
;; The plain `function_definition` / `class_definition` patterns match BOTH bare definitions
;; and the inner definition of a `decorated_definition`. To capture decorator metadata we add
;; a second pair of patterns that target `decorated_definition` and capture each decorator
;; AND the inner symbol node — using the inner node so the (start_byte, name) dedupe key
;; aligns with the plain pattern. The dedupe pass in extract/l1.rs merges decorators across
;; the two matches.

(function_definition name: (identifier) @symbol.name) @symbol.function

(class_definition name: (identifier) @symbol.name) @symbol.class

;; Decorated function — `@symbol.function` capture is on the inner function_definition so
;; start_byte matches the bare pattern above; decorators are surfaced as a separate capture.
(decorated_definition
  (decorator) @symbol.decorator
  definition: (function_definition name: (identifier) @symbol.name) @symbol.function)

;; Decorated class — same shape.
(decorated_definition
  (decorator) @symbol.decorator
  definition: (class_definition name: (identifier) @symbol.name) @symbol.class)

(assignment
  left: (identifier) @symbol.name
  right: (_)) @symbol.const

;; section: imports

(import_statement) @import.range
(import_from_statement) @import.range

;; section: implementations
;;
;; `class Foo(Bar, Baz):` — one match per base class. The @impl.implementor capture
;; is on the class_definition node's name field and @impl.trait_name on each argument.

(class_definition
  name: (identifier) @impl.implementor
  superclasses: (argument_list
    (identifier) @impl.trait_name)) @impl.range

;; Dotted base: `class Foo(some.Bar):`
(class_definition
  name: (identifier) @impl.implementor
  superclasses: (argument_list
    (attribute attribute: (identifier) @impl.trait_name))) @impl.range

;; section: calls

(call function: (identifier) @call.callee) @call.range
(call function: (attribute attribute: (identifier) @call.callee)) @call.range

;; section: routes
;;
;; HTTP routes and the mounts that prefix them (ADR-0048, `src/extract/http.rs`).
;; FastAPI `@app.get("/users/{id}")` and Flask `@app.route("/users/<int:id>", methods=[..])` on a
;; function: the function is the handler.

(decorated_definition
  (decorator
    (call
      function: (attribute
        object: (identifier) @route.receiver
        attribute: (identifier) @route.method)
      arguments: (argument_list . (string) @route.path) @route.args))
  definition: (function_definition name: (identifier) @route.handler)
  (#any-of? @route.method "get" "post" "put" "delete" "patch" "head" "options" "route" "api_route"))
  @route.range

;; `router = APIRouter(prefix="/users")`, `bp = Blueprint("bp", __name__, url_prefix="/bp")`.
(assignment
  left: (identifier) @mount.name
  right: (call
    function: [(identifier) (attribute)] @_ctor
    arguments: (argument_list
      (keyword_argument name: (identifier) @_kw value: (string) @mount.path)))
  (#match? @_ctor "(^|[.])(APIRouter|Blueprint)$")
  (#any-of? @_kw "prefix" "url_prefix"))

;; `app.include_router(router, prefix="/api")`, `app.register_blueprint(bp, url_prefix="/x")`.
(call
  function: (attribute
    object: (identifier) @mount.parent
    attribute: (identifier) @_inc)
  arguments: (argument_list
    . (identifier) @mount.name
    (keyword_argument name: (identifier) @_kw value: (string) @mount.path))
  (#any-of? @_inc "include_router" "register_blueprint")
  (#any-of? @_kw "prefix" "url_prefix"))

;; section: http_calls
;;
;; Literal-URL requests: `requests.get(url)`, `httpx.post(url)`, `requests.request("PUT", url)`.
;; Through any other receiver (a client or session) the URL must be absolute http(s).

(call
  function: (attribute
    object: (_) @http.receiver
    attribute: (identifier) @http.method)
  arguments: (argument_list) @http.args
  (#any-of? @http.method "get" "post" "put" "delete" "patch" "head" "options" "request")) @http.range

;; section: docs

(comment) @doc.text
