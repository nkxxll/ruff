use tracing::Level;

use ruff_formatter::printer::SourceMapGeneration;
use ruff_formatter::{
    format, FormatContext, FormatError, FormatOptions, IndentStyle, PrintedRange, SourceCode,
};
use ruff_python_ast::visitor::preorder::{walk_body, PreorderVisitor, TraversalSignal};
use ruff_python_ast::{AnyNode, AnyNodeRef, Stmt, StmtMatch, StmtTry};
use ruff_python_index::tokens_and_ranges;
use ruff_python_parser::{parse_tokens, AsMode, ParseError, ParseErrorType};
use ruff_python_trivia::{indentation_at_offset, BackwardsTokenizer, SimpleToken, SimpleTokenKind};
use ruff_source_file::Locator;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::comments::Comments;
use crate::context::{IndentLevel, NodeLevel};
use crate::prelude::*;
use crate::statement::suite::DocstringStmt;
use crate::verbatim::{ends_suppression, starts_suppression};
use crate::{format_module_source, FormatModuleError, PyFormatOptions};

/// Formats the given `range` in source rather than the entire file.
///
/// The returned formatted range guarantees to cover at least `range` (excluding whitespace), but the range might be larger.
/// Some cases in which the returned range is larger than `range` are:
/// * The logical lines in `range` use a indentation different from the configured [`IndentStyle`] and [`IndentWidth`].
/// * `range` is smaller than a logical lines and the formatter needs to format the entire logical line.
/// * `range` falls on a single line body.
///
/// The formatting of logical lines using range formatting should produce the same result as when formatting the entire document (for the same lines and options).
///
/// ## Implementation
///
/// This is an optimisation problem. The goal is to find the minimal range that fully covers `range`, is still formattable,
/// and produces the same result as when formatting the entire document.
///
/// The implementation performs the following steps:
/// 1. Find the deepest node that fully encloses `range`. The node with the minimum covering range.
/// 2. Try to narrow the range found in step one by searching its children and find node and comment start and end offsets that are closer to `range`'s start and end.
/// 3. Format the node from step 1 and use the source map information generated by the formatter to map the narrowed range in the source document to the range in the formatted output.
/// 4. Take the formatted code and return it.
///
/// # Error
/// Returns a range error if `range` lies outside of the source file.
///
/// # Panics
/// If `range` doesn't point to a valid char boundaries.
///
/// [`IndentWidth`]: `ruff_formatter::IndentWidth`
#[tracing::instrument(name = "format_range", level = Level::TRACE, skip_all)]
pub fn format_range(
    source: &str,
    range: TextRange,
    options: PyFormatOptions,
) -> Result<PrintedRange, FormatModuleError> {
    // Error if the specified range lies outside of the source file.
    if source.text_len() < range.end() {
        return Err(FormatModuleError::FormatError(FormatError::RangeError {
            input: range,
            tree: TextRange::up_to(source.text_len()),
        }));
    }

    // Formatting an empty string always yields an empty string. Return directly.
    if range.is_empty() {
        return Ok(PrintedRange::empty());
    }

    if range == TextRange::up_to(source.text_len()) {
        let formatted = format_module_source(source, options)?;
        return Ok(PrintedRange::new(formatted.into_code(), range));
    }

    let (tokens, comment_ranges) =
        tokens_and_ranges(source, options.source_type()).map_err(|err| ParseError {
            offset: err.location(),
            error: ParseErrorType::Lexical(err.into_error()),
        })?;

    assert_valid_char_boundaries(range, source);

    let module = parse_tokens(tokens, source, options.source_type().as_mode())?;
    let root = AnyNode::from(module);
    let source_code = SourceCode::new(source);
    let comments = Comments::from_ast(root.as_ref(), source_code, &comment_ranges);

    let mut context = PyFormatContext::new(
        options.with_source_map_generation(SourceMapGeneration::Enabled),
        source,
        comments,
    );

    let (enclosing_node, base_indent) = match find_enclosing_node(range, root.as_ref(), &context) {
        EnclosingNode::Node { node, indent_level } => (node, indent_level),
        EnclosingNode::Suppressed => {
            // The entire range falls into a suppressed range. There's nothing to format.
            return Ok(PrintedRange::empty());
        }
    };

    let narrowed_range = narrow_range(range, enclosing_node, &context);
    assert_valid_char_boundaries(narrowed_range, source);

    // Correctly initialize the node level for the blank line rules.
    if !enclosing_node.is_mod_module() {
        context.set_node_level(NodeLevel::CompoundStatement);
        context.set_indent_level(
            // Plus 1 because `IndentLevel=0` equals the module level.
            IndentLevel::new(base_indent.saturating_add(1)),
        );
    }

    let formatted = format!(
        context,
        [FormatEnclosingNode {
            root: enclosing_node
        }]
    )?;

    let printed = formatted.print_with_indent(base_indent)?;
    Ok(printed.slice_range(narrowed_range, source))
}

