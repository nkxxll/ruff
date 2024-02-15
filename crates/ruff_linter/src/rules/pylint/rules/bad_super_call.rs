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
    let cl_args: ast::Arguments;
    match arguments {
        Some(args) => {
            cl_args = args;
        }
        None => {}
    }
    let res: Option<TextRange>;
    let methods = get_methods(body);
    for method in methods {
        let super_call = get_super_call(method);
        match super_call {
            Some(sc) => {
                let args = sc.arguments;
                match args {
                    Some(args) => {
                        res = get_bad_super_call_range(args, cl_args);
                        match res {
                            Some(r) => {
                                return Some(sc);
                            }
                            None => {}
                        }
                    }
                    None => {}
                }
            }
            None => {}
        }
    }
}

fn get_methods(body: &[Stmt]) -> Vec<Vec<Stmt>> {
    let mut res = Vec::new();
    for statement in body {
        match statement {
            Stmt::FunctionDef(ast::StmtFunctionDef { body, .. }) => {
                res.push(body);
            }
            _ => {}
        }
    }
    res
}

fn get_super_call(init: ast::StmtFunctionDef) -> Option<ast::ExprCall> {
    for statement in init.body {
        match statement {
            Stmt::Expr(ast::ExprCall {
                range,
                func,
                arguments,
                ..
            }) => {
                if func.name_expr() == "super" {
                    return Some(statement);
                }
            }
            _ => {}
        }
    }
    None
}

/// Gets the range of the bad super call if the super call is acctually bad
///
/// For that the function tests the the first argument of the super call is the same as the first
/// argument of the class statement. For a real bad super call if the first arguments do not match
/// the super call has to have self as the first argument if the first arguments match the super
/// call has self right behind the first matching arguments.
///
/// * `super_args`: arguments of the super call
/// * `class_args`: arguments of the class statement
fn get_bad_super_call_range(
    super_args: ast::Arguments,
    class_args: ast::Arguments,
) -> Option<TextRange> {
    let super_args = super_args.args.iter().peekable();
    let class_args = class_args.args.iter().peekable();
    // if the super call has no arguments the super call is not bad
    while (super_args.peek().is_some()) {
        let super_arg = super_args.next().unwrap();
        // you can have a bad super call if the super call has more arguments than the class
        let class_arg = class_args.next();
        match class_args {
            Some(ca) => {
                // if we have arguments in the class statement we can have a bad super call if the
                // arguments do not match
                if super_arg != ca {
                    return Some(super_arg.range());
                }
            }
            None => {
                // if the class statement has no arguments the super call is bad if self is not
                // the first argument if self is the first argument we have an other error
                if super_arg.name_expr() != "self" {
                    return Some(super_arg.range());
                }
            }
        }
    }
}
