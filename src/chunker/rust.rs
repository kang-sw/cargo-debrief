use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::chunk::{Chunk, ChunkKind, ChunkMetadata, ChunkType, Visibility};

use super::Chunker;

/// Methods/functions with this many lines or fewer are inlined into their
/// parent overview chunk instead of getting a standalone function chunk.
const MIN_METHOD_CHUNK_LINES: usize = 5;

pub struct RustChunker;

impl Chunker for RustChunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .context("failed to load tree-sitter Rust grammar")?;

        let tree = parser
            .parse(source, None)
            .context("tree-sitter parse returned None")?;

        let module_path = derive_module_path(file_path);
        let root = tree.root_node();

        let mut collector = ChunkCollector {
            source: source.as_bytes(),
            source_str: source,
            file_path,
            module_path: &module_path,
            types: HashMap::new(),
            free_functions: Vec::new(),
        };

        collector.collect_top_level(root, &module_path);
        Ok(collector.into_chunks())
    }
}

/// Derive a Rust module path from a file path.
///
/// `src/lib.rs` → `crate`, `src/foo.rs` → `crate::foo`,
/// `src/net/pool.rs` → `crate::net::pool`.
fn derive_module_path(file_path: &Path) -> String {
    let path_str = file_path.to_string_lossy();
    let path_str = path_str.as_ref();

    // Strip leading `src/` if present.
    let stem = path_str.strip_prefix("src/").unwrap_or(path_str);

    match stem {
        "lib.rs" | "main.rs" => "crate".to_string(),
        other => {
            let without_ext = other.strip_suffix(".rs").unwrap_or(other);
            // `foo/mod.rs` → `foo`
            let without_mod = without_ext.strip_suffix("/mod").unwrap_or(without_ext);
            format!("crate::{}", without_mod.replace('/', "::"))
        }
    }
}

/// Extract the text of a node from source bytes.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Parse visibility from a node's `visibility_modifier` child.
fn parse_visibility(node: Node<'_>, source: &[u8]) -> Visibility {
    // tree-sitter-rust exposes visibility as a child of kind
    // `visibility_modifier`, not as a named field.
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "visibility_modifier" {
            let text = node_text(child, source);
            return parse_visibility_text(text);
        }
    }
    Visibility::Private
}

fn parse_visibility_text(text: &str) -> Visibility {
    let trimmed = text.trim();
    if trimmed == "pub" {
        Visibility::Pub
    } else if trimmed.starts_with("pub") && trimmed.contains("crate") {
        Visibility::PubCrate
    } else if trimmed.starts_with("pub") && trimmed.contains("super") {
        Visibility::PubSuper
    } else {
        Visibility::Private
    }
}

/// Extract the first doc-comment line from lines preceding a node.
fn extract_doc_comment(source: &str, start_row: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    // Walk backwards from the line before the node.
    let mut row = start_row;
    while row > 0 {
        row -= 1;
        let line = lines.get(row)?.trim();
        if line.starts_with("///") {
            let comment = line.strip_prefix("///").unwrap_or("").trim();
            if !comment.is_empty() {
                return Some(comment.to_string());
            }
        } else if line.starts_with("//!") || line.starts_with("#[") || line.is_empty() {
            // Skip attributes, inner doc comments, and blank lines.
            continue;
        } else {
            break;
        }
    }
    None
}

/// Strip the body `{ ... }` from a function signature, keeping everything
/// up to the return type, and append `;`.
fn signature_without_body(source: &str, node: Node<'_>) -> String {
    // Find the `block` child (the function body).
    let mut cursor = node.walk();
    let body_start = node
        .children(&mut cursor)
        .find(|c| c.kind() == "block" || c.kind() == "declaration_list")
        .map(|c| c.start_byte());

    let node_src = &source[node.start_byte()..node.end_byte()];
    match body_start {
        Some(body) => {
            let relative = body - node.start_byte();
            let before_body = node_src[..relative].trim_end();
            format!("{before_body};")
        }
        None => format!("{};", node_src.trim()),
    }
}