/// Finds the node with the minimum covering range of `range`.
///
/// It traverses the tree and returns the deepest node that fully encloses `range`.
///
/// ## Eligible nodes
/// The search is restricted to nodes that mark the start of a logical line to ensure
/// formatting a range results in the same formatting for that logical line as when formatting the entire document.
/// This property can't be guaranteed when supporting sub-expression formatting because
/// a) Adding parentheses around enclosing expressions can toggle an expression from non-splittable to splittable,
/// b) formatting a sub-expression has fewer split points than formatting the entire expressions.
///
/// ### Possible docstrings
/// Strings that are suspected to be docstrings are excluded from the search to format the enclosing suite instead
/// so that the formatter's docstring detection in [`FormatSuite`] correctly detects and formats the docstrings.
///
/// ### Compound statements with a simple statement body
/// Don't include simple-statement bodies of compound statements `if True: pass` because the formatter
/// must run [`FormatClauseBody`] to determine if the body should be collapsed or not.
///
/// ### Incorrectly indented code
/// Code that uses indentations that don't match the configured [`IndentStyle`] and [`IndentWidth`] are excluded from the search,
/// because formatting such nodes on their own can lead to indentation mismatch with its sibling nodes.
///
/// ## Suppression comments
/// The search ends when `range` falls into a suppressed range because there's nothing to format. It also avoids that the
/// formatter formats the statement because it doesn't see the suppression comment of the enclosing node.
///
/// The implementation doesn't handle `fmt: ignore` suppression comments because the statement's formatting logic
/// correctly detects the suppression comment and returns the statement text as is.
fn find_enclosing_node<'ast>(
    range: TextRange,
    root: AnyNodeRef<'ast>,
    context: &PyFormatContext<'ast>,
) -> EnclosingNode<'ast> {
    let mut visitor = FindEnclosingNode::new(range, context);

    if visitor.enter_node(root).is_traverse() {
        root.visit_preorder(&mut visitor);
    }
    visitor.leave_node(root);

    visitor.closest
}

struct FindEnclosingNode<'a, 'ast> {
    range: TextRange,
    context: &'a PyFormatContext<'ast>,

    /// The, to this point, deepest node that fully encloses `range`.
    closest: EnclosingNode<'ast>,

    /// Tracks if the current statement is suppressed
    suppressed: Suppressed,
}

impl<'a, 'ast> FindEnclosingNode<'a, 'ast> {
    fn new(range: TextRange, context: &'a PyFormatContext<'ast>) -> Self {
        Self {
            range,
            context,
            suppressed: Suppressed::No,
            closest: EnclosingNode::Suppressed,
        }
    }
}

