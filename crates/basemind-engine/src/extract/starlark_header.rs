//! Where a Starlark rule invocation's header ends. A target such as
//! `py_library(name = "lib", srcs = [...])` has no body, so the generic signature would be the
//! whole call with every attribute; the rule name is what tells targets apart.

/// Byte length of the header of a call node (`py_library`), `None` for any other node kind (the
/// generic header rules then apply).
pub(super) fn header_len(kind: &str, text: &str) -> Option<usize> {
    if kind != "call" {
        return None;
    }
    text.find('(')
}

#[cfg(test)]
mod tests {
    use super::header_len;

    #[test]
    fn a_call_header_is_the_rule_name() {
        let text = "py_library(\n    name = \"lib\",\n)";
        assert_eq!(header_len("call", text).map(|len| &text[..len]), Some("py_library"));
    }

    #[test]
    fn other_kinds_are_left_to_the_generic_rules() {
        assert_eq!(header_len("function_definition", "def f():\n    pass"), None);
    }
}