/// Extract the first line of a function (up to `{`) as signature.
fn extract_signature(source: &str, node: Node<'_>) -> String {
    let text = &source[node.start_byte()..node.end_byte()];
    match text.find('{') {
        Some(pos) => text[..pos].trim().to_string(),
        None => {
            // No body — take first line.
            text.lines().next().unwrap_or("").trim().to_string()
        }
    }
}

/// Extract the first line of a type definition as signature.
fn extract_type_signature(source: &str, node: Node<'_>) -> String {
    let text = &source[node.start_byte()..node.end_byte()];
    // Take up to first `{` or first line.
    match text.find('{') {
        Some(pos) => text[..pos].trim().to_string(),
        None => text.lines().next().unwrap_or("").trim().to_string(),
    }
}

// ---------------------------------------------------------------------------
// Intermediate data structures for two-pass collection
// ---------------------------------------------------------------------------

struct TypeOverview<'a> {
    /// The definition node (struct/enum/trait). `None` for orphan impls.
    definition: Option<Node<'a>>,
    /// Collected impl blocks: (impl header text, vec of method signature nodes).
    impls: Vec<ImplBlock<'a>>,
    kind: ChunkKind,
    visibility: Visibility,
    doc_comment: Option<String>,
}

struct ImplBlock<'a> {
    /// Full impl header text, e.g. `impl Display for Foo`.
    header: String,
    /// Method nodes inside this impl.
    methods: Vec<Node<'a>>,
    /// The impl node itself (for line range).
    node: Node<'a>,
}

struct FreeFunction<'a> {
    node: Node<'a>,
    module_path: String,
}

struct ChunkCollector<'a> {
    source: &'a [u8],
    source_str: &'a str,
    file_path: &'a Path,
    module_path: &'a str,
    types: HashMap<String, TypeOverview<'a>>,
    free_functions: Vec<FreeFunction<'a>>,
}

impl<'a> ChunkCollector<'a> {
    /// First pass: collect type definitions, impl blocks, and free functions.
    fn collect_top_level(&mut self, container: Node<'a>, current_module: &str) {
        let mut cursor = container.walk();
        for child in container.children(&mut cursor) {
            match child.kind() {
                "struct_item" => self.collect_type(child, ChunkKind::Struct),
                "enum_item" => self.collect_type(child, ChunkKind::Enum),
                "trait_item" => self.collect_type(child, ChunkKind::Trait),
                "impl_item" => self.collect_impl(child),
                "function_item" => self.collect_free_function(child, current_module),
                "mod_item" => self.collect_mod(child, current_module),
                _ => {}
            }
        }
    }

    fn collect_type(&mut self, node: Node<'a>, kind: ChunkKind) {
        let name = self.type_name_from_node(node);
        let visibility = parse_visibility(node, self.source);
        let doc_comment = extract_doc_comment(self.source_str, node.start_position().row);

        let entry = self.types.entry(name).or_insert_with(|| TypeOverview {
            definition: None,
            impls: Vec::new(),
            kind,
            visibility,
            doc_comment: None,
        });
        entry.definition = Some(node);
        entry.kind = kind;
        entry.visibility = visibility;
        entry.doc_comment = doc_comment;
    }

    fn collect_impl(&mut self, node: Node<'a>) {
        let impl_type_name = self.impl_type_name(node);
        let header = self.impl_header_text(node);
        let methods = self.impl_methods(node);

        let entry = self
            .types
            .entry(impl_type_name.clone())
            .or_insert_with(|| TypeOverview {
                definition: None,
                impls: Vec::new(),
                kind: ChunkKind::Impl,
                visibility: Visibility::Private,
                doc_comment: None,
            });

        entry.impls.push(ImplBlock {
            header,
            methods,
            node,
        });
    }