impl<'ast> PreorderVisitor<'ast> for FindEnclosingNode<'_, 'ast> {
    fn enter_node(&mut self, node: AnyNodeRef<'ast>) -> TraversalSignal {
        if !(is_logical_line(node) || node.is_mod_module()) {
            return TraversalSignal::Skip;
        }

        // Handle `fmt: off` suppression comments for statements.
        if node.is_statement() {
            let leading_comments = self.context.comments().leading(node);
            self.suppressed = Suppressed::from(match self.suppressed {
                Suppressed::No => starts_suppression(leading_comments, self.context.source()),
                Suppressed::Yes => !ends_suppression(leading_comments, self.context.source()),
            });
        }

        if !node.range().contains_range(self.range) {
            return TraversalSignal::Skip;
        }

        if self.suppressed.is_yes() && node.is_statement() {
            self.closest = EnclosingNode::Suppressed;
            return TraversalSignal::Skip;
        }

        // Don't pick potential docstrings as the closest enclosing node because `suite.rs` than fails to identify them as
        // docstrings and docstring formatting won't kick in.
        // Format the enclosing node instead and slice the formatted docstring from the result.
        let is_maybe_docstring = node.as_stmt_expr().is_some_and(|stmt| {
            DocstringStmt::is_docstring_statement(stmt, self.context.options().source_type())
        });

        if is_maybe_docstring {
            return TraversalSignal::Skip;
        }

        // Only computing the count here is sufficient because each enclosing node ensures that it has the necessary indent
        // or we don't traverse otherwise.
        let Some(indent_level) =
            indent_level(node.start(), self.context.source(), self.context.options())
        else {
            // Non standard indent or a simple-statement body of a compound statement, format the enclosing node
            return TraversalSignal::Skip;
        };

        self.closest = EnclosingNode::Node { node, indent_level };

        TraversalSignal::Traverse
    }

    fn leave_node(&mut self, node: AnyNodeRef<'ast>) {
        if node.is_statement() {
            let trailing_comments = self.context.comments().trailing(node);
            // Update the suppressed state for the next statement.
            self.suppressed = Suppressed::from(match self.suppressed {
                Suppressed::No => starts_suppression(trailing_comments, self.context.source()),
                Suppressed::Yes => !ends_suppression(trailing_comments, self.context.source()),
            });
        }
    }

    fn visit_body(&mut self, body: &'ast [Stmt]) {
        // We only visit statements that aren't suppressed that's why we don't need to track the suppression
        // state in a stack. Assert that this assumption is safe.
        debug_assert!(self.suppressed.is_no());
        walk_body(self, body);
        self.suppressed = Suppressed::No;
    }
}

#[derive(Debug, Copy, Clone)]
enum EnclosingNode<'a> {
    /// The entire range falls into a suppressed `fmt: off` range.
    Suppressed,

    /// The node outside of a suppression range that fully encloses the searched range.
    Node {
        node: AnyNodeRef<'a>,
        indent_level: u16,
    },
}

/// Narrows the formatting `range` to a smaller sub-range than the enclosing node's range.
///
/// The range is narrowed by searching the enclosing node's children and:
/// * Find the closest node or comment start or end offset to `range.start`
/// * Find the closest node or comment start or end offset, or the clause header's `:` end offset to `range.end`
///
/// The search is restricted to positions where the formatter emits source map entries because it guarantees
/// that we know the exact range in the formatted range and not just an approximation that could include other tokens.
///
/// ## Clause Headers
/// For clause headers like `if`, `while`, `match`, `case` etc. consider the `:` end position for narrowing `range.end`
/// to support formatting the clause header without its body.
///
/// ## Compound statements with simple statement bodies
/// Similar to [`find_enclosing_node`], exclude the compound statement's body if it is a simple statement (not a suite) from the search to format the entire clause header
/// with the body. This ensures that the formatter runs [`FormatClauseBody`] that determines if the body should be indented.s
///
/// ## Non-standard indentation
/// Node's that use an indentation that doesn't match the configured [`IndentStyle`] and [`IndentWidth`] are excluded from the search.
/// This is because the formatter always uses the configured [`IndentStyle`] and [`IndentWidth`], resulting in the
/// formatted nodes using a different indentation than the unformatted sibling nodes. This would be tolerable
/// in non whitespace sensitive languages like JavaScript but results in lexical errors in Python.
///
/// ## Implementation
/// It would probably be possible to merge this visitor with [`FindEnclosingNode`] but they are separate because
/// it avoids some unnecessary work for nodes that aren't the `enclosing_node` and I found reasoning
/// and debugging the visiting logic easier when they are separate.
///
/// [`IndentStyle`]: ruff_formatter::IndentStyle
/// [`IndentWidth`]: ruff_formatter::IndentWidth
fn narrow_range(
    range: TextRange,
    enclosing_node: AnyNodeRef,
    context: &PyFormatContext,
) -> TextRange {
    let locator = context.locator();
    let enclosing_indent = indentation_at_offset(enclosing_node.start(), &locator)
        .expect("Expected enclosing to never be a same line body statement.");

    let mut visitor = NarrowRange {
        context,
        range,

        narrowed_start: enclosing_node.start(),
        narrowed_end: enclosing_node.end(),

        enclosing_indent,
        level: usize::from(!enclosing_node.is_mod_module()),
    };

    if visitor.enter_node(enclosing_node).is_traverse() {
        enclosing_node.visit_preorder(&mut visitor);
    }

    visitor.leave_node(enclosing_node);

    TextRange::new(visitor.narrowed_start, visitor.narrowed_end)
}

