//! Byte-offset to LSP-position conversion.
//!
//! The compiler counts byte offsets; LSP positions count UTF-16 code units.
//! Everything that crosses that boundary goes through [`LineIndex`] so the
//! conversion exists in one place.

use std::path::Path;

use tower_lsp::lsp_types::{Position, Range, Url};

#[derive(Debug, Clone)]
pub struct LineIndex {
    starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(text: &str) -> Self {
        let mut starts = vec![0];
        starts.extend(text.match_indices('\n').map(|(offset, _)| offset + 1));
        Self { starts }
    }

    pub fn position(&self, text: &str, offset: usize) -> Position {
        let offset = offset.min(text.len());
        let line = self.starts.partition_point(|&start| start <= offset) - 1;
        let column = text[self.starts[line]..offset].encode_utf16().count();
        Position {
            line: line as u32,
            character: column as u32,
        }
    }

    pub fn offset(&self, text: &str, position: Position) -> usize {
        let line = (position.line as usize).min(self.starts.len() - 1);
        let start = self.starts[line];
        let end = self
            .starts
            .get(line + 1)
            .map(|next| next.saturating_sub(1))
            .unwrap_or(text.len());
        let mut units = 0;
        for (index, character) in text[start..end].char_indices() {
            if units >= position.character as usize {
                return start + index;
            }
            units += character.len_utf16();
        }
        end
    }

    pub fn range(&self, text: &str, start: usize, end: usize) -> Range {
        Range {
            start: self.position(text, start),
            end: self.position(text, end.max(start)),
        }
    }
}

/// The identifier around `offset`, if there is one.
///
/// A cursor placed at the end of an identifier still selects that identifier,
/// which is what editors expect for hover and go-to-definition.
pub fn word_span(text: &str, offset: usize) -> Option<(usize, usize)> {
    let offset = offset.min(text.len());
    let mut start = offset;
    while start > 0 {
        let character = text[..start].chars().next_back()?;
        if !is_ident_char(character) {
            break;
        }
        start -= character.len_utf8();
    }

    let mut end = offset;
    while end < text.len() {
        let character = text[end..].chars().next()?;
        if !is_ident_char(character) {
            break;
        }
        end += character.len_utf8();
    }

    if start == end {
        None
    } else {
        Some((start, end))
    }
}

fn is_ident_char(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

pub fn uri_for_path(path: &Path) -> Option<Url> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    Url::from_file_path(path).ok()
}
