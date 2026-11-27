//! Java's `imports` section: plain, static and wildcard imports reach `FileMapL1::imports`, which
//! is what `code_refs` `importers` and the dependents lookup read.

use super::l1::extract_l1;
use super::l3::dependents_of;

const SRC: &str = "\
package app;

import java.util.List;
import static org.junit.Assert.assertEquals;
import com.example.model.*;

class A {
    List<String> names;
}
";

/// The `module` of every import of `src`, in source order.
fn modules(src: &str) -> Vec<Option<String>> {
    extract_l1("java", src.as_bytes())
        .expect("extract l1")
        .imports
        .into_iter()
        .map(|import| import.module)
        .collect()
}

#[test]
fn plain_static_and_wildcard_imports_are_extracted() {
    assert_eq!(
        modules(SRC),
        [
            Some("java.util.List".to_string()),
            Some("org.junit.Assert.assertEquals".to_string()),
            Some("com.example.model".to_string()),
        ]
    );
}

#[test]
fn the_import_range_covers_the_whole_declaration() {
    let map = extract_l1("java", SRC.as_bytes()).expect("extract l1");
    let raws: Vec<&str> = map.imports.iter().map(|import| import.raw.as_str()).collect();
    assert_eq!(
        raws,
        [
            "import java.util.List;",
            "import static org.junit.Assert.assertEquals;",
            "import com.example.model.*;",
        ]
    );
    let first = &map.imports[0];
    assert_eq!(&SRC[first.start_byte as usize..first.end_byte as usize], first.raw);
}

#[test]
fn a_file_without_imports_has_none() {
    assert!(modules("package app;\n\nclass A {}\n").is_empty());
}

#[test]
fn dependents_match_a_type_a_package_and_a_static_member_on_whole_segments() {
    let map = extract_l1("java", SRC.as_bytes()).expect("extract l1");
    let index = [("A.java", map.imports)];
    for module in ["java.util.List", "com.example.model", "org.junit.Assert", "Assert"] {
        assert_eq!(dependents_of(module, &index).len(), 1, "{module}");
    }
    for module in ["java.util.Lis", "example.mod"] {
        assert!(dependents_of(module, &index).is_empty(), "{module}");
    }
}
