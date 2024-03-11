use once_cell::sync::Lazy;
use regex::Regex;
use ruff_diagnostics::{Diagnostic, Violation};
use ruff_macros::{derive_message_formats, violation};
use ruff_source_file::Locator;
use ruff_text_size::{TextRange, TextSize};

// see https://peps.python.org/pep-0263/
// utf-8 aliases: utf8, U8, UTF, cp65001 case and _- can be used interchangebly
// just added utf-8 to it
static IS_ENCODING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(^[ \t\f]*#.*?coding[:=][ \t]*((?i)u8|utf(_8|-8)?|cp65001)($| ).*)").unwrap()
});
static IS_UTF8_ENCODING: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(^[ \t\f]*#.*?coding[:=][ \t]*((?i)u8|utf(_8|-8)?|cp65001)($| ).*)").unwrap()
});

/// ## What it does
/// Checks for the file encoding in python files and emmits a message if the file encoding is not
/// utf-8
///
/// ## Why is this bad?
/// PEP8 recommends UTF-8 default encoding for Python files. See
/// https://peps.python.org/pep-0008/#source-file-encoding
#[violation]
pub struct BadFileEncoding;

impl Violation for BadFileEncoding {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("PEP8 recommends UTF-8 as encoding for Python files")
    }
}

pub(crate) fn bad_file_encoding(locator: &Locator) -> Option<Diagnostic> {
    // Only search the first 2 lines rest is not relevant
    let contents = locator.up_to(locator.floor_char_boundary(TextSize::new(2)));

    if IS_ENCODING.is_match(contents) && !IS_UTF8_ENCODING.is_match(contents) {
        return Some(Diagnostic::new(BadFileEncoding, TextRange::default()));
    }
    // try out if there is an encoding in the second line
    if contents.starts_with("#!") {
        let try_second = contents.split_once('\n');
        match try_second {
            Some((_, second)) => {
                if IS_ENCODING.is_match(second) && !IS_UTF8_ENCODING.is_match(second) {
                    return Some(Diagnostic::new(BadFileEncoding, TextRange::default()));
                }
            }
            None => {
                return None;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use crate::registry::Rule;
    use crate::test::test_snippet;
    use crate::{assert_messages, settings};

    #[test]
    fn utf8_file_encoding() {
        let diagnostics = test_snippet(
            r"
#!/usr/bin/python
# -*- coding: utf-8 -*-
import os, sys
"
            .trim(),
            &settings::LinterSettings::for_rules(vec![Rule::BadFileEncoding]),
        );
        assert_messages!(diagnostics);
    }

    #[test]
    fn latin1_file_encoding() {
        let diagnostics = test_snippet(
            r"
#!/usr/bin/python
# -*- coding: latin-1 -*-
import os, sys
"
            .trim(),
            &settings::LinterSettings::for_rules(vec![Rule::BadFileEncoding]),
        );
        assert_messages!(diagnostics);
    }
}
