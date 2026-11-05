//! The few names the C backend still makes up.
//!
//! Almost every symbol in the C output is the compiler's: a method is
//! `____Point__move`, a file-local function is `.L.module.name`, and a generic
//! instance is whatever the instance table or the type printer calls it. What is
//! left here is the spelling of a trait value constructor, which exists only for
//! the C output: `Display::from<Point>()` prints as `Display_from_Point()`.

/// Make a compiler symbol a C identifier.
///
/// A file-local symbol is `.L.module.name` and a method is `____Point__move`;
/// the dot cannot appear in C, so every character that is not an identifier
/// character becomes `_`. Nothing else is renamed: the C output calls a symbol
/// what the compiler called it.
pub(crate) fn symbol(symbol: &str) -> String {
    symbol
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '_' {
                character
            } else {
                '_'
            }
        })
        .collect()
}

/// The constructor that fills a trait value for one receiver:
/// `Display_from_Point`.
pub(crate) fn trait_value(trait_name: &str, receiver: &str) -> String {
    format!("{}_from_{}", ident(trait_name), ident(receiver))
}

/// One type's contribution to a constructor name.
pub(crate) fn ident(text: &str) -> String {
    let (base, pointers) = split(text);
    let mut name = base.replace(' ', "_");
    for _ in 0..pointers {
        name.push_str("_ptr");
    }
    name
}

/// Split a C type spelling into its base words and its pointer depth.
///
/// `unsigned long *` is `("unsigned long", 1)`; `struct Point` is
/// `("Point", 0)`. Every caller here resolves methods and fields by the base
/// name, so qualifiers and tag keywords carry no information, and an array
/// keeps the type of one element: `file [16]` is `("file", 0)`.
pub(crate) fn split(text: &str) -> (String, usize) {
    let mut words = Vec::new();
    let mut pointers = 0;
    for word in text.split_whitespace() {
        match word {
            "const" | "volatile" | "struct" | "union" | "enum" | "restrict" => {}
            _ => {
                pointers += word.chars().filter(|character| *character == '*').count();
                let word = word.trim_matches('*');
                let word = word.split('[').next().unwrap_or(word);
                if !word.is_empty() {
                    words.push(word);
                }
            }
        }
    }
    (words.join(" "), pointers)
}