struct NarrowRange<'a> {
    context: &'a PyFormatContext<'a>,

    // The range to format
    range: TextRange,

    // The narrowed range
    narrowed_start: TextSize,
    narrowed_end: TextSize,

    // Stated tracked by the visitor
    enclosing_indent: &'a str,
    level: usize,
}

impl PreorderVisitor<'_> for NarrowRange<'_> {
    fn enter_node(&mut self, node: AnyNodeRef<'_>) -> TraversalSignal {
        if !(is_logical_line(node) || node.is_mod_module()) {
            return TraversalSignal::Skip;
        }

        // Find the start offset of the node that starts the closest to (and before) the start offset of the formatting range.
        // We do this by iterating over known positions that emit source map entries and pick the start point that ends closest
        // to the searched range's start.
        let leading_comments = self.context.comments().leading(node);
        self.narrow(leading_comments);
        self.narrow([node]);

        // Avoid traversing when it's known to not be able to narrow the range further to avoid traversing the entire tree (entire file in the worst case).
        // If the node's range is entirely before the searched range, don't traverse because non of its children
        // can be closer to `narrow_start` than the node itself (which we already narrowed).
        //
        // Don't traverse if the current node is passed the narrowed range (it's impossible to refine it further).
        if node.end() < self.range.start()
            || (self.narrowed_start > node.start() && self.narrowed_end <= node.end())
        {
            return TraversalSignal::Skip;
        }

        // Handle nodes that have indented child-nodes that aren't a `Body` (which is handled by `visit_body`).
        // Ideally, this would be handled as part of `visit_stmt` but `visit_stmt` doesn't get called for the `enclosing_node`
        // because it's not possible to convert` AnyNodeRef` to `&Stmt` :(
        match node {
            AnyNodeRef::StmtMatch(StmtMatch {
                subject: _,
                cases,
                range: _,
            }) => {
                if let Some(saved_state) = self.enter_level(cases.first().map(AnyNodeRef::from)) {
                    for match_case in cases {
                        self.visit_match_case(match_case);
                    }
                    self.leave_level(saved_state);
                }

                // Already traversed as part of `enter_node`.
                TraversalSignal::Skip
            }
            AnyNodeRef::StmtTry(StmtTry {
                body,
                handlers,
                orelse,
                finalbody,
                is_star: _,
                range: _,
            }) => {
                self.visit_body(body);
                if let Some(except_handler_saved) =
                    self.enter_level(handlers.first().map(AnyNodeRef::from))
                {
                    for except_handler in handlers {
                        self.visit_except_handler(except_handler);
                    }
                    self.leave_level(except_handler_saved);
                }
                self.visit_body(orelse);
                self.visit_body(finalbody);

                // Already traversed as part of `enter_node`.
                TraversalSignal::Skip
            }
            _ => TraversalSignal::Traverse,
        }
    }

    fn leave_node(&mut self, node: AnyNodeRef<'_>) {
        if !(is_logical_line(node) || node.is_mod_module()) {
            return;
        }

        // Find the end offset of the closest node to the end offset of the formatting range.
        // We do this by iterating over end positions that we know generate source map entries end pick the end
        // that ends closest or after the searched range's end.
        self.narrow(
            self.context
                .comments()
                .trailing(node)
                .iter()
                .filter(|comment| comment.line_position().is_own_line()),
        );
    }

    fn visit_body(&mut self, body: &'_ [Stmt]) {
        if let Some(saved_state) = self.enter_level(body.first().map(AnyNodeRef::from)) {
            walk_body(self, body);
            self.leave_level(saved_state);
        }
    }
}

