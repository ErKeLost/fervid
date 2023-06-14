use fervid_core::{ElementNode, HtmlAttribute, Node, StartingTag, VDirective};
use smallvec::SmallVec;
use swc_core::{
    common::{BytePos, Span, SyntaxContext, DUMMY_SP},
    ecma::ast::{
        BinExpr, BinaryOp, CallExpr, Callee, Expr, ExprOrSpread, Ident, Lit, Number, ParenExpr,
        SeqExpr,
    },
};

use crate::{attributes::DirectivesToProcess, context::CodegenContext, imports::VueImports, utils};

type TextNodesConcatenationVec = SmallVec<[Expr; 3]>;

impl CodegenContext {
    pub fn generate_node(&mut self, node: &Node, wrap_in_block: bool) -> Expr {
        match node {
            Node::TextNode(contents) => self.generate_text_node(contents, DUMMY_SP),
            Node::DynamicExpression {
                value,
                template_scope,
            } => self.generate_dynamic_expression(value, *template_scope, DUMMY_SP),

            Node::ElementNode(element_node) => {
                if self.is_component(&element_node.starting_tag) {
                    self.generate_component_vnode(element_node, wrap_in_block)
                } else {
                    self.generate_element_vnode(element_node, wrap_in_block)
                }
                // todo builtins as well
            }

            Node::CommentNode(comment) => self.generate_comment_vnode(comment, DUMMY_SP),
        }
    }

