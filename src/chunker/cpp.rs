//! C++ chunker.
//!
//! Tree-sitter-cpp-backed AST chunker for `.cpp/.cc/.cxx/.h/.hpp/.hh/...`
//! sources. Mirrors the two-pass design of `RustChunker`:
//!
//! - **Pass 1** walks the translation unit, descends into `namespace_definition`
//!   bodies, and collects:
//!   - class/struct/union definitions (with their methods)
//!   - free functions (and out-of-class method definitions like `void Foo::bar()`)
//!   - top-level preprocessor directives (`#include`, `#define`, ...)
//! - **Pass 2** emits `Chunk` values: a per-file skeleton chunk containing
//!   preprocessor directives and a list of top-level symbols, an overview
//!   chunk for each class/struct/union, and a function chunk for each large
//!   method/free function.
//!
//! Symbol names are namespace-qualified (e.g. `MyNs::MyClass::myMethod`).
//! Out-of-class definitions like `void MyClass::method()` produce a function
//! chunk attributed to `MyClass`.
//!
//! `ChunkKind` reuses existing variants:
//! - `class`/`struct`/`union` → `ChunkKind::Struct`
//! - `enum`                    → `ChunkKind::Enum`
//! - functions/methods         → `ChunkKind::Function`
//! - namespaces / file skeleton → `ChunkKind::Module`
//!
//! Visibility maps as: `public` → `Pub`, `protected`/`private` → `Private`.
//! C++ has no perfect analogue for `pub(crate)`/`pub(super)`, so we collapse
//! the non-public access levels.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use tree_sitter::{Node, Parser};

use crate::chunk::{Chunk, ChunkKind, ChunkMetadata, ChunkOrigin, ChunkType, Visibility};

use super::Chunker;

/// Methods/functions with this many lines or fewer are inlined into their
/// parent overview chunk instead of getting a standalone function chunk.
const MIN_METHOD_CHUNK_LINES: usize = 5;

pub struct CppChunker;

impl Chunker for CppChunker {
    fn chunk(&self, file_path: &Path, source: &str) -> Result<Vec<Chunk>> {
        let mut parser = Parser::new();
        parser
            .set_language(&tree_sitter_cpp::LANGUAGE.into())
            .context("failed to load tree-sitter C++ grammar")?;

        let tree = parser
            .parse(source, None)
            .context("tree-sitter parse returned None")?;

        let root = tree.root_node();
        let module_name = derive_module_name(file_path);

        let mut collector = ChunkCollector {
            source: source.as_bytes(),
            source_str: source,
            file_path,
            module_name: module_name.clone(),
            classes: HashMap::new(),
            class_order: Vec::new(),
            free_functions: Vec::new(),
            preproc_directives: Vec::new(),
        };

        collector.collect_top_level(root, "");
        Ok(collector.into_chunks())
    }
}

/// Derive a short "module name" for a C++ source file. Used as the symbol
/// name of the file skeleton chunk and in embedding-text headers.
///
/// `src/foo/bar.cpp` → `bar`, `include/baz.h` → `baz`.
fn derive_module_name(file_path: &Path) -> String {
    file_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "_".to_string())
}

/// Extract the text of a node from source bytes.
fn node_text<'a>(node: Node<'_>, source: &'a [u8]) -> &'a str {
    node.utf8_text(source).unwrap_or("")
}