impl NarrowRange<'_> {
    fn narrow<I, T>(&mut self, items: I)
    where
        I: IntoIterator<Item = T>,
        T: Ranged,
    {
        for ranged in items {
            self.narrow_offset(ranged.start());
            self.narrow_offset(ranged.end());
        }
    }

    fn narrow_offset(&mut self, offset: TextSize) {
        self.narrow_start(offset);
        self.narrow_end(offset);
    }

    fn narrow_start(&mut self, offset: TextSize) {
        if offset <= self.range.start() {
            self.narrowed_start = self.narrowed_start.max(offset);
        }
    }

    fn narrow_end(&mut self, offset: TextSize) {
        if offset >= self.range.end() {
            self.narrowed_end = self.narrowed_end.min(offset);
        }
    }

    fn enter_level(&mut self, first_child: Option<AnyNodeRef>) -> Option<SavedLevel> {
        if let Some(first_child) = first_child {
            // If this is a clause header and the `range` ends within the clause header, then avoid formatting the body.
            // This prevents that we format an entire function definition when the selected range is fully enclosed by the parameters.
            // ```python
            // 1| def foo(<RANGE_START>a, b, c<RANGE_END>):
            // 2|    pass
            // ```
            // We don't want to format the body of the function.
            if let Some(SimpleToken {
                kind: SimpleTokenKind::Colon,
                range: colon_range,
            }) = BackwardsTokenizer::up_to(
                first_child.start(),
                self.context.source(),
                self.context.comments().ranges(),
            )
            .skip_trivia()
            .next()
            {
                self.narrow_offset(colon_range.end());
            }

            // It is necessary to format all statements if the statement or any of its parents don't use the configured indentation.
            // ```python
            // 0| def foo():
            // 1|     if True:
            // 2|       print("Hello")
            // 3|       print("More")
            // 4|       a = 10
            // ```
            // Here, the `if` statement uses the correct 4 spaces indentation, but the two `print` statements use a 2 spaces indentation.
            // The formatter output uses 8 space indentation for the `print` statement which doesn't match the indentation of the statement on line 4 when
            // replacing the source with the formatted code. That's why we expand the range in this case to cover the entire if-body range.
            //
            // I explored the alternative of using `indent(dedent(formatted))` to retain the correct indentation. It works pretty well except that it can change the
            // content of multiline strings:
            // ```python
            // def test  ():
            //   pass
            //   <RANGE_START>1 + 2
            //   """A Multiline string
            //     that uses the same indentation as the formatted code will. This should not be dedented."""
            //
            //   print("Done")<RANGE_END>
            // ```
            // The challenge here is that the second line of the multiline string uses a 4 space indentation. Using `dedent` would
            // dedent the second line to 0 spaces and the `indent` then adds a 2 space indentation to match the indentation in the source.
            // This is incorrect because the leading whitespace is the content of the string and not indentation, resulting in changed string content.
            if let Some(indentation) =
                indentation_at_offset(first_child.start(), &self.context.locator())
            {
                let relative_indent = indentation.strip_prefix(self.enclosing_indent).unwrap();
                let expected_indents = self.level;

                // Each level must always add one level of indent. That's why an empty relative indent to the parent node tells us that the enclosing node is the Module.
                let has_expected_indentation = match self.context.options().indent_style() {
                    IndentStyle::Tab => {
                        relative_indent.len() == expected_indents
                            && relative_indent.chars().all(|c| c == '\t')
                    }
                    IndentStyle::Space => {
                        relative_indent.len()
                            == expected_indents
                                * self.context.options().indent_width().value() as usize
                            && relative_indent.chars().all(|c| c == ' ')
                    }
                };

                if !has_expected_indentation {
                    return None;
                }
            } else {
                // Simple-statement body of a compound statement (not a suite body).
                // Don't narrow the range because the formatter must run `FormatClauseBody` to determine if the body should be collapsed or not.
                return None;
            }
        }

        let saved_level = self.level;
        self.level += 1;

        Some(SavedLevel { level: saved_level })
    }

    #[allow(clippy::needless_pass_by_value)]
    fn leave_level(&mut self, saved_state: SavedLevel) {
        self.level = saved_state.level;
    }
}

