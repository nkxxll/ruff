//! Lint rules based on token traversal.

use std::path::Path;

use ruff_notebook::CellOffsets;
use ruff_python_ast::PySourceType;
use ruff_python_codegen::Stylist;
use ruff_python_parser::lexer::LexResult;

use ruff_diagnostics::Diagnostic;
use ruff_python_index::Indexer;
use ruff_source_file::Locator;

use crate::directives::TodoComment;
use crate::registry::{AsRule, Rule};
use crate::rules::pycodestyle::rules::BlankLinesChecker;
use crate::rules::{
    eradicate, flake8_commas, flake8_executable, flake8_fixme, flake8_implicit_str_concat,
    flake8_pyi, flake8_quotes, flake8_todos, pycodestyle, pygrep_hooks, pylint, pyupgrade, ruff,
};
use crate::settings::LinterSettings;

#[allow(clippy::too_many_arguments)]
pub(crate) fn check_tokens(
    tokens: &[LexResult],
    path: &Path,
    locator: &Locator,
    indexer: &Indexer,
    stylist: &Stylist,
    settings: &LinterSettings,
    source_type: PySourceType,
    cell_offsets: Option<&CellOffsets>,
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = vec![];

    if settings.rules.any_enabled(&[
        Rule::BlankLineBetweenMethods,
        Rule::BlankLinesTopLevel,
        Rule::TooManyBlankLines,
        Rule::BlankLineAfterDecorator,
        Rule::BlankLinesAfterFunctionOrClass,
        Rule::BlankLinesBeforeNestedDefinition,
    ]) {
        let mut blank_lines_checker = BlankLinesChecker::default();
        blank_lines_checker.check_lines(
            tokens,
            locator,
            stylist,
            settings.tab_size,
            &mut diagnostics,
        );
    }

    if settings.rules.enabled(Rule::BlanketNOQA) {
        pygrep_hooks::rules::blanket_noqa(&mut diagnostics, indexer, locator);
    }

    if settings.rules.enabled(Rule::BlanketTypeIgnore) {
        pygrep_hooks::rules::blanket_type_ignore(&mut diagnostics, indexer, locator);
    }

    if settings.rules.enabled(Rule::EmptyComment) {
        pylint::rules::empty_comments(&mut diagnostics, indexer, locator);
    }

    if settings
        .rules
        .enabled(Rule::AmbiguousUnicodeCharacterComment)
    {
        for range in indexer.comment_ranges() {
            ruff::rules::ambiguous_unicode_character_comment(
                &mut diagnostics,
                locator,
                *range,
                settings,
            );
        }
    }

    if settings.rules.enabled(Rule::CommentedOutCode) {
        eradicate::rules::commented_out_code(&mut diagnostics, locator, indexer, settings);
    }

    if settings.rules.enabled(Rule::UTF8EncodingDeclaration) {
        pyupgrade::rules::unnecessary_coding_comment(&mut diagnostics, locator, indexer);
    }

    if settings.rules.enabled(Rule::InvalidEscapeSequence) {
        for (tok, range) in tokens.iter().flatten() {
            pycodestyle::rules::invalid_escape_sequence(
                &mut diagnostics,
                locator,
                indexer,
                tok,
                *range,
            );
        }
    }

    if settings.rules.enabled(Rule::TabIndentation) {
        pycodestyle::rules::tab_indentation(&mut diagnostics, locator, indexer);
    }

    if settings.rules.any_enabled(&[
        Rule::InvalidCharacterBackspace,
        Rule::InvalidCharacterSub,
        Rule::InvalidCharacterEsc,
        Rule::InvalidCharacterNul,
        Rule::InvalidCharacterZeroWidthSpace,
    ]) {
        for (tok, range) in tokens.iter().flatten() {
            pylint::rules::invalid_string_characters(&mut diagnostics, tok, *range, locator);
        }
    }

    if settings.rules.any_enabled(&[
        Rule::MultipleStatementsOnOneLineColon,
        Rule::MultipleStatementsOnOneLineSemicolon,
        Rule::UselessSemicolon,
    ]) {
        pycodestyle::rules::compound_statements(
            &mut diagnostics,
            tokens,
            locator,
            indexer,
            source_type,
            cell_offsets,
        );
    }

    if settings.rules.enabled(Rule::AvoidableEscapedQuote) && settings.flake8_quotes.avoid_escape {
        flake8_quotes::rules::avoidable_escaped_quote(&mut diagnostics, tokens, locator, settings);
    }

    if settings.rules.enabled(Rule::UnnecessaryEscapedQuote) {
        flake8_quotes::rules::unnecessary_escaped_quote(&mut diagnostics, tokens, locator);
    }

    if settings.rules.any_enabled(&[
        Rule::BadQuotesInlineString,
        Rule::BadQuotesMultilineString,
        Rule::BadQuotesDocstring,
    ]) {
        flake8_quotes::rules::check_string_quotes(&mut diagnostics, tokens, locator, settings);
    }

    if settings.rules.any_enabled(&[
        Rule::SingleLineImplicitStringConcatenation,
        Rule::MultiLineImplicitStringConcatenation,
    ]) {
        flake8_implicit_str_concat::rules::implicit(
            &mut diagnostics,
            tokens,
            settings,
            locator,
            indexer,
        );
    }

    if settings.rules.any_enabled(&[
        Rule::MissingTrailingComma,
        Rule::TrailingCommaOnBareTuple,
        Rule::ProhibitedTrailingComma,
    ]) {
        flake8_commas::rules::trailing_commas(&mut diagnostics, tokens, locator, indexer);
    }

    if settings.rules.enabled(Rule::ExtraneousParentheses) {
        pyupgrade::rules::extraneous_parentheses(&mut diagnostics, tokens, locator);
    }

    if source_type.is_stub() && settings.rules.enabled(Rule::TypeCommentInStub) {
        flake8_pyi::rules::type_comment_in_stub(&mut diagnostics, locator, indexer);
    }

    if settings.rules.any_enabled(&[
        Rule::ShebangNotExecutable,
        Rule::ShebangMissingExecutableFile,
        Rule::ShebangLeadingWhitespace,
        Rule::ShebangNotFirstLine,
        Rule::ShebangMissingPython,
    ]) {
        flake8_executable::rules::from_tokens(&mut diagnostics, path, locator, indexer);
    }

    if settings.rules.any_enabled(&[
        Rule::InvalidTodoTag,
        Rule::MissingTodoAuthor,
        Rule::MissingTodoLink,
        Rule::MissingTodoColon,
        Rule::MissingTodoDescription,
        Rule::InvalidTodoCapitalization,
        Rule::MissingSpaceAfterTodoColon,
        Rule::LineContainsFixme,
        Rule::LineContainsXxx,
        Rule::LineContainsTodo,
        Rule::LineContainsHack,
    ]) {
        let todo_comments: Vec<TodoComment> = indexer
            .comment_ranges()
            .iter()
            .enumerate()
            .filter_map(|(i, comment_range)| {
                let comment = locator.slice(*comment_range);
                TodoComment::from_comment(comment, *comment_range, i)
            })
            .collect();
        flake8_todos::rules::todos(&mut diagnostics, &todo_comments, locator, indexer);
        flake8_fixme::rules::todos(&mut diagnostics, &todo_comments);
    }

    diagnostics.retain(|diagnostic| settings.rules.enabled(diagnostic.kind.rule()));

    diagnostics
}