    fn collect_free_function(&mut self, node: Node<'a>, current_module: &str) {
        self.free_functions.push(FreeFunction {
            node,
            module_path: current_module.to_string(),
        });
    }

    fn collect_mod(&mut self, node: Node<'a>, parent_module: &str) {
        let name_node = node.child_by_field_name("name");
        let mod_name = name_node.map(|n| node_text(n, self.source)).unwrap_or("_");
        let child_module = format!("{parent_module}::{mod_name}");

        // Only recurse into inline modules (those with a `declaration_list` body).
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "declaration_list" {
                self.collect_inline_module_body(child, &child_module);
                break;
            }
        }
    }

    fn collect_inline_module_body(&mut self, body: Node<'a>, module_path: &str) {
        let mut cursor = body.walk();
        for child in body.children(&mut cursor) {
            match child.kind() {
                "struct_item" => self.collect_type(child, ChunkKind::Struct),
                "enum_item" => self.collect_type(child, ChunkKind::Enum),
                "trait_item" => self.collect_type(child, ChunkKind::Trait),
                "impl_item" => self.collect_impl(child),
                "function_item" => self.collect_free_function(child, module_path),
                "mod_item" => self.collect_mod(child, module_path),
                _ => {}
            }
        }
    }

    /// Extract type name from a struct/enum/trait node.
    fn type_name_from_node(&self, node: Node<'a>) -> String {
        node.child_by_field_name("name")
            .map(|n| node_text(n, self.source).to_string())
            .unwrap_or_else(|| "_".to_string())
    }

    /// Extract the type name from an impl node (the type being implemented).
    fn impl_type_name(&self, node: Node<'a>) -> String {
        // The impl's type is the `type` field child. For trait impls, it's
        // the type after `for`. tree-sitter-rust names it "type" in both cases.
        if let Some(type_node) = node.child_by_field_name("type") {
            // For generic types like `Foo<T>`, just use the base name.
            return self.base_type_name(type_node);
        }
        // Fallback: try the `trait` field for trait impls (shouldn't normally
        // need this path).
        "_".to_string()
    }

    /// Extract just the base name from a type node (strips generics).
    fn base_type_name(&self, type_node: Node<'_>) -> String {
        let text = node_text(type_node, self.source);
        // Strip generics: `Foo<T>` → `Foo`
        match text.find('<') {
            Some(pos) => text[..pos].trim().to_string(),
            None => text.trim().to_string(),
        }
    }

    /// Build the impl header: `impl Foo` or `impl Display for Foo`.
    fn impl_header_text(&self, node: Node<'a>) -> String {
        let text = &self.source_str[node.start_byte()..node.end_byte()];
        // Take everything up to the first `{`.
        match text.find('{') {
            Some(pos) => text[..pos].trim().to_string(),
            None => text.lines().next().unwrap_or("").trim().to_string(),
        }
    }

    /// Collect method nodes from an impl block's declaration_list.
    fn impl_methods(&self, node: Node<'a>) -> Vec<Node<'a>> {
        let mut methods = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "declaration_list" {
                let mut inner_cursor = child.walk();
                for item in child.children(&mut inner_cursor) {
                    if item.kind() == "function_item" {
                        methods.push(item);
                    }
                }
            }
        }
        methods
    }

    /// Second pass: generate Chunk values from collected data.
    fn into_chunks(mut self) -> Vec<Chunk> {
        let mut chunks = Vec::new();

        // Take ownership of collected data while keeping borrowed fields accessible.
        let mut types: Vec<_> = std::mem::take(&mut self.types).into_iter().collect();
        types.sort_by(|a, b| a.0.cmp(&b.0));
        let free_functions = std::mem::take(&mut self.free_functions);

        for (type_name, overview) in &types {
            // Generate overview chunk.
            chunks.push(self.build_overview_chunk(type_name, overview));

            // Generate function chunks only for large methods (small ones are inlined in overview).
            for impl_block in &overview.impls {
                for method in &impl_block.methods {
                    let method_lines = method.end_position().row - method.start_position().row + 1;
                    if method_lines > MIN_METHOD_CHUNK_LINES {
                        chunks.push(self.build_method_chunk(
                            type_name,
                            &impl_block.header,
                            *method,
                        ));
                    }
                }
            }
        }

        // Module overview chunk aggregating free functions.
        if !free_functions.is_empty() {
            chunks.push(self.build_module_overview_chunk(&free_functions));

            // Only generate separate chunks for large free functions.
            for free_fn in &free_functions {
                let fn_lines =
                    free_fn.node.end_position().row - free_fn.node.start_position().row + 1;
                if fn_lines > MIN_METHOD_CHUNK_LINES {
                    chunks.push(self.build_free_function_chunk(free_fn));
                }
            }
        }

        chunks
    }

    fn build_overview_chunk(&self, type_name: &str, overview: &TypeOverview<'_>) -> Chunk {
        let mut display_parts = Vec::new();

        // Type definition text.
        if let Some(def_node) = overview.definition {
            let def_text = node_text(def_node, self.source);
            display_parts.push(def_text.to_string());
        }

        // Impl block signatures (small methods inlined, large ones as signature-only).
        for impl_block in &overview.impls {
            let mut impl_text = format!("{} {{", impl_block.header);
            for method in &impl_block.methods {
                let method_lines = method.end_position().row - method.start_position().row + 1;
                if method_lines <= MIN_METHOD_CHUNK_LINES {
                    let full_text = node_text(*method, self.source);
                    impl_text.push_str(&format!("\n    {full_text}"));
                } else {
                    let sig = signature_without_body(self.source_str, *method);
                    impl_text.push_str(&format!("\n    {sig}"));
                }
            }
            impl_text.push_str("\n}");
            display_parts.push(impl_text);
        }

        let display_text = display_parts.join("\n\n");

        // Line range: span from definition (if any) through all impl blocks.
        let start_row = overview
            .definition
            .map(|n| n.start_position().row)
            .into_iter()
            .chain(overview.impls.iter().map(|ib| ib.node.start_position().row))
            .min()
            .unwrap_or(0);
        let end_row = overview
            .definition
            .map(|n| n.end_position().row)
            .into_iter()
            .chain(overview.impls.iter().map(|ib| ib.node.end_position().row))
            .max()
            .unwrap_or(0);

        let signature = overview
            .definition
            .map(|n| extract_type_signature(self.source_str, n));

        let file_path_str = self.file_path.to_string_lossy().to_string();

        // Build embedding_text with context.
        let doc_brief = overview
            .doc_comment
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let vis_prefix = visibility_prefix(overview.visibility);
        let embedding_text = format!(
            "// {module} ({file}:{start}..{end})\n\
             // {vis}{type_name}{doc}\n\
             {display}",
            module = self.module_path,
            file = file_path_str,
            start = start_row,
            end = end_row,
            vis = vis_prefix,
            doc = doc_brief,
            display = display_text,
        );

        Chunk {
            display_text,
            embedding_text,
            metadata: ChunkMetadata {
                symbol_name: type_name.to_string(),
                kind: overview.kind,
                chunk_type: ChunkType::Overview,
                parent: None,
                visibility: overview.visibility,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature,
            },
            embedding: None,
        }
    }

    fn build_method_chunk(&self, type_name: &str, impl_header: &str, method: Node<'_>) -> Chunk {
        let method_name = method
            .child_by_field_name("name")
            .map(|n| node_text(n, self.source))
            .unwrap_or("_");

        let method_text = node_text(method, self.source);
        let visibility = parse_visibility(method, self.source);
        let signature = extract_signature(self.source_str, method);

        // Wrap method in impl context for display_text.
        let display_text = format!(
            "{impl_header} {{\n    {body}\n}}",
            body = indent_block(method_text, 4),
        );

        let file_path_str = self.file_path.to_string_lossy().to_string();
        let start_row = method.start_position().row;
        let end_row = method.end_position().row;
        let doc_comment = extract_doc_comment(self.source_str, start_row);
        let doc_brief = doc_comment
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let vis_prefix = visibility_prefix(visibility);

        let symbol_name = format!("{type_name}::{method_name}");

        let embedding_text = format!(
            "// {module} ({file}:{start}..{end})\n\
             // {vis}{symbol}{doc}\n\
             // {impl_header}\n\
             {display}",
            module = self.module_path,
            file = file_path_str,
            start = start_row,
            end = end_row,
            vis = vis_prefix,
            symbol = symbol_name,
            doc = doc_brief,
            display = display_text,
        );

        Chunk {
            display_text,
            embedding_text,
            metadata: ChunkMetadata {
                symbol_name,
                kind: ChunkKind::Function,
                chunk_type: ChunkType::Function,
                parent: Some(type_name.to_string()),
                visibility,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature: Some(signature),
            },
            embedding: None,
        }
    }

    fn build_module_overview_chunk(&self, free_functions: &[FreeFunction<'_>]) -> Chunk {
        let mut display_parts = Vec::new();
        display_parts.push(format!("// {}", self.module_path));

        for free_fn in free_functions {
            let node = free_fn.node;
            let fn_lines = node.end_position().row - node.start_position().row + 1;
            if fn_lines <= MIN_METHOD_CHUNK_LINES {
                display_parts.push(node_text(node, self.source).to_string());
            } else {
                display_parts.push(signature_without_body(self.source_str, node));
            }
        }

        let display_text = display_parts.join("\n\n");

        let start_row = free_functions
            .iter()
            .map(|f| f.node.start_position().row)
            .min()
            .unwrap_or(0);
        let end_row = free_functions
            .iter()
            .map(|f| f.node.end_position().row)
            .max()
            .unwrap_or(0);

        let file_path_str = self.file_path.to_string_lossy().to_string();
        let module_name = self
            .module_path
            .rsplit("::")
            .next()
            .unwrap_or(self.module_path);

        let embedding_text = format!(
            "// {module} ({file}:{start}..{end})\n\
             // Module overview\n\
             {display}",
            module = self.module_path,
            file = file_path_str,
            start = start_row,
            end = end_row,
            display = display_text,
        );

        Chunk {
            display_text,
            embedding_text,
            metadata: ChunkMetadata {
                symbol_name: module_name.to_string(),
                kind: ChunkKind::Module,
                chunk_type: ChunkType::Overview,
                parent: None,
                visibility: Visibility::Pub,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature: None,
            },
            embedding: None,
        }
    }

    fn build_free_function_chunk(&self, free_fn: &FreeFunction<'_>) -> Chunk {
        let node = free_fn.node;
        let fn_name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, self.source))
            .unwrap_or("_");

        let fn_text = node_text(node, self.source);
        let visibility = parse_visibility(node, self.source);
        let signature = extract_signature(self.source_str, node);

        let display_text = format!(
            "// {module}\n{body}",
            module = free_fn.module_path,
            body = fn_text,
        );

        let file_path_str = self.file_path.to_string_lossy().to_string();
        let start_row = node.start_position().row;
        let end_row = node.end_position().row;
        let doc_comment = extract_doc_comment(self.source_str, start_row);
        let doc_brief = doc_comment
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();
        let vis_prefix = visibility_prefix(visibility);

        let embedding_text = format!(
            "// {module} ({file}:{start}..{end})\n\
             // {vis}{fn_name}{doc}\n\
             {display}",
            module = free_fn.module_path,
            file = file_path_str,
            start = start_row,
            end = end_row,
            vis = vis_prefix,
            doc = doc_brief,
            display = display_text,
        );

        Chunk {
            display_text,
            embedding_text,
            metadata: ChunkMetadata {
                symbol_name: fn_name.to_string(),
                kind: ChunkKind::Function,
                chunk_type: ChunkType::Function,
                parent: None,
                visibility,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature: Some(signature),
            },
            embedding: None,
        }
    }
}