/// Strip the body `{ ... }` from a function definition, keeping the
/// declaration up to (but not including) the body, then append `;`.
fn signature_without_body(source: &str, node: Node<'_>) -> String {
    let mut cursor = node.walk();
    let body_start = node
        .children(&mut cursor)
        .find(|c| c.kind() == "compound_statement" || c.kind() == "field_declaration_list")
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

/// Extract the first line of a function/type definition (up to `{`) as a
/// concise signature string.
fn extract_signature(source: &str, node: Node<'_>) -> String {
    let text = &source[node.start_byte()..node.end_byte()];
    match text.find('{') {
        Some(pos) => text[..pos].trim().to_string(),
        None => text.lines().next().unwrap_or("").trim().to_string(),
    }
}

/// Extract the first preceding `//` line-comment line as a brief doc comment.
/// C++ has no standardized doc-comment marker like Rust's `///`, so we treat
/// any contiguous run of `//` lines immediately above the node as the doc.
fn extract_doc_comment(source: &str, start_row: usize) -> Option<String> {
    let lines: Vec<&str> = source.lines().collect();
    let mut row = start_row;
    while row > 0 {
        row -= 1;
        let line = lines.get(row)?.trim();
        if let Some(rest) = line.strip_prefix("///") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        } else if let Some(rest) = line.strip_prefix("//") {
            let trimmed = rest.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        } else if line.is_empty() {
            continue;
        } else {
            break;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Intermediate data structures for two-pass collection
// ---------------------------------------------------------------------------

struct ClassOverview<'a> {
    /// The class/struct/union node. `None` for orphan classes referenced
    /// only by out-of-class method definitions.
    definition: Option<Node<'a>>,
    /// Methods defined inline inside the class body.
    inline_methods: Vec<Node<'a>>,
    /// Out-of-class definitions (e.g. `void Foo::bar() {}` in a `.cpp`).
    out_of_class_methods: Vec<Node<'a>>,
    kind: ChunkKind,
    /// Top-level visibility — class members are tracked separately during
    /// signature emission via access specifiers in the source.
    visibility: Visibility,
    /// Doc comment immediately above the definition.
    doc_comment: Option<String>,
    /// The fully-qualified name including any enclosing namespaces.
    qualified_name: String,
}

struct FreeFunction<'a> {
    node: Node<'a>,
    /// The enclosing namespace prefix (e.g. `MyNs` or `MyNs::Inner`), or
    /// empty for top-level functions.
    namespace: String,
}

struct ChunkCollector<'a> {
    source: &'a [u8],
    source_str: &'a str,
    file_path: &'a Path,
    module_name: String,
    /// Classes keyed by their fully-qualified name (e.g. `MyNs::MyClass`).
    classes: HashMap<String, ClassOverview<'a>>,
    /// Insertion order of class keys, so output is deterministic and matches
    /// source order.
    class_order: Vec<String>,
    free_functions: Vec<FreeFunction<'a>>,
    /// Source text of preprocessor directives at the top level, in source
    /// order. Stored as raw strings (not nodes) for simplicity.
    preproc_directives: Vec<String>,
}