    /// Generates a sequence of nodes taken from an iterator.
    ///
    /// - `total_nodes` is a hint of how many nodes are in the original Vec,
    ///   it will be used when deciding whether to inline or not;
    /// - `allow_inlining` is whether all text nodes can be merged
    ///   without a surrounding `createTextVNode` call.
    ///
    /// Returns `true` if all the nodes were inlined successfully
    pub fn generate_node_sequence<'n>(
        &mut self,
        iter: &mut impl Iterator<Item = &'n Node<'n>>,
        out: &mut Vec<Expr>,
        total_nodes: usize,
        allow_inlining: bool,
    ) -> bool {
        // Buffer for concatenating text nodes. Will be reused multiple times
        let mut text_nodes = TextNodesConcatenationVec::new();
        let mut text_nodes_span = [BytePos(0), BytePos(0)];

        macro_rules! maybe_concatenate_text_nodes {
            () => {
                if text_nodes.len() != 0 {
                    // Ignore `createTextVNode` if allowed and all the nodes are text nodes
                    let should_inline = allow_inlining && text_nodes.len() == total_nodes;
                    let concatenation = self.concatenate_text_nodes(
                        &mut text_nodes,
                        should_inline,
                        Span {
                            lo: text_nodes_span[0],
                            hi: text_nodes_span[1],
                            ctxt: SyntaxContext::empty(),
                        },
                    );
                    out.push(concatenation);

                    // Reset text nodes
                    text_nodes.clear();
                    text_nodes_span[0] = BytePos(0);
                    text_nodes_span[1] = BytePos(0);

                    // Return whether was inlined or not
                    should_inline
                } else {
                    false
                }
            };
        }

        while let Some(node) = iter.next() {
            let generated = self.generate_node(node, false);
            let is_text_node = matches!(node, Node::TextNode(_) | Node::DynamicExpression { .. });

            if is_text_node {
                text_nodes.push(generated);

                // Save span
                // TODO real spans
                if text_nodes_span[0].is_dummy() {
                    text_nodes_span[0] = BytePos(0);
                }
                text_nodes_span[1] = BytePos(0);
            } else {
                // Process the text nodes from before
                maybe_concatenate_text_nodes!();

                out.push(generated);
            }
        }

        // Process the remaining text nodes.
        maybe_concatenate_text_nodes!()
    }

    pub fn is_component(self: &Self, starting_tag: &StartingTag) -> bool {
        // todo use analyzed components? (fields of `components: {}`)

        let tag_name = starting_tag.tag_name;

        let is_html_tag = utils::is_html_tag(tag_name);
        // if is_html_tag {
        //     return false;
        // }

        !is_html_tag

        // Check with isCustomElement
        // let is_custom_element = match &self.is_custom_element {
        //     IsCustomElementParam::String(string) => tag_name == *string,
        //     IsCustomElementParam::Regex(re) => re.is_match(tag_name),
        //     IsCustomElementParam::None => false,
        // };

        // !is_custom_element
    }

    /// Wraps the expression in openBlock construction,
    /// e.g. `(openBlock(), expr)`
    pub fn wrap_in_open_block(&mut self, expr: Expr, span: Span) -> Expr {
        Expr::Paren(ParenExpr {
            span,
            expr: Box::new(Expr::Seq(SeqExpr {
                span,
                exprs: vec![
                    // openBlock()
                    Box::new(Expr::Call(CallExpr {
                        span,
                        callee: Callee::Expr(Box::new(Expr::Ident(Ident {
                            span,
                            sym: self.get_and_add_import_ident(VueImports::OpenBlock),
                            optional: false,
                        }))),
                        args: Vec::new(),
                        type_args: None,
                    })),
                    Box::new(expr),
                ],
            })),
        })
    }

    pub fn generate_remaining_directives(
        &mut self,
        expr: &mut Expr,
        remaining_directives: &DirectivesToProcess,
    ) {
        todo!()
    }

    /// Special case: `<template>` with `v-if`/`v-else-if`/`v-else`/`v-for`
    #[inline]
    pub fn should_generate_fragment(&self, element_node: &ElementNode) -> bool {
        element_node.starting_tag.tag_name == "template"
            && element_node
                .starting_tag
                .attributes
                .iter()
                .any(|attr| match attr {
                    HtmlAttribute::VDirective(
                        VDirective::If(_)
                        | VDirective::ElseIf(_)
                        | VDirective::Else
                        | VDirective::For(_),
                    ) => true,
                    _ => false,
                })
    }

    fn concatenate_text_nodes(
        &mut self,
        text_nodes_concatenation: &mut TextNodesConcatenationVec,
        inline: bool,
        span: Span,
    ) -> Expr {
        let text_nodes_len = text_nodes_concatenation.len();

        let concatenation: Expr = join_exprs_to_concatenation(text_nodes_concatenation, span);

        // In `inline` mode, just return concatenation as-is
        if inline {
            return concatenation;
        }

        // TODO Really, this depends on variable usage from `transform_scoped`
        // If inlining is off, surround with `createTextVNode()`
        // When concatenation is longer than 1 element, that means
        // there were some `Node::DynamicExpression`s
        let has_patch_flag = text_nodes_len > 1;

        // `concatenation`
        let mut create_text_vnode_args = Vec::with_capacity(if has_patch_flag { 2 } else { 1 });
        create_text_vnode_args.push(ExprOrSpread {
            spread: None,
            expr: Box::new(concatenation),
        });

        // Add patch flag
        // `concatenation, 1`
        if has_patch_flag {
            create_text_vnode_args.push(ExprOrSpread {
                spread: None,
                expr: Box::new(Expr::Lit(Lit::Num(Number {
                    span,
                    value: 1.0,
                    raw: None,
                }))),
            })
        }

        // createTextVNode(/* args */)
        Expr::Call(CallExpr {
            span,
            callee: Callee::Expr(Box::new(Expr::Ident(Ident {
                span,
                sym: self.get_and_add_import_ident(VueImports::CreateTextVNode),
                optional: false,
            }))),
            args: create_text_vnode_args,
            type_args: None,
        })
    }
}

/// Concatenate multiple expressions, e.g. `expr1 + expr2 + expr3`
fn join_exprs_to_concatenation(exprs: &mut TextNodesConcatenationVec, span: Span) -> Expr {
    let mut drain = exprs.drain(..);

    let mut expr = drain.next().expect("TextNodesConcatenationVec was empty");

    for item in drain {
        expr = Expr::Bin(BinExpr {
            span,
            op: BinaryOp::Add,
            left: Box::new(expr),
            right: Box::new(item),
        })
    }

    expr
}