/// Format visibility for embedding context comments.
fn visibility_prefix(vis: Visibility) -> &'static str {
    match vis {
        Visibility::Pub => "pub ",
        Visibility::PubCrate => "pub(crate) ",
        Visibility::PubSuper => "pub(super) ",
        Visibility::Private => "",
    }
}

/// Indent a block of text by `n` spaces (first line is not indented since
/// it's placed inline).
fn indent_block(text: &str, _n: usize) -> String {
    // The method text already has its original indentation. We just include
    // it as-is since the impl wrapper provides the context.
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_source(source: &str) -> Vec<Chunk> {
        let chunker = RustChunker;
        chunker
            .chunk(Path::new("src/example.rs"), source)
            .expect("chunking should succeed")
    }

    #[test]
    fn struct_with_impl() {
        let source = r#"
pub struct Foo {
    pub value: i32,
}

impl Foo {
    pub fn new(value: i32) -> Self {
        Self { value }
    }

    pub fn get(&self) -> i32 {
        self.value
    }
}
"#;
        let chunks = chunk_source(source);

        // Small methods (≤5 lines) are inlined into overview, no separate function chunks.
        let overview = chunks
            .iter()
            .find(|c| {
                c.metadata.symbol_name == "Foo" && c.metadata.chunk_type == ChunkType::Overview
            })
            .expect("should have overview chunk");
        assert_eq!(overview.metadata.kind, ChunkKind::Struct);
        assert_eq!(overview.metadata.visibility, Visibility::Pub);

        // Overview should contain the struct def and inlined method bodies.
        assert!(overview.display_text.contains("pub struct Foo"));
        assert!(
            overview
                .display_text
                .contains("pub fn new(value: i32) -> Self"),
            "display_text: {}",
            overview.display_text
        );
        assert!(
            overview.display_text.contains("Self { value }"),
            "small method should be inlined with body"
        );

        // No separate function chunks — both methods are ≤5 lines.
        let fn_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.metadata.chunk_type == ChunkType::Function)
            .collect();
        assert_eq!(
            fn_chunks.len(),
            0,
            "small methods should not produce function chunks"
        );
    }

    #[test]
    fn multiple_impl_blocks() {
        let source = r#"
pub struct Bar;

impl Bar {
    pub fn hello(&self) -> String {
        "hello".to_string()
    }
}

impl std::fmt::Display for Bar {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Bar")
    }
}
"#;
        let chunks = chunk_source(source);

        let overview = chunks
            .iter()
            .find(|c| {
                c.metadata.symbol_name == "Bar" && c.metadata.chunk_type == ChunkType::Overview
            })
            .expect("should have Bar overview");

        // Overview should show both impl blocks.
        assert!(overview.display_text.contains("impl Bar"));
        assert!(
            overview
                .display_text
                .contains("impl std::fmt::Display for Bar"),
            "display_text: {}",
            overview.display_text
        );
        // Small methods are inlined with full body, not signature-only.
        assert!(
            overview
                .display_text
                .contains("pub fn hello(&self) -> String"),
            "display_text: {}",
            overview.display_text
        );
        assert!(overview.display_text.contains("fn fmt("));
    }

    #[test]
    fn trait_definition() {
        let source = r#"
pub trait Greet {
    fn greet(&self) -> String;
    fn name(&self) -> &str;
}
"#;
        let chunks = chunk_source(source);

        let overview = chunks
            .iter()
            .find(|c| c.metadata.symbol_name == "Greet")
            .expect("should have Greet overview");
        assert_eq!(overview.metadata.kind, ChunkKind::Trait);
        assert_eq!(overview.metadata.chunk_type, ChunkType::Overview);
        assert_eq!(overview.metadata.visibility, Visibility::Pub);
        assert!(overview.display_text.contains("pub trait Greet"));
    }

    #[test]
    fn free_function() {
        let source = r#"
/// Does something useful.
pub fn do_stuff(x: i32) -> i32 {
    x + 1
}
"#;
        let chunks = chunk_source(source);

        // Small free function (≤5 lines) is inlined into module overview only.
        assert_eq!(chunks.len(), 1);
        let chunk = &chunks[0];
        assert_eq!(chunk.metadata.symbol_name, "example");
        assert_eq!(chunk.metadata.kind, ChunkKind::Module);
        assert_eq!(chunk.metadata.chunk_type, ChunkType::Overview);
        assert!(chunk.metadata.parent.is_none());
        assert!(chunk.display_text.contains("crate::example"));
        assert!(chunk.display_text.contains("pub fn do_stuff"));

        // Embedding text should include module context.
        assert!(
            chunk.embedding_text.contains("Module overview"),
            "embedding: {}",
            chunk.embedding_text
        );
    }

    #[test]
    fn orphan_impl() {
        let source = r#"
impl ExternalType {
    pub fn adapt(&self) -> String {
        "adapted".to_string()
    }
}
"#;
        let chunks = chunk_source(source);

        let overview = chunks
            .iter()
            .find(|c| c.metadata.chunk_type == ChunkType::Overview)
            .expect("should have overview for orphan impl");
        assert_eq!(overview.metadata.symbol_name, "ExternalType");
        // No definition, so kind should be Impl.
        assert_eq!(overview.metadata.kind, ChunkKind::Impl);

        // Small method (≤5 lines) is inlined into overview, no separate function chunk.
        assert!(
            overview
                .display_text
                .contains("pub fn adapt(&self) -> String"),
            "overview should contain method: {}",
            overview.display_text
        );
        assert!(
            overview.display_text.contains("\"adapted\".to_string()"),
            "overview should contain method body: {}",
            overview.display_text
        );
        let fn_chunks: Vec<_> = chunks
            .iter()
            .filter(|c| c.metadata.chunk_type == ChunkType::Function)
            .collect();
        assert_eq!(
            fn_chunks.len(),
            0,
            "small method should not produce a separate function chunk"
        );
    }

    #[test]
    fn nested_module() {
        let source = r#"
mod inner {
    pub struct Nested {
        pub val: u32,
    }

    impl Nested {
        pub fn value(&self) -> u32 {
            self.val
        }
    }

    pub fn helper() -> u32 {
        42
    }
}
"#;
        let chunks = chunk_source(source);

        let overview = chunks
            .iter()
            .find(|c| c.metadata.symbol_name == "Nested")
            .expect("should find Nested in inner module");
        assert_eq!(overview.metadata.kind, ChunkKind::Struct);

        // Small free function (≤5 lines) is inlined into module overview chunk.
        let mod_overview = chunks
            .iter()
            .find(|c| c.metadata.kind == ChunkKind::Module)
            .expect("should have module overview containing helper");
        assert!(
            mod_overview.display_text.contains("pub fn helper()"),
            "module overview should contain helper: {}",
            mod_overview.display_text
        );
        assert!(
            mod_overview.embedding_text.contains("crate::example"),
            "embedding: {}",
            mod_overview.embedding_text
        );
    }

    #[test]
    fn visibility_detection() {
        // Test parse_visibility directly on tree-sitter nodes since small
        // free functions (≤5 lines) no longer get individual chunks.
        let source = r#"
pub fn public_fn() {}
pub(crate) fn crate_fn() {}
pub(super) fn super_fn() {}
fn private_fn() {}
"#;

        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_rust::LANGUAGE.into())
            .unwrap();
        let tree = parser.parse(source, None).unwrap();
        let root = tree.root_node();

        let mut functions = Vec::new();
        let mut cursor = root.walk();
        for child in root.children(&mut cursor) {
            if child.kind() == "function_item" {
                let name = child
                    .child_by_field_name("name")
                    .map(|n| n.utf8_text(source.as_bytes()).unwrap())
                    .unwrap_or("");
                let vis = parse_visibility(child, source.as_bytes());
                functions.push((name, vis));
            }
        }

        assert_eq!(functions.len(), 4);
        assert_eq!(functions[0], ("public_fn", Visibility::Pub));
        assert_eq!(functions[1], ("crate_fn", Visibility::PubCrate));
        assert_eq!(functions[2], ("super_fn", Visibility::PubSuper));
        assert_eq!(functions[3], ("private_fn", Visibility::Private));

        // Also verify the module overview chunk exists with all functions.
        let chunks = chunk_source(source);
        let mod_overview = chunks
            .iter()
            .find(|c| c.metadata.kind == ChunkKind::Module)
            .expect("should have module overview");
        assert!(mod_overview.display_text.contains("pub fn public_fn"));
        assert!(mod_overview.display_text.contains("pub(crate) fn crate_fn"));
        assert!(mod_overview.display_text.contains("pub(super) fn super_fn"));
        assert!(mod_overview.display_text.contains("fn private_fn"));
    }

    #[test]
    fn enum_definition() {
        let source = r#"
pub enum Color {
    Red,
    Green,
    Blue,
}
"#;
        let chunks = chunk_source(source);
        let overview = chunks
            .iter()
            .find(|c| c.metadata.symbol_name == "Color")
            .expect("should have Color overview");
        assert_eq!(overview.metadata.kind, ChunkKind::Enum);
        assert_eq!(overview.metadata.visibility, Visibility::Pub);
    }

    #[test]
    fn parses_project_config_rs() {
        let config_source =
            std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config.rs"))
                .expect("should read src/config.rs");

        let chunker = RustChunker;
        let chunks = chunker
            .chunk(Path::new("src/config.rs"), &config_source)
            .expect("should chunk config.rs");

        // Should have overview chunks for Config and ConfigPaths.
        let config_overview = chunks.iter().find(|c| {
            c.metadata.symbol_name == "Config" && c.metadata.chunk_type == ChunkType::Overview
        });
        assert!(config_overview.is_some(), "should have Config overview");

        let config_paths_overview = chunks.iter().find(|c| {
            c.metadata.symbol_name == "ConfigPaths" && c.metadata.chunk_type == ChunkType::Overview
        });
        assert!(
            config_paths_overview.is_some(),
            "should have ConfigPaths overview"
        );

        // Should have function chunks for free functions.
        let fn_names: Vec<&str> = chunks
            .iter()
            .filter(|c| c.metadata.chunk_type == ChunkType::Function && c.metadata.parent.is_none())
            .map(|c| c.metadata.symbol_name.as_str())
            .collect();
        assert!(
            fn_names.contains(&"find_git_root"),
            "missing find_git_root, got: {fn_names:?}"
        );
        assert!(
            fn_names.contains(&"config_paths"),
            "missing config_paths, got: {fn_names:?}"
        );
        assert!(
            fn_names.contains(&"load_config"),
            "missing load_config, got: {fn_names:?}"
        );
        assert!(
            fn_names.contains(&"load_layer_single"),
            "missing load_layer_single, got: {fn_names:?}"
        );

        // Module path should be crate::config.
        let config_ov = config_overview.unwrap();
        assert!(
            config_ov.embedding_text.contains("crate::config"),
            "embedding: {}",
            config_ov.embedding_text
        );
    }

    #[test]
    fn module_path_derivation() {
        assert_eq!(derive_module_path(Path::new("src/lib.rs")), "crate");
        assert_eq!(derive_module_path(Path::new("src/main.rs")), "crate");
        assert_eq!(derive_module_path(Path::new("src/foo.rs")), "crate::foo");
        assert_eq!(
            derive_module_path(Path::new("src/foo/mod.rs")),
            "crate::foo"
        );
        assert_eq!(
            derive_module_path(Path::new("src/net/pool.rs")),
            "crate::net::pool"
        );
    }
}
