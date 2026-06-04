//! Render a Markdown string as a tree of Dioxus elements.
//!
//! Pulldown-cmark gives us a flat event stream (Start, End, Text, Image, ...).
//! We collect the events into a small `MdNode` tree, then recurse to emit
//! Dioxus `Element`s. This preserves images, links, code blocks, blockquotes
//! and lists without resorting to `dangerouslySetInnerHTML` or an iframe.

use dioxus::prelude::*;
use pulldown_cmark::{Event, HeadingLevel, Parser, Tag, TagEnd};

/// One block or inline in the parsed markdown document.
#[derive(Debug, Clone)]
enum MdNode {
    Block(Vec<MdNode>),
    Paragraph(Vec<MdNode>),
    Heading {
        level: u32,
        children: Vec<MdNode>,
    },
    List(Vec<MdNode>),
    ListItem(Vec<MdNode>),
    BlockQuote(Vec<MdNode>),
    CodeBlock(String),
    #[allow(dead_code)]
    Inline(Vec<MdNode>),
    Text(String),
    SoftBreak,
    HardBreak,
    Em(Vec<MdNode>),
    Strong(Vec<MdNode>),
    Strikethrough(Vec<MdNode>),
    Code(String),
    Link {
        href: String,
        children: Vec<MdNode>,
    },
    Image {
        src: String,
        alt: String,
    },
    Html(String),
}