pub(crate) const fn is_logical_line(node: AnyNodeRef) -> bool {
    // Make sure to update [`FormatEnclosingLine`] when changing this.
    node.is_statement()
        || node.is_decorator()
        || node.is_except_handler()
        || node.is_elif_else_clause()
        || node.is_match_case()
}

#[derive(Debug)]
struct SavedLevel {
    level: usize,
}

#[derive(Copy, Clone, Default, Debug)]
enum Suppressed {
    /// Code is not suppressed
    #[default]
    No,

    /// The node is suppressed by a suppression comment in the same body block.
    Yes,
}

impl Suppressed {
    const fn is_no(self) -> bool {
        matches!(self, Suppressed::No)
    }

    const fn is_yes(self) -> bool {
        matches!(self, Suppressed::Yes)
    }
}

impl From<bool> for Suppressed {
    fn from(value: bool) -> Self {
        if value {
            Suppressed::Yes
        } else {
            Suppressed::No
        }
    }
}

fn assert_valid_char_boundaries(range: TextRange, source: &str) {
    assert!(source.is_char_boundary(usize::from(range.start())));
    assert!(source.is_char_boundary(usize::from(range.end())));
}

struct FormatEnclosingNode<'a> {
    root: AnyNodeRef<'a>,
}

impl Format<PyFormatContext<'_>> for FormatEnclosingNode<'_> {
    fn fmt(&self, f: &mut Formatter<PyFormatContext<'_>>) -> FormatResult<()> {
        // Note: It's important that this supports formatting all nodes for which `is_logical_line`
        // returns + the root `Mod` nodes.
        match self.root {
            AnyNodeRef::ModModule(node) => node.format().fmt(f),
            AnyNodeRef::ModExpression(node) => node.format().fmt(f),
            AnyNodeRef::StmtFunctionDef(node) => node.format().fmt(f),
            AnyNodeRef::StmtClassDef(node) => node.format().fmt(f),
            AnyNodeRef::StmtReturn(node) => node.format().fmt(f),
            AnyNodeRef::StmtDelete(node) => node.format().fmt(f),
            AnyNodeRef::StmtTypeAlias(node) => node.format().fmt(f),
            AnyNodeRef::StmtAssign(node) => node.format().fmt(f),
            AnyNodeRef::StmtAugAssign(node) => node.format().fmt(f),
            AnyNodeRef::StmtAnnAssign(node) => node.format().fmt(f),
            AnyNodeRef::StmtFor(node) => node.format().fmt(f),
            AnyNodeRef::StmtWhile(node) => node.format().fmt(f),
            AnyNodeRef::StmtIf(node) => node.format().fmt(f),
            AnyNodeRef::StmtWith(node) => node.format().fmt(f),
            AnyNodeRef::StmtMatch(node) => node.format().fmt(f),
            AnyNodeRef::StmtRaise(node) => node.format().fmt(f),
            AnyNodeRef::StmtTry(node) => node.format().fmt(f),
            AnyNodeRef::StmtAssert(node) => node.format().fmt(f),
            AnyNodeRef::StmtImport(node) => node.format().fmt(f),
            AnyNodeRef::StmtImportFrom(node) => node.format().fmt(f),
            AnyNodeRef::StmtGlobal(node) => node.format().fmt(f),
            AnyNodeRef::StmtNonlocal(node) => node.format().fmt(f),
            AnyNodeRef::StmtExpr(node) => node.format().fmt(f),
            AnyNodeRef::StmtPass(node) => node.format().fmt(f),
            AnyNodeRef::StmtBreak(node) => node.format().fmt(f),
            AnyNodeRef::StmtContinue(node) => node.format().fmt(f),
            AnyNodeRef::StmtIpyEscapeCommand(node) => node.format().fmt(f),
            AnyNodeRef::ExceptHandlerExceptHandler(node) => node.format().fmt(f),
            AnyNodeRef::MatchCase(node) => node.format().fmt(f),
            AnyNodeRef::Decorator(node) => node.format().fmt(f),
            AnyNodeRef::ElifElseClause(node) => node.format().fmt(f),

            AnyNodeRef::ExprBoolOp(_)
            | AnyNodeRef::ExprNamedExpr(_)
            | AnyNodeRef::ExprBinOp(_)
            | AnyNodeRef::ExprUnaryOp(_)
            | AnyNodeRef::ExprLambda(_)
            | AnyNodeRef::ExprIfExp(_)
            | AnyNodeRef::ExprDict(_)
            | AnyNodeRef::ExprSet(_)
            | AnyNodeRef::ExprListComp(_)
            | AnyNodeRef::ExprSetComp(_)
            | AnyNodeRef::ExprDictComp(_)
            | AnyNodeRef::ExprGeneratorExp(_)
            | AnyNodeRef::ExprAwait(_)
            | AnyNodeRef::ExprYield(_)
            | AnyNodeRef::ExprYieldFrom(_)
            | AnyNodeRef::ExprCompare(_)
            | AnyNodeRef::ExprCall(_)
            | AnyNodeRef::FStringExpressionElement(_)
            | AnyNodeRef::FStringLiteralElement(_)
            | AnyNodeRef::FStringFormatSpec(_)
            | AnyNodeRef::ExprFString(_)
            | AnyNodeRef::ExprStringLiteral(_)
            | AnyNodeRef::ExprBytesLiteral(_)
            | AnyNodeRef::ExprNumberLiteral(_)
            | AnyNodeRef::ExprBooleanLiteral(_)
            | AnyNodeRef::ExprNoneLiteral(_)
            | AnyNodeRef::ExprEllipsisLiteral(_)
            | AnyNodeRef::ExprAttribute(_)
            | AnyNodeRef::ExprSubscript(_)
            | AnyNodeRef::ExprStarred(_)
            | AnyNodeRef::ExprName(_)
            | AnyNodeRef::ExprList(_)
            | AnyNodeRef::ExprTuple(_)
            | AnyNodeRef::ExprSlice(_)
            | AnyNodeRef::ExprIpyEscapeCommand(_)
            | AnyNodeRef::FString(_)
            | AnyNodeRef::StringLiteral(_)
            | AnyNodeRef::PatternMatchValue(_)
            | AnyNodeRef::PatternMatchSingleton(_)
            | AnyNodeRef::PatternMatchSequence(_)
            | AnyNodeRef::PatternMatchMapping(_)
            | AnyNodeRef::PatternMatchClass(_)
            | AnyNodeRef::PatternMatchStar(_)
            | AnyNodeRef::PatternMatchAs(_)
            | AnyNodeRef::PatternMatchOr(_)
            | AnyNodeRef::PatternArguments(_)
            | AnyNodeRef::PatternKeyword(_)
            | AnyNodeRef::Comprehension(_)
            | AnyNodeRef::Arguments(_)
            | AnyNodeRef::Parameters(_)
            | AnyNodeRef::Parameter(_)
            | AnyNodeRef::ParameterWithDefault(_)
            | AnyNodeRef::Keyword(_)
            | AnyNodeRef::Alias(_)
            | AnyNodeRef::WithItem(_)
            | AnyNodeRef::TypeParams(_)
            | AnyNodeRef::TypeParamTypeVar(_)
            | AnyNodeRef::TypeParamTypeVarTuple(_)
            | AnyNodeRef::TypeParamParamSpec(_)
            | AnyNodeRef::BytesLiteral(_) => {
                panic!("Range formatting only supports formatting logical lines")
            }
        }
    }
}

/// Computes the level of indentation for `indentation` when using the configured [`IndentStyle`] and [`IndentWidth`].
///
/// Returns `None` if the indentation doesn't conform to the configured [`IndentStyle`] and [`IndentWidth`].
///
/// # Panics
/// If `offset` is outside of `source`.
fn indent_level(offset: TextSize, source: &str, options: &PyFormatOptions) -> Option<u16> {
    let locator = Locator::new(source);
    let indentation = indentation_at_offset(offset, &locator)?;

    let level = match options.indent_style() {
        IndentStyle::Tab => {
            if indentation.chars().all(|c| c == '\t') {
                Some(indentation.len())
            } else {
                None
            }
        }

        IndentStyle::Space => {
            let indent_width = options.indent_width().value() as usize;
            if indentation.chars().all(|c| c == ' ') && indentation.len() % indent_width == 0 {
                Some(indentation.len() / indent_width)
            } else {
                None
            }
        }
    };

    level.map(|level| u16::try_from(level).unwrap_or(u16::MAX))
}
