# shellcheck shell=bash
# JSON for the basemind hooks in bash and POSIX awk alone. The host's hook event and the CLI's
# answers are JSON, and jq is missing on Git for Windows and on many minimal images, where a hook
# that needs it would report a working basemind as unavailable. Sourced by the hooks, never run.
#
# json_fields reads one JSON document on stdin and lists every value in it, one line each:
#
#   <path> TAB <type> TAB <value>
#
# A path joins object keys and array indices with dots (`tool_input.command`, `messages.0.id`; the
# root is the empty path). A dot inside a key is written `\.`, so a key named `a.b` never reads as
# the member `b` of `a`. The type is object, array, string, number, boolean or null. A string's
# value is decoded and then encoded for the line: a backslash as `\\`, a line feed, tab or carriage
# return as `\n`, `\t` or `\r`, so json_get can turn it back with printf %b. A container's value is
# empty and its line comes before its members'. A document that does not parse lists nothing.

# shellcheck disable=SC2016 # an awk program: its $0 is awk's, not the shell's.
JSON_FIELDS_AWK='
function skip_ws() {
  while (pos <= n && (ch[pos] == " " || ch[pos] == "\t" || ch[pos] == "\n" || ch[pos] == "\r")) pos++
}
function put(path, type, value) { out = out path "\t" type "\t" value "\n" }
function member(path, key,    k, at) {
  k = ""
  while ((at = index(key, ".")) > 0) {
    k = k substr(key, 1, at - 1) "\\."
    key = substr(key, at + 1)
  }
  k = k key
  return path == "" ? k : (path "." k)
}
function hex4(at,    i, d, v) {
  v = 0
  for (i = 0; i < 4; i++) {
    if (ch[at + i] == "") return -1
    d = index("0123456789abcdef", tolower(ch[at + i]))
    if (d == 0) return -1
    v = v * 16 + d - 1
  }
  return v
}
function codepoint(c) {
  if (c == 10) return "\\n"
  if (c == 9) return "\\t"
  if (c == 13) return "\\r"
  if (c == 92) return "\\\\"
  if (c == 0) return ""
  if (c >= 55296 && c < 57344) c = 65533
  if (c < 128) return sprintf("%c", c)
  if (c < 2048) return sprintf("%c%c", 192 + int(c / 64), 128 + c % 64)
  if (c < 65536) return sprintf("%c%c%c", 224 + int(c / 4096), 128 + int(c / 64) % 64, 128 + c % 64)
  return sprintf("%c%c%c%c", 240 + int(c / 262144), 128 + int(c / 4096) % 64, 128 + int(c / 64) % 64, 128 + c % 64)
}
function escaped(e,    code, low) {
  if (e == "\"" || e == "/") return e
  if (e == "\\") return "\\\\"
  if (e == "n") return "\\n"
  if (e == "t") return "\\t"
  if (e == "r") return "\\r"
  if (e == "b") return sprintf("%c", 8)
  if (e == "f") return sprintf("%c", 12)
  if (e != "u") { failed = 1; return "" }
  code = hex4(pos)
  if (code < 0) { failed = 1; return "" }
  pos += 4
  if (code >= 55296 && code < 56320 && ch[pos] == "\\" && ch[pos + 1] == "u") {
    low = hex4(pos + 2)
    if (low >= 56320 && low < 57344) {
      code = 65536 + (code - 55296) * 1024 + low - 56320
      pos += 6
    }
  }
  return codepoint(code)
}
function str(    s, start, c) {
  pos++
  s = ""
  start = pos
  while (pos <= n) {
    c = ch[pos]
    if (c != "\"" && c != "\\" && c >= " ") {
      pos++
      continue
    }
    if (pos > start) s = s substr(text, start, pos - start)
    if (c == "\"") { pos++; return s }
    if (c != "\\") break
    pos += 2
    s = s escaped(ch[pos - 1])
    if (failed) return ""
    start = pos
  }
  failed = 1
  return ""
}
function scalar(path,    start, word) {
  start = pos
  while (pos <= n && index("+-.0123456789eE", ch[pos])) pos++
  if (pos > start) { put(path, "number", substr(text, start, pos - start)); return }
  word = ch[pos] ch[pos + 1] ch[pos + 2] ch[pos + 3]
  if (word == "true") { pos += 4; put(path, "boolean", "true"); return }
  if (word == "null") { pos += 4; put(path, "null", ""); return }
  if (word ch[pos + 4] == "false") { pos += 5; put(path, "boolean", "false"); return }
  failed = 1
}
function object(path,    key) {
  put(path, "object", "")
  pos++
  skip_ws()
  if (ch[pos] == "}") { pos++; return }
  for (;;) {
    if (ch[pos] != "\"") { failed = 1; return }
    key = str()
    if (failed) return
    skip_ws()
    if (ch[pos] != ":") { failed = 1; return }
    pos++
    skip_ws()
    value(member(path, key))
    if (failed) return
    skip_ws()
    if (ch[pos] == "}") { pos++; return }
    if (ch[pos] != ",") { failed = 1; return }
    pos++
    skip_ws()
  }
}
function array(path,    i) {
  put(path, "array", "")
  pos++
  skip_ws()
  if (ch[pos] == "]") { pos++; return }
  for (i = 0; ; i++) {
    value(member(path, i))
    if (failed) return
    skip_ws()
    if (ch[pos] == "]") { pos++; return }
    if (ch[pos] != ",") { failed = 1; return }
    pos++
    skip_ws()
  }
}
function value(path,    s) {
  if (ch[pos] == "{") object(path)
  else if (ch[pos] == "[") array(path)
  else if (ch[pos] == "\"") {
    s = str()
    if (!failed) put(path, "string", s)
  } else scalar(path)
}
{ text = text $0 "\n" }
END {
  n = split(text, ch, "")
  pos = 1
  skip_ws()
  value("")
  skip_ws()
  if (failed || pos <= n) exit 1
  printf "%s", out
}
'

# List the JSON document on stdin as json_fields lines; nothing when it does not parse.
json_fields() {
  LC_ALL=C awk "${JSON_FIELDS_AWK}" 2>/dev/null || true
}

# Print the decoded value at path $2 of the json_fields listing $1, when it is there and, given a
# type $3, of that type.
json_get() {
  local raw
  raw="$(printf '%s\n' "$1" | LC_ALL=C awk -F '\t' -v want="$2" -v type="${3:-}" \
    '!done && $1 == want && (type == "" || $2 == type) { print $3; done = 1 }')"
  printf '%b' "${raw}"
}

# Print how many members the array at path $2 of the json_fields listing $1 has; nothing when that
# path is not an array.
json_length() {
  printf '%s\n' "$1" | LC_ALL=C awk -F '\t' -v want="$2" '
    $1 == want { found = ($2 == "array") }
    found && index($1, want ".") == 1 && substr($1, length(want) + 2) ~ /^[0-9]+$/ { count++ }
    END { if (found) print count + 0 }'
}

# Print $1 escaped for the inside of a JSON string.
json_escape() {
  local s="$1" code char
  s="${s//\\/\\\\}"
  s="${s//\"/\\\"}"
  s="${s//$'\n'/\\n}"
  s="${s//$'\r'/\\r}"
  s="${s//$'\t'/\\t}"
  for code in 01 02 03 04 05 06 07 08 0b 0c 0e 0f 10 11 12 13 14 15 16 17 18 19 1a 1b 1c 1d 1e 1f; do
    printf -v char '%b' "\\x${code}"
    s="${s//${char}/\\u00${code}}"
  done
  printf '%s' "${s}"
}