/// Build a flat list of root-level nodes from a markdown string.
fn parse(md: &str) -> Vec<MdNode> {
    let parser = Parser::new(md);
    let mut stack: Vec<MdNode> = Vec::new();
    let mut roots: Vec<MdNode> = Vec::new();

    for event in parser {
        match event {
            Event::Start(tag) => stack.push(node_from_start(tag)),
            Event::End(tag_end) => {
                let finished = stack.pop().unwrap_or(MdNode::Block(vec![]));
                let finished = match tag_end {
                    TagEnd::Image | TagEnd::CodeBlock => finished, // no children possible
                    _ => finished,
                };
                push_to(stack.last_mut(), finished, &mut roots);
            }
            Event::Text(s) => push_to(stack.last_mut(), MdNode::Text(s.to_string()), &mut roots),
            Event::Code(s) => push_to(stack.last_mut(), MdNode::Code(s.to_string()), &mut roots),
            Event::SoftBreak => push_to(stack.last_mut(), MdNode::SoftBreak, &mut roots),
            Event::HardBreak => push_to(stack.last_mut(), MdNode::HardBreak, &mut roots),
            Event::Html(s) => push_to(stack.last_mut(), MdNode::Html(s.to_string()), &mut roots),
            Event::InlineHtml(s) => {
                push_to(stack.last_mut(), MdNode::Html(s.to_string()), &mut roots)
            }
            Event::FootnoteReference(s) => {
                push_to(stack.last_mut(), MdNode::Text(format!("[{s}]")), &mut roots)
            }
            Event::Rule => push_to(stack.last_mut(), MdNode::Html("<hr />".into()), &mut roots),
            Event::TaskListMarker(_) => {}
            Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }

    // Anything left on the stack (malformed input) goes to the roots.
    roots.extend(stack);
    roots
}

fn node_from_start(tag: Tag<'_>) -> MdNode {
    match tag {
        Tag::Paragraph => MdNode::Paragraph(vec![]),
        Tag::Heading { level, .. } => MdNode::Heading {
            level: heading_to_u32(level),
            children: vec![],
        },
        Tag::BlockQuote(_) => MdNode::BlockQuote(vec![]),
        Tag::CodeBlock(_) => MdNode::CodeBlock(String::new()),
        Tag::List(_) => MdNode::List(vec![]),
        Tag::Item => MdNode::ListItem(vec![]),
        Tag::Emphasis => MdNode::Em(vec![]),
        Tag::Strong => MdNode::Strong(vec![]),
        Tag::Strikethrough => MdNode::Strikethrough(vec![]),
        Tag::Link { dest_url, .. } => MdNode::Link {
            href: dest_url.to_string(),
            children: vec![],
        },
        Tag::Image {
            dest_url, title, ..
        } => MdNode::Image {
            src: dest_url.to_string(),
            alt: title.to_string(),
        },
        Tag::HtmlBlock => MdNode::Html(String::new()),
        Tag::FootnoteDefinition(_) => MdNode::Block(vec![]),
        Tag::DefinitionList => MdNode::Block(vec![]),
        Tag::DefinitionListTitle => MdNode::Block(vec![]),
        Tag::DefinitionListDefinition => MdNode::Block(vec![]),
        Tag::Table(_) => MdNode::Block(vec![]),
        Tag::TableHead => MdNode::Block(vec![]),
        Tag::TableRow => MdNode::Block(vec![]),
        Tag::TableCell => MdNode::Block(vec![]),
        Tag::MetadataBlock(_) => MdNode::Block(vec![]),
        Tag::Superscript => MdNode::Em(vec![]),
        Tag::Subscript => MdNode::Em(vec![]),
    }
}

fn heading_to_u32(level: HeadingLevel) -> u32 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

fn push_to(parent: Option<&mut MdNode>, child: MdNode, roots: &mut Vec<MdNode>) {
    match parent {
        Some(node) => match node {
            MdNode::Paragraph(c)
            | MdNode::Em(c)
            | MdNode::Strong(c)
            | MdNode::Strikethrough(c)
            | MdNode::Link { children: c, .. }
            | MdNode::Heading { children: c, .. }
            | MdNode::List(c)
            | MdNode::ListItem(c)
            | MdNode::BlockQuote(c)
            | MdNode::Block(c)
            | MdNode::Inline(c) => c.push(child),
            MdNode::CodeBlock(s) => {
                if let MdNode::Text(t) | MdNode::Code(t) = child {
                    s.push_str(&t);
                    s.push('\n');
                }
            }
            MdNode::Image { alt, .. } => {
                if let MdNode::Text(t) = child {
                    alt.push_str(&t);
                }
            }
            MdNode::Html(s) => {
                if let MdNode::Text(t) | MdNode::Code(t) = child {
                    s.push_str(&t);
                }
            }
            MdNode::Text(_) | MdNode::SoftBreak | MdNode::HardBreak | MdNode::Code(_) => {}
        },
        None => roots.push(child),
    }
}

/// Render a markdown string to a Dioxus `Element`.
pub fn render_markdown(md: &str) -> Element {
    let nodes = parse(md);
    rsx! {
        div { class: "markdown-body",
            for node in nodes {
                {render_node(&node)}
            }
        }
    }
}

fn render_nodes(nodes: &[MdNode]) -> Element {
    rsx! {
        for n in nodes {
            {render_node(n)}
        }
    }
}

fn render_node(node: &MdNode) -> Element {
    match node {
        MdNode::Block(c) => rsx! { {render_nodes(c)} },
        MdNode::Paragraph(c) => rsx! {
            p { class: "md-p", {render_nodes(c)} }
        },
        MdNode::Heading { level, children } => {
            let cls = format!("md-h{level}");
            match level {
                1 => rsx! { h1 { class: "{cls}", {render_nodes(children)} } },
                2 => rsx! { h2 { class: "{cls}", {render_nodes(children)} } },
                3 => rsx! { h3 { class: "{cls}", {render_nodes(children)} } },
                4 => rsx! { h4 { class: "{cls}", {render_nodes(children)} } },
                5 => rsx! { h5 { class: "{cls}", {render_nodes(children)} } },
                _ => rsx! { h6 { class: "{cls}", {render_nodes(children)} } },
            }
        }
        MdNode::List(items) => rsx! {
            ul { class: "md-ul",
                for item in items {
                    {render_node(item)}
                }
            }
        },
        MdNode::ListItem(c) => rsx! {
            li { class: "md-li", {render_nodes(c)} }
        },
        MdNode::BlockQuote(c) => rsx! {
            blockquote { class: "md-quote", {render_nodes(c)} }
        },
        MdNode::CodeBlock(s) => rsx! {
            pre { class: "md-pre",
                code { "{s}" }
            }
        },
        MdNode::Inline(c) => rsx! { span { {render_nodes(c)} } },
        MdNode::Text(s) => rsx! { "{s}" },
        MdNode::SoftBreak => rsx! { " " },
        MdNode::HardBreak => rsx! { br {} },
        MdNode::Em(c) => rsx! { em { {render_nodes(c)} } },
        MdNode::Strong(c) => rsx! { strong { {render_nodes(c)} } },
        MdNode::Strikethrough(c) => rsx! { s { {render_nodes(c)} } },
        MdNode::Code(s) => rsx! { code { class: "md-code", "{s}" } },
        MdNode::Link { href, children } => rsx! {
            a { class: "md-link", href: "{href}", target: "_blank", rel: "noopener noreferrer",
                {render_nodes(children)}
            }
        },
        MdNode::Image { src, alt } => rsx! {
            img { class: "md-img", src: "{src}", alt: "{alt}", loading: "lazy" }
        },
        MdNode::Html(s) => rsx! { span { class: "md-html", "{s}" } },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_node(md: &str) -> MdNode {
        let nodes = parse(md);
        assert_eq!(nodes.len(), 1, "expected one root node, got {:?}", nodes);
        nodes.into_iter().next().unwrap()
    }

    #[test]
    fn parses_paragraph_with_emphasis() {
        let node = first_node("Hello *world*.");
        match node {
            MdNode::Paragraph(children) => {
                assert!(matches!(children[0], MdNode::Text(_)));
                assert!(matches!(children[1], MdNode::Em(_)));
            }
            other => panic!("expected Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn parses_image() {
        let node = first_node("![a cat](https://x/cat.png)");
        if let MdNode::Paragraph(children) = node {
            assert!(
                matches!(&children[0], MdNode::Image { src, alt } if src == "https://x/cat.png" && alt == "a cat")
            );
        } else {
            panic!("expected Paragraph");
        }
    }

    #[test]
    fn parses_heading_levels() {
        let node = first_node("## Title");
        match node {
            MdNode::Heading { level, .. } => assert_eq!(level, 2),
            other => panic!("expected Heading, got {other:?}"),
        }
    }

    #[test]
    fn parses_code_block() {
        let node = first_node("```\nlet x = 1;\n```");
        match node {
            MdNode::CodeBlock(s) => assert!(s.contains("let x = 1;")),
            other => panic!("expected CodeBlock, got {other:?}"),
        }
    }

    #[test]
    fn parses_list_items() {
        let nodes = parse("- a\n- b\n- c");
        assert_eq!(nodes.len(), 1, "list collapses to a single MdNode::List");
        if let MdNode::List(items) = &nodes[0] {
            assert_eq!(items.len(), 3);
        } else {
            panic!("expected List");
        }
    }
}
