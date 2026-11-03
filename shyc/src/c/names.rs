//! The C names ShyC constructs mangle to.
//!
//! A method, a trait value constructor, and a generic instance each need a C
//! symbol of their own. The C source backend and the header export spell them
//! here and nowhere else: a header that declares a name the definition does not
//! have fails at link time, long after either printer could notice.
//!
//! The rules are the obvious ones: the receiver comes first, so a method of
//! `Point` is `Point_move`, and `Display::from<Point>()` is
//! `Display_from_Point`. A generic instance joins its base name and its
//! arguments, so `Vec<int>` is `Vec_int`.

/// The C type and function name of one generic instance: `Vec_int`.
pub(crate) fn instance(base: &str, args: &[String]) -> String {
    let mut name = type_part(base);
    for arg in args {
        name.push('_');
        name.push_str(&type_part(arg));
    }
    name
}

/// The C symbol of one method of a concrete receiver: `Point_move`.
pub(crate) fn method(receiver: &str, method: &str) -> String {
    format!("{}_{}", type_part(receiver), method)
}

/// The constructor that fills a trait value for one receiver:
/// `Display_from_Point`.
pub(crate) fn trait_value(trait_name: &str, receiver: &str) -> String {
    format!("{}_from_{}", type_part(trait_name), type_part(receiver))
}

/// One type's contribution to a mangled name.
///
/// Keywords that only describe storage or a tag are dropped, a pointer becomes
/// `_ptr`, and the one pointer type with a name of its own keeps it: `char *`
/// is `str`.
pub(crate) fn type_part(text: &str) -> String {
    let (mut base, pointers) = split(text);
    if base == "char" && pointers == 1 {
        return "str".to_string();
    }
    for _ in 0..pointers {
        base.push_str("_ptr");
    }
    base.replace(' ', "_")
}

/// Split a C type spelling into its base words and its pointer depth.
///
/// `unsigned long *` is `("unsigned long", 1)`; `struct Point` is
/// `("Point", 0)`. Every caller here resolves methods and fields by the base
/// name, so qualifiers and tag keywords carry no information.
pub(crate) fn split(text: &str) -> (String, usize) {
    let mut words = Vec::new();
    let mut pointers = 0;
    for word in text.split_whitespace() {
        match word {
            "const" | "volatile" | "struct" | "union" | "enum" | "restrict" => {}
            _ => {
                pointers += word.chars().filter(|character| *character == '*').count();
                let word = word.trim_matches('*');
                if !word.is_empty() {
                    words.push(word);
                }
            }
        }
    }
    (words.join(" "), pointers)
}
