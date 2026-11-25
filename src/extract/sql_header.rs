//! Where a SQL `CREATE` statement's header ends. The grammar has no `body` field, so the generic
//! signature would run to the end of the statement (every column of a table, the whole body of a
//! function); the header is the part that names and types the object.

/// Statement kinds whose header stops at the column list, the `AS` clause or the first `(`.
const OBJECT_KINDS: &[&str] = &[
    "create_table",
    "create_view",
    "create_materialized_view",
    "create_type",
    "create_index",
    "create_trigger",
    "create_sequence",
    "create_schema",
];

/// Routine kinds: the parameter list and return type belong to the header, so it stops at the first
/// clause that starts the definition.
const ROUTINE_KINDS: &[&str] = &["create_function", "create_procedure"];

const OBJECT_STOP_WORDS: &[&str] = &["as"];
const ROUTINE_STOP_WORDS: &[&str] = &["as", "language", "begin"];

/// Byte length of the header of a `CREATE` statement of grammar kind `kind`, `None` for any other
/// kind (the generic header rules then apply).
pub(super) fn header_len(kind: &str, text: &str) -> Option<usize> {
    let (stop_words, stops_at_paren) = if OBJECT_KINDS.contains(&kind) {
        (OBJECT_STOP_WORDS, true)
    } else if ROUTINE_KINDS.contains(&kind) {
        (ROUTINE_STOP_WORDS, false)
    } else {
        return None;
    };
    let word_start = text.split_whitespace().find_map(|word| {
        let is_stop = stop_words.iter().any(|stop| word.eq_ignore_ascii_case(stop));
        is_stop.then(|| word.as_ptr() as usize - text.as_ptr() as usize)
    });
    let paren = if stops_at_paren { text.find('(') } else { None };
    Some(word_start.into_iter().chain(paren).min().unwrap_or(text.len()))
}

#[cfg(test)]
mod tests {
    use super::header_len;

    fn header<'a>(kind: &str, text: &'a str) -> Option<&'a str> {
        header_len(kind, text).map(|len| &text[..len])
    }

    #[test]
    fn object_headers_stop_before_the_column_list_or_as_clause() {
        let cases = [
            ("create_table", "CREATE TABLE public.users (id int)", "CREATE TABLE public.users "),
            ("create_view", "CREATE VIEW v\nAS SELECT 1", "CREATE VIEW v\n"),
            ("create_index", "CREATE INDEX i ON t (c)", "CREATE INDEX i ON t "),
            ("create_type", "create type mood as enum ('a')", "create type mood "),
        ];
        for (kind, text, want) in cases {
            assert_eq!(header(kind, text), Some(want), "{text}");
        }
    }

    #[test]
    fn routine_headers_keep_parameters_and_return_type() {
        let text = "CREATE FUNCTION add(a int, b int) RETURNS int LANGUAGE sql AS $$ select 1 $$";
        assert_eq!(header("create_function", text), Some("CREATE FUNCTION add(a int, b int) RETURNS int "));
    }

    #[test]
    fn other_kinds_are_left_to_the_generic_rules() {
        assert_eq!(header("function_definition", "greet() { :; }"), None);
    }
}