impl<'a> ChunkCollector<'a> {
    /// First pass: walk a container node (translation unit or namespace body)
    /// and collect classes, free functions, and (at the top level) preprocessor
    /// directives.
    fn collect_top_level(&mut self, container: Node<'a>, current_namespace: &str) {
        let mut cursor = container.walk();
        for child in container.children(&mut cursor) {
            match child.kind() {
                "namespace_definition" => self.collect_namespace(child, current_namespace),
                "class_specifier" | "struct_specifier" | "union_specifier" => {
                    self.collect_class(child, current_namespace);
                }
                "function_definition" => self.collect_function(child, current_namespace),
                "template_declaration" => {
                    // Templates wrap a class or function — recurse into the inner
                    // declaration so it gets registered.
                    self.collect_template(child, current_namespace);
                }
                "preproc_include"
                | "preproc_def"
                | "preproc_function_def"
                | "preproc_call"
                | "preproc_ifdef"
                | "preproc_if"
                | "preproc_else"
                | "preproc_elif" => {
                    // Only collect preprocessor directives at the translation-unit
                    // level — those inside namespaces are uncommon and skipped.
                    if current_namespace.is_empty() {
                        let text = node_text(child, self.source);
                        // First line only — keeps the file skeleton compact.
                        let first_line = text.lines().next().unwrap_or("").trim();
                        if !first_line.is_empty() {
                            self.preproc_directives.push(first_line.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn collect_template(&mut self, node: Node<'a>, current_namespace: &str) {
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            match child.kind() {
                "class_specifier" | "struct_specifier" | "union_specifier" => {
                    self.collect_class(child, current_namespace);
                    return;
                }
                "function_definition" => {
                    self.collect_function(child, current_namespace);
                    return;
                }
                "template_declaration" => {
                    // Nested template — keep peeling.
                    self.collect_template(child, current_namespace);
                    return;
                }
                _ => {}
            }
        }
    }

    fn collect_namespace(&mut self, node: Node<'a>, parent_namespace: &str) {
        let name = node
            .child_by_field_name("name")
            .map(|n| node_text(n, self.source).to_string())
            .unwrap_or_default();
        let child_namespace = if name.is_empty() {
            parent_namespace.to_string()
        } else if parent_namespace.is_empty() {
            name
        } else {
            format!("{parent_namespace}::{name}")
        };

        // The body is a `declaration_list` child.
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "declaration_list" {
                self.collect_top_level(child, &child_namespace);
                break;
            }
        }
    }

    fn collect_class(&mut self, node: Node<'a>, namespace: &str) {
        let name = node
            .child_by_field_name("name")
            .map(|name_node| {
                let base = node_text(name_node, self.source).to_string();
                // For template explicit/partial specializations such as
                // `template<> class Foo<int>` or `template<T> class Bar<T, int>`,
                // a `template_argument_list` immediately follows the name node.
                // Append it so the registered name is `Foo<int>` not just `Foo`.
                match name_node.next_sibling() {
                    Some(sib) if sib.kind() == "template_argument_list" => {
                        format!("{base}{}", node_text(sib, self.source))
                    }
                    _ => base,
                }
            })
            .unwrap_or_else(|| "_".to_string());
        let qualified = if namespace.is_empty() {
            name.clone()
        } else {
            format!("{namespace}::{name}")
        };
        let kind = ChunkKind::Struct; // class/struct/union all map to Struct
        let doc_comment = extract_doc_comment(self.source_str, node.start_position().row);

        // Collect inline method definitions from the class body.
        let mut inline_methods = Vec::new();
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            if child.kind() == "field_declaration_list" {
                let mut inner = child.walk();
                for item in child.children(&mut inner) {
                    match item.kind() {
                        "function_definition" => inline_methods.push(item),
                        "template_declaration" => {
                            // A templated method definition.
                            let mut tcur = item.walk();
                            for tc in item.children(&mut tcur) {
                                if tc.kind() == "function_definition" {
                                    inline_methods.push(tc);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        let entry = self.classes.entry(qualified.clone()).or_insert_with(|| {
            self.class_order.push(qualified.clone());
            ClassOverview {
                definition: None,
                inline_methods: Vec::new(),
                out_of_class_methods: Vec::new(),
                kind,
                visibility: Visibility::Pub,
                doc_comment: None,
                qualified_name: qualified.clone(),
            }
        });
        entry.definition = Some(node);
        entry.kind = kind;
        entry.doc_comment = doc_comment;
        entry.inline_methods.extend(inline_methods);
    }

    fn collect_function(&mut self, node: Node<'a>, namespace: &str) {
        // Inspect the declarator to figure out whether this is a free function
        // or an out-of-class definition like `void Foo::bar() { ... }`.
        let qualified_owner = self.declarator_owner(node);
        match qualified_owner {
            Some(owner_qualified) => {
                // Out-of-class method definition. Attach to the owning class,
                // creating an orphan ClassOverview entry if we haven't seen the
                // class definition yet (it may live in a header).
                let key = if namespace.is_empty() || owner_qualified.contains("::") {
                    owner_qualified.clone()
                } else {
                    format!("{namespace}::{owner_qualified}")
                };
                let entry = self.classes.entry(key.clone()).or_insert_with(|| {
                    self.class_order.push(key.clone());
                    ClassOverview {
                        definition: None,
                        inline_methods: Vec::new(),
                        out_of_class_methods: Vec::new(),
                        kind: ChunkKind::Struct,
                        visibility: Visibility::Pub,
                        doc_comment: None,
                        qualified_name: key.clone(),
                    }
                });
                entry.out_of_class_methods.push(node);
            }
            None => {
                self.free_functions.push(FreeFunction {
                    node,
                    namespace: namespace.to_string(),
                });
            }
        }
    }

    /// If the function's declarator names a `qualified_identifier` (e.g.
    /// `Foo::bar` or `outer::inner::Widget::value`), return everything left
    /// of the final `::` (`Foo` or `outer::inner::Widget`). Otherwise return
    /// `None`.
    ///
    /// Note: tree-sitter-cpp parses nested qualifications right-recursively
    /// (`scope` is the leftmost segment and `name` may itself be another
    /// `qualified_identifier`). We side-step that by taking the full text of
    /// the qualified-identifier node and stripping the last `::segment`.
    fn declarator_owner(&self, node: Node<'a>) -> Option<String> {
        let declarator = node.child_by_field_name("declarator")?;
        let inner = self.unwrap_function_declarator(declarator)?;
        if inner.kind() == "qualified_identifier" {
            let full = node_text(inner, self.source);
            let stripped = full.trim_start_matches("::");
            let (scope, _name) = stripped.rsplit_once("::")?;
            return Some(scope.to_string());
        }
        None
    }

    /// Walk through `function_declarator` / `pointer_declarator` /
    /// `reference_declarator` wrappers to find the inner identifier node
    /// that names the function.
    fn unwrap_function_declarator(&self, declarator: Node<'a>) -> Option<Node<'a>> {
        let mut current = declarator;
        loop {
            match current.kind() {
                "function_declarator" => {
                    current = current.child_by_field_name("declarator")?;
                }
                "pointer_declarator" | "reference_declarator" => {
                    current = current.child_by_field_name("declarator")?;
                }
                _ => return Some(current),
            }
        }
    }

    /// Extract the bare function name from a function definition node.
    /// For `void Foo::bar()` returns `bar`; for `void baz()` returns `baz`.
    fn function_name(&self, node: Node<'a>) -> String {
        let Some(declarator) = node.child_by_field_name("declarator") else {
            return "_".to_string();
        };
        let Some(inner) = self.unwrap_function_declarator(declarator) else {
            return "_".to_string();
        };
        match inner.kind() {
            "qualified_identifier" => {
                // Take the rightmost `::`-separated segment of the full
                // qualified-identifier text — see `declarator_owner` for why.
                let full = node_text(inner, self.source);
                let stripped = full.trim_start_matches("::");
                stripped
                    .rsplit_once("::")
                    .map(|(_scope, name)| name.to_string())
                    .unwrap_or_else(|| stripped.to_string())
            }
            _ => node_text(inner, self.source).to_string(),
        }
    }

    /// Second pass: build `Chunk` values from collected data.
    fn into_chunks(mut self) -> Vec<Chunk> {
        let mut chunks = Vec::new();

        // 1. File skeleton chunk (preprocessor + symbol roster).
        chunks.push(self.build_file_skeleton_chunk());

        // 2. Class overview + per-method function chunks.
        let class_order = std::mem::take(&mut self.class_order);
        let mut classes = std::mem::take(&mut self.classes);
        for key in &class_order {
            if let Some(overview) = classes.remove(key) {
                chunks.push(self.build_class_overview_chunk(&overview));

                for method in overview
                    .inline_methods
                    .iter()
                    .chain(&overview.out_of_class_methods)
                {
                    let lines = method.end_position().row - method.start_position().row + 1;
                    if lines > MIN_METHOD_CHUNK_LINES {
                        chunks.push(self.build_method_chunk(&overview, *method));
                    }
                }
            }
        }

        // 3. Free functions.
        let free_functions = std::mem::take(&mut self.free_functions);
        for free_fn in &free_functions {
            let lines = free_fn.node.end_position().row - free_fn.node.start_position().row + 1;
            if lines > MIN_METHOD_CHUNK_LINES {
                chunks.push(self.build_free_function_chunk(free_fn));
            } else {
                // Small free functions are not worth their own chunk; they
                // are still listed in the file skeleton above.
            }
        }

        chunks
    }

    fn build_file_skeleton_chunk(&self) -> Chunk {
        let mut display_parts: Vec<String> = Vec::new();
        display_parts.push(format!("// {}", self.file_path.to_string_lossy()));

        if !self.preproc_directives.is_empty() {
            display_parts.push(self.preproc_directives.join("\n"));
        }

        // Roster of declared symbols at file scope.
        let mut roster: Vec<String> = Vec::new();
        for key in &self.class_order {
            roster.push(format!("class {key};"));
        }
        for free_fn in &self.free_functions {
            let sig = signature_without_body(self.source_str, free_fn.node);
            let qualified = if free_fn.namespace.is_empty() {
                sig
            } else {
                format!("// in namespace {}\n{sig}", free_fn.namespace)
            };
            roster.push(qualified);
        }
        if !roster.is_empty() {
            display_parts.push(roster.join("\n"));
        }

        let display_text = display_parts.join("\n\n");
        let file_path_str = self.file_path.to_string_lossy().to_string();

        let embedding_text = format!(
            "// {file}\n\
             // File skeleton — preprocessor directives and top-level symbols\n\
             {display}",
            file = file_path_str,
            display = display_text,
        );

        Chunk {
            display_text,
            embedding_text,
            metadata: ChunkMetadata {
                symbol_name: self.module_name.clone(),
                kind: ChunkKind::Module,
                chunk_type: ChunkType::Overview,
                parent: None,
                visibility: Visibility::Pub,
                file_path: file_path_str,
                line_range: (0, 0),
                signature: None,
            },
            embedding: None,
            origin: ChunkOrigin::default(),
        }
    }

    fn build_class_overview_chunk(&self, overview: &ClassOverview<'_>) -> Chunk {
        let mut display_parts: Vec<String> = Vec::new();

        if let Some(def) = overview.definition {
            // Build a body listing only signatures (no method bodies).
            let header_text = self.class_header_text(def);
            let mut body = format!("{header_text} {{");
            self.append_class_member_signatures(def, &mut body);
            body.push_str("\n};");
            display_parts.push(body);
        } else {
            // Orphan: only out-of-class definitions are visible in this file.
            display_parts.push(format!("// (forward) class {};", overview.qualified_name));
        }

        // Out-of-class definition signatures appended for context.
        if !overview.out_of_class_methods.is_empty() {
            let mut out = String::from("// out-of-class definitions:");
            for method in &overview.out_of_class_methods {
                let sig = signature_without_body(self.source_str, *method);
                out.push_str("\n");
                out.push_str(&sig);
            }
            display_parts.push(out);
        }

        let display_text = display_parts.join("\n\n");

        let start_row = overview
            .definition
            .map(|n| n.start_position().row)
            .into_iter()
            .chain(
                overview
                    .out_of_class_methods
                    .iter()
                    .map(|m| m.start_position().row),
            )
            .min()
            .unwrap_or(0);
        let end_row = overview
            .definition
            .map(|n| n.end_position().row)
            .into_iter()
            .chain(
                overview
                    .out_of_class_methods
                    .iter()
                    .map(|m| m.end_position().row),
            )
            .max()
            .unwrap_or(0);

        let signature = overview
            .definition
            .map(|n| extract_signature(self.source_str, n));

        let file_path_str = self.file_path.to_string_lossy().to_string();
        let doc_brief = overview
            .doc_comment
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();

        let embedding_text = format!(
            "// {file}:{start}..{end}\n\
             // class {name}{doc}\n\
             {display}",
            file = file_path_str,
            start = start_row,
            end = end_row,
            name = overview.qualified_name,
            doc = doc_brief,
            display = display_text,
        );

        Chunk {
            display_text,
            embedding_text,
            metadata: ChunkMetadata {
                symbol_name: overview.qualified_name.clone(),
                kind: overview.kind,
                chunk_type: ChunkType::Overview,
                parent: None,
                visibility: overview.visibility,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature,
            },
            embedding: None,
            origin: ChunkOrigin::default(),
        }
    }

    /// Build the class header text: everything from the class node up to the
    /// opening `{` of the body. Strips the trailing `{` if present.
    fn class_header_text(&self, class_node: Node<'_>) -> String {
        let text = &self.source_str[class_node.start_byte()..class_node.end_byte()];
        match text.find('{') {
            Some(pos) => text[..pos].trim().to_string(),
            None => text.lines().next().unwrap_or("").trim().to_string(),
        }
    }

    /// Append signature-only listings for the members of a class body to
    /// `out`. Includes access specifiers and field declarations as-is, but
    /// strips method bodies down to signatures.
    fn append_class_member_signatures(&self, class_node: Node<'_>, out: &mut String) {
        let mut cursor = class_node.walk();
        for child in class_node.children(&mut cursor) {
            if child.kind() != "field_declaration_list" {
                continue;
            }
            let mut inner = child.walk();
            for item in child.children(&mut inner) {
                match item.kind() {
                    "access_specifier" => {
                        let text = node_text(item, self.source).trim();
                        out.push_str("\n");
                        out.push_str(text);
                        out.push(':');
                    }
                    "function_definition" => {
                        let sig = signature_without_body(self.source_str, item);
                        out.push_str("\n    ");
                        out.push_str(&sig);
                    }
                    "template_declaration" => {
                        // Show the template line plus the inner signature.
                        let text = node_text(item, self.source);
                        let first_line = text.lines().next().unwrap_or("").trim();
                        out.push_str("\n    ");
                        out.push_str(first_line);
                        // Find the inner function/field for its signature.
                        let mut tcur = item.walk();
                        for tc in item.children(&mut tcur) {
                            if tc.kind() == "function_definition" {
                                let sig = signature_without_body(self.source_str, tc);
                                out.push_str("\n    ");
                                out.push_str(&sig);
                                break;
                            }
                            if tc.kind() == "field_declaration" {
                                let text = node_text(tc, self.source).trim();
                                out.push_str("\n    ");
                                out.push_str(text);
                                break;
                            }
                        }
                    }
                    "field_declaration" | "declaration" => {
                        let text = node_text(item, self.source).trim();
                        out.push_str("\n    ");
                        out.push_str(text);
                    }
                    _ => {}
                }
            }
        }
    }

    fn build_method_chunk(&self, overview: &ClassOverview<'_>, method: Node<'_>) -> Chunk {
        let method_name = self.function_name(method);
        let method_text = node_text(method, self.source);
        let signature = extract_signature(self.source_str, method);

        let display_text = format!(
            "// class {parent}\n{body}",
            parent = overview.qualified_name,
            body = method_text,
        );

        let file_path_str = self.file_path.to_string_lossy().to_string();
        let start_row = method.start_position().row;
        let end_row = method.end_position().row;
        let doc_comment = extract_doc_comment(self.source_str, start_row);
        let doc_brief = doc_comment
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();

        let symbol_name = format!("{}::{}", overview.qualified_name, method_name);

        let embedding_text = format!(
            "// {file}:{start}..{end}\n\
             // {symbol}{doc}\n\
             {display}",
            file = file_path_str,
            start = start_row,
            end = end_row,
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
                parent: Some(overview.qualified_name.clone()),
                visibility: Visibility::Pub,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature: Some(signature),
            },
            embedding: None,
            origin: ChunkOrigin::default(),
        }
    }

    fn build_free_function_chunk(&self, free_fn: &FreeFunction<'_>) -> Chunk {
        let node = free_fn.node;
        let fn_name = self.function_name(node);
        let fn_text = node_text(node, self.source);
        let signature = extract_signature(self.source_str, node);

        let display_text = if free_fn.namespace.is_empty() {
            fn_text.to_string()
        } else {
            format!("// namespace {}\n{}", free_fn.namespace, fn_text)
        };

        let file_path_str = self.file_path.to_string_lossy().to_string();
        let start_row = node.start_position().row;
        let end_row = node.end_position().row;
        let doc_comment = extract_doc_comment(self.source_str, start_row);
        let doc_brief = doc_comment
            .as_deref()
            .map(|d| format!(" — {d}"))
            .unwrap_or_default();

        let symbol_name = if free_fn.namespace.is_empty() {
            fn_name.clone()
        } else {
            format!("{}::{}", free_fn.namespace, fn_name)
        };

        let embedding_text = format!(
            "// {file}:{start}..{end}\n\
             // {symbol}{doc}\n\
             {display}",
            file = file_path_str,
            start = start_row,
            end = end_row,
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
                parent: None,
                visibility: Visibility::Pub,
                file_path: file_path_str,
                line_range: (start_row, end_row),
                signature: Some(signature),
            },
            embedding: None,
            origin: ChunkOrigin::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_source(file_name: &str, source: &str) -> Vec<Chunk> {
        let chunker = CppChunker;
        chunker
            .chunk(Path::new(file_name), source)
            .expect("chunking should succeed")
    }

    #[test]
    fn struct_with_methods() {
        let source = r#"
struct Counter {
    int value;

    int get() const {
        return value;
    }

    void increment_by(int delta) {
        value += delta;
        if (value > 100) {
            value = 100;
        }
        return;
    }
};
"#;
        let chunks = chunk_source("src/counter.hpp", source);

        let overview = chunks
            .iter()
            .find(|c| {
                c.metadata.symbol_name == "Counter" && c.metadata.chunk_type == ChunkType::Overview
            })
            .expect("Counter overview present");
        assert_eq!(overview.metadata.kind, ChunkKind::Struct);
        assert!(overview.display_text.contains("struct Counter"));
        assert!(
            overview.display_text.contains("int get() const"),
            "overview should list get() signature: {}",
            overview.display_text
        );
        assert!(
            overview
                .display_text
                .contains("void increment_by(int delta)"),
            "overview should list increment_by signature: {}",
            overview.display_text
        );
        // Method bodies must NOT appear in the overview chunk.
        assert!(
            !overview.display_text.contains("value += delta"),
            "overview must be signatures only: {}",
            overview.display_text
        );

        // The large method should produce its own function chunk.
        let large_method = chunks
            .iter()
            .find(|c| c.metadata.symbol_name == "Counter::increment_by")
            .expect("increment_by function chunk present");
        assert_eq!(large_method.metadata.kind, ChunkKind::Function);
        assert_eq!(large_method.metadata.parent.as_deref(), Some("Counter"));
        assert!(large_method.display_text.contains("value += delta"));

        // The small `get()` method (≤5 lines) should NOT produce a separate chunk.
        assert!(
            chunks
                .iter()
                .all(|c| c.metadata.symbol_name != "Counter::get"),
            "small inline method should not get its own chunk"
        );
    }

    #[test]
    fn free_function_in_namespace() {
        let source = r#"
namespace math {

int add(int a, int b) {
    return a + b;
}

double compute_average(const double* values, int count) {
    double sum = 0.0;
    for (int i = 0; i < count; ++i) {
        sum += values[i];
    }
    if (count == 0) {
        return 0.0;
    }
    return sum / count;
}

}
"#;
        let chunks = chunk_source("src/math.cpp", source);

        // File skeleton must list both functions with their namespace.
        let skeleton = chunks
            .iter()
            .find(|c| c.metadata.kind == ChunkKind::Module)
            .expect("file skeleton present");
        assert!(
            skeleton.display_text.contains("namespace math"),
            "skeleton should mention namespace: {}",
            skeleton.display_text
        );

        // Large function gets its own chunk; small one is inlined into the
        // skeleton only.
        let big = chunks
            .iter()
            .find(|c| c.metadata.symbol_name == "math::compute_average")
            .expect("compute_average function chunk present");
        assert_eq!(big.metadata.kind, ChunkKind::Function);
        assert!(big.display_text.contains("namespace math"));
        assert!(big.display_text.contains("sum += values[i]"));

        assert!(
            chunks.iter().all(|c| c.metadata.symbol_name != "math::add"),
            "small free function should not get its own chunk"
        );
    }

    #[test]
    fn preprocessor_directives_in_file_skeleton() {
        let source = r#"
#include <vector>
#include "local.h"
#define MAX_SIZE 1024

int trivial() { return 0; }
"#;
        let chunks = chunk_source("src/with_preproc.cpp", source);

        let skeleton = chunks
            .iter()
            .find(|c| c.metadata.kind == ChunkKind::Module)
            .expect("file skeleton present");
        assert!(
            skeleton.display_text.contains("#include <vector>"),
            "skeleton should contain #include: {}",
            skeleton.display_text
        );
        assert!(
            skeleton.display_text.contains("#include \"local.h\""),
            "skeleton should contain quoted include: {}",
            skeleton.display_text
        );
        assert!(
            skeleton.display_text.contains("#define MAX_SIZE 1024"),
            "skeleton should contain #define: {}",
            skeleton.display_text
        );
        assert_eq!(skeleton.metadata.symbol_name, "with_preproc");
    }

    #[test]
    fn nested_namespace_qualifies_class() {
        let source = r#"
namespace outer {
namespace inner {

class Widget {
public:
    Widget();
    int value() const;
};

}
}
"#;
        let chunks = chunk_source("include/widget.h", source);

        let widget = chunks
            .iter()
            .find(|c| {
                c.metadata.symbol_name == "outer::inner::Widget"
                    && c.metadata.chunk_type == ChunkType::Overview
            })
            .expect("nested namespaced class overview present");
        assert_eq!(widget.metadata.kind, ChunkKind::Struct);
        assert!(widget.display_text.contains("class Widget"));
        assert!(widget.display_text.contains("public:"));
        assert!(widget.display_text.contains("Widget()"));
        assert!(widget.display_text.contains("int value() const"));
    }

    #[test]
    fn out_of_class_method_definition() {
        let source = r#"
#include "widget.h"

namespace outer {

int outer::inner::Widget::value() const {
    int result = 0;
    for (int i = 0; i < 10; ++i) {
        result += i;
    }
    return result;
}

}
"#;
        let chunks = chunk_source("src/widget.cpp", source);

        // The class is referenced but never defined here, so we should still
        // see an overview chunk attributed to the qualified class name.
        let overview = chunks
            .iter()
            .find(|c| {
                c.metadata.chunk_type == ChunkType::Overview && c.metadata.kind == ChunkKind::Struct
            })
            .expect("orphan class overview present");
        assert!(
            overview.metadata.symbol_name.contains("Widget"),
            "symbol_name should mention Widget: {}",
            overview.metadata.symbol_name
        );
        assert!(
            overview.display_text.contains("out-of-class definitions"),
            "overview should list out-of-class methods: {}",
            overview.display_text
        );

        // The method body is large enough to get its own function chunk.
        let method = chunks
            .iter()
            .find(|c| {
                c.metadata.chunk_type == ChunkType::Function
                    && c.metadata.symbol_name.ends_with("::value")
            })
            .expect("Widget::value function chunk present");
        assert!(method.display_text.contains("result += i"));
        assert!(method.metadata.parent.is_some());
    }

    #[test]
    fn class_with_access_specifiers() {
        let source = r#"
class Buffer {
public:
    Buffer();
    int size() const;

private:
    int* data;
    int len;
};
"#;
        let chunks = chunk_source("include/buffer.h", source);

        let overview = chunks
            .iter()
            .find(|c| c.metadata.symbol_name == "Buffer")
            .expect("Buffer overview present");
        assert!(overview.display_text.contains("public:"));
        assert!(overview.display_text.contains("private:"));
        assert!(overview.display_text.contains("Buffer()"));
        assert!(overview.display_text.contains("int size() const"));
        assert!(overview.display_text.contains("int* data"));
    }

    #[test]
    fn template_full_specialization_name_includes_arguments() {
        let source = r#"
template<> class Foo<int> {
public:
    int value() const;
    void set_value(int v);
    void reset_to_default_and_notify();
};
"#;
        let chunks = chunk_source("include/foo.h", source);

        // The chunk name must be `Foo<int>`, not just `Foo`.
        let overview = chunks
            .iter()
            .find(|c| {
                c.metadata.symbol_name == "Foo<int>" && c.metadata.chunk_type == ChunkType::Overview
            })
            .expect(
                "Foo<int> overview chunk present — full specialization name must include <int>",
            );
        assert_eq!(overview.metadata.kind, ChunkKind::Struct);
        assert!(overview.display_text.contains("Foo"));
    }

    #[test]
    fn template_partial_specialization_name_includes_arguments() {
        let source = r#"
template<typename T> class Bar<T, int> {
public:
    T first() const;
    int second() const;
    void apply_transform_and_notify(T val);
};
"#;
        let chunks = chunk_source("include/bar.h", source);

        // The chunk name must be `Bar<T, int>`, not just `Bar`.
        let overview = chunks
            .iter()
            .find(|c| {
                c.metadata.symbol_name == "Bar<T, int>"
                    && c.metadata.chunk_type == ChunkType::Overview
            })
            .expect(
                "Bar<T, int> overview chunk present — partial specialization name must include <T, int>",
            );
        assert_eq!(overview.metadata.kind, ChunkKind::Struct);
        assert!(overview.display_text.contains("Bar"));
    }
}
