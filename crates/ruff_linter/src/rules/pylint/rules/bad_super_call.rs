use ast::Arguments;
use ruff_diagnostics::{Diagnostic, Violation};
use ruff_macros::{derive_message_formats, violation};

use ruff_python_ast::{self as ast, Expr, Stmt};
use ruff_text_size::{Ranged, TextRange};

use crate::checkers::ast::Checker;

#[violation]
pub struct BadSuperCall;

impl Violation for BadSuperCall {
    #[derive_message_formats]
    fn message(&self) -> String {
        format!("Bad first argument given to super()")
    }
}

pub(crate) fn bad_super_call(
    checker: &mut Checker,
    ast::StmtClassDef {
        arguments, body, ..
    }: &ast::StmtClassDef,
) {
    let bad_super = get_bad_super(arguments, body);
    match bad_super {
        Some(bs) => {
            checker
                .diagnostics
                .push(Diagnostic::new(BadSuperCall, bs.range()));
        }
        None => {}
    }
}

fn get_bad_super(arguments: &Option<Box<Arguments>>, body: &[Stmt]) -> Option<ast::Stmt> {
    // if args then save the args for later
    match arguments {
        Some(args) => {}
        None => (),
    }
    let res: Option<TextRange>;
    for statement in body {
        match statement {
            Stmt::FunctionDef(ast::StmtFunctionDef { name, body, .. }) => {
                if name == "__init__" {
                    for stmt in body {
                        match stmt {
                            Stmt::Expr(ast::StmtExpr { range, value, .. }) => {
                                if value.name_expr() == Some("super") && value.call_expr() == Some {
                                    let call_expression = value.as_call_expr();
                                    match call_expression {
                                        Some(ce) => {
                                            // can't do bad super call with no arguments
                                            if ce.arguments.len() == 0 {
                                                continue;
                                            // } else if ce.arguments == cl_arguments.push {
                                            //     ()
                                            }
                                            res = range
                                        }
                                        None => {}
                                    }
                                },
                            },
                            _ => {}
                        }
                    }
                }
            }
            _ => {}
        }
    }
    res
}
