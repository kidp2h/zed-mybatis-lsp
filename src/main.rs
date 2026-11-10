use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

use async_trait::async_trait;
use regex::Regex;
use tokio::sync::RwLock;
use tower_lsp::lsp_types::{
    DidChangeTextDocumentParams, DidOpenTextDocumentParams, GotoDefinitionParams,
    GotoDefinitionResponse, InitializeParams, InitializeResult, Location, OneOf, Position, Range,
    ReferenceParams, ServerCapabilities, TextDocumentSyncCapability, TextDocumentSyncKind, Url,
};
use tower_lsp::{Client, LanguageServer, LspService, Server};
use walkdir::WalkDir;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MapperKey {
    namespace: String,
    id: String,
}

#[derive(Clone, Debug)]
struct SymbolLocation {
    uri: Url,
    range: Range,
}

#[derive(Clone, Debug)]
struct JavaParameter {
    name: String,
    alias: Option<String>,
    type_name: String,
    location: SymbolLocation,
}

#[derive(Default)]
struct WorkspaceIndex {
    root: Option<PathBuf>,
    documents: HashMap<Url, String>,
    java_methods: HashMap<MapperKey, SymbolLocation>,
    java_method_parameters: HashMap<MapperKey, Vec<JavaParameter>>,
    xml_statements: HashMap<MapperKey, SymbolLocation>,
    xml_parameter_types: HashMap<MapperKey, String>,
    java_types: HashMap<String, String>,
    java_fields: HashMap<String, HashMap<String, SymbolLocation>>,
}

impl WorkspaceIndex {
    fn rebuild(&mut self, root: PathBuf) {
        self.root = Some(root.clone());
        self.documents.clear();
        self.java_methods.clear();
        self.java_method_parameters.clear();
        self.xml_statements.clear();
        self.xml_parameter_types.clear();
        self.java_types.clear();
        self.java_fields.clear();

        let walker = WalkDir::new(root).into_iter().filter_entry(|entry| {
            //  Skip build output and tool folders
            !(entry.file_type().is_dir()
                && matches!(
                    entry.file_name().to_str(),
                    Some("target" | "build" | "node_modules" | ".git" | ".idea")
                ))
        });
        for entry in walker.filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if !is_source_file(path) {
                continue;
            }
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(uri) = Url::from_file_path(path) {
                    self.update_document(uri, text);
                }
            }
        }
    }

    fn update_document(&mut self, uri: Url, text: String) {
        self.remove_document(&uri);
        if let Ok(path) = uri.to_file_path() {
            if path.extension().and_then(|v| v.to_str()) == Some("java") {
                self.index_java(&uri, &text);
            } else if path.extension().and_then(|v| v.to_str()) == Some("xml") {
                self.index_xml(&uri, &text);
            }
        }
        self.documents.insert(uri, text);
    }

    fn remove_document(&mut self, uri: &Url) {
        self.documents.remove(uri);
        self.java_methods.retain(|_, location| &location.uri != uri);
        self.java_method_parameters
            .retain(|key, _| self.java_methods.contains_key(key));
        self.xml_statements
            .retain(|_, location| &location.uri != uri);
        self.xml_parameter_types
            .retain(|key, _| self.xml_statements.contains_key(key));
        self.java_fields.retain(|_, fields| {
            fields.retain(|_, location| &location.uri != uri);
            !fields.is_empty()
        });
    }

    fn index_java(&mut self, uri: &Url, text: &str) {
        let package = Regex::new(r"(?m)^\s*package\s+([A-Za-z_][\w.]*)\s*;")
            .unwrap()
            .captures(text)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let type_re = Regex::new(
            r"\b(?:public\s+|protected\s+|private\s+|abstract\s+|final\s+|static\s+)*(class|interface|record|enum)\s+([A-Za-z_]\w*)",
        )
        .unwrap();
        let method_re = Regex::new(r"(?m)^\s*(?:@[A-Za-z_][\w.]*.*\n\s*)?(?:public\s+|default\s+|protected\s+|private\s+|static\s+|<[^>]+>\s+|[\w<>\[\],.?]+\s+)+([A-Za-z_]\w*)\s*\(").unwrap();
        let imports = Regex::new(r"(?m)^\s*import\s+([A-Za-z_][\w.]*)\s*;").unwrap();
        let mut imported_types = HashMap::new();
        for capture in imports.captures_iter(text) {
            if let Some(simple) = capture[1].rsplit('.').next() {
                imported_types.insert(simple.to_string(), capture[1].to_string());
            }
        }

        let mut type_ranges: Vec<(String, usize, usize, bool)> = Vec::new();
        for capture in type_re.captures_iter(text) {
            let is_interface = &capture[1] == "interface";
            let name = capture[2].to_string();
            let parent_namespace = type_ranges
                .iter()
                .filter(|(_, parent_open, parent_close, _)| {
                    *parent_open < capture.get(0).unwrap().start()
                        && capture.get(0).unwrap().start() < *parent_close
                })
                .min_by_key(|(_, parent_open, parent_close, _)| parent_close - parent_open)
                .map(|(namespace, _, _, _)| namespace.clone());
            let namespace = if let Some(parent_namespace) = parent_namespace {
                format!("{parent_namespace}${name}")
            } else if package.is_empty() {
                name.clone()
            } else {
                format!("{package}.{name}")
            };
            self.java_types.insert(name, namespace.clone());
            let declaration_end = capture.get(0).unwrap().end();
            if let Some(open) = text[declaration_end..].find('{') {
                let open = declaration_end + open;
                if let Some(close) = matching_delimiter(text, open, b'{', b'}') {
                    type_ranges.push((namespace, open, close, is_interface));
                }
            }
        }

        let field_re = Regex::new(
            r"(?m)^\s*(?:(?:public|protected|private|static|final|volatile|transient)\s+)+([A-Za-z_$][\w$]*(?:\s*<[^;\n]+>)?(?:\[\])?)\s+([A-Za-z_$][\w$]*)\s*(?:=|;)",
        )
        .unwrap();
        for capture in field_re.captures_iter(text) {
            let Some((namespace, _, _, _)) = type_ranges
                .iter()
                .filter(|(_, open, close, _)| {
                    *open < capture.get(0).unwrap().start()
                        && capture.get(0).unwrap().start() < *close
                })
                .min_by_key(|(_, open, close, _)| close - open)
            else {
                continue;
            };
            let name = capture[2].to_string();
            let start = capture.get(2).unwrap().start();
            self.java_fields
                .entry(namespace.clone())
                .or_default()
                .insert(
                    name.clone(),
                    SymbolLocation {
                        uri: uri.clone(),
                        range: range_for_span(text, start, start + name.len()),
                    },
                );
        }

        let Some(namespace) = type_ranges
            .iter()
            .find(|(_, _, _, is_interface)| *is_interface)
            .map(|(namespace, _, _, _)| namespace.clone())
        else {
            return;
        };

        for capture in method_re.captures_iter(text) {
            let id = capture[1].to_string();
            if ["if", "for", "while", "switch", "catch"].contains(&id.as_str()) {
                continue;
            }
            let start = capture.get(1).unwrap().start();
            let range = range_for_span(text, start, start + id.len());
            self.java_methods.insert(
                MapperKey {
                    namespace: namespace.clone(),
                    id: id.clone(),
                },
                SymbolLocation {
                    uri: uri.clone(),
                    range,
                },
            );
            let method_end = capture.get(1).unwrap().end();
            let Some(open_paren) = text[method_end..].find('(').map(|v| method_end + v) else {
                continue;
            };
            let Some(close_paren) = matching_delimiter(text, open_paren, b'(', b')') else {
                continue;
            };
            self.java_method_parameters.insert(
                MapperKey {
                    namespace: namespace.clone(),
                    id,
                },
                parse_parameters(
                    &text[open_paren + 1..close_paren],
                    open_paren + 1,
                    text,
                    uri,
                    &package,
                    &imported_types,
                ),
            );
        }

        for (simple, full) in imported_types {
            self.java_types.entry(simple).or_insert(full);
        }
    }

    fn index_xml(&mut self, uri: &Url, text: &str) {
        let mapper_re =
            Regex::new(r#"(?s)<mapper\b[^>]*?\bnamespace\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#)
                .unwrap();
        let statement_re = Regex::new(
            r#"(?s)<(select|insert|update|delete|sql)\b[^>]*?\bid\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#,
        )
        .unwrap();
        let Some(mapper) = mapper_re.captures(text) else {
            return;
        };
        let namespace = mapper[1].to_string();
        for capture in statement_re.captures_iter(text) {
            let id = capture[2].to_string();
            let whole = capture.get(0).unwrap();
            let id_offset = whole.start() + whole.as_str().find(&id).unwrap_or(0);
            let range = range_for_span(text, id_offset, id_offset + id.len());
            self.xml_statements.insert(
                MapperKey {
                    namespace: namespace.clone(),
                    id: id.clone(),
                },
                SymbolLocation {
                    uri: uri.clone(),
                    range,
                },
            );
            if let Some(parameter_type) = xml_attribute(whole.as_str(), "parameterType") {
                self.xml_parameter_types.insert(
                    MapperKey {
                        namespace: namespace.clone(),
                        id,
                    },
                    parameter_type,
                );
            }
        }
    }

    fn java_definition(&self, uri: &Url, position: Position) -> Option<Location> {
        let text = self.documents.get(uri)?;
        let offset = offset_at_position(text, position);
        let token = identifier_at(text, offset)?;
        let before = &text[..token.0];
        let receiver = Regex::new(r"([A-Za-z_]\w*)\s*\.\s*$")
            .unwrap()
            .captures(before)
            .map(|c| c[1].to_string());

        let declaration_namespace = self.java_methods.iter().find_map(|(key, location)| {
            if &location.uri != uri {
                return None;
            }
            let start = offset_at_position(text, location.range.start);
            let end = offset_at_position(text, location.range.end);
            (start <= token.0 && token.0 <= end).then(|| key.namespace.clone())
        });

        let mut candidates = Vec::new();
        for (key, location) in &self.xml_statements {
            if key.id != token.1 {
                continue;
            }
            if let Some(ref namespace) = declaration_namespace {
                if &key.namespace != namespace {
                    continue;
                }
            }
            if let Some(ref receiver) = receiver {
                if !receiver_type_matches(text, receiver, &key.namespace, &self.java_types) {
                    continue;
                }
            }
            candidates.push(location);
        }
        candidates
            .first()
            .map(|location| Location::new(location.uri.clone(), location.range))
    }

    fn xml_definition(&self, uri: &Url, position: Position) -> Option<Location> {
        let text = self.documents.get(uri)?;
        let offset = offset_at_position(text, position);
        if let Some(location) = self.xml_refid_definition(text, offset) {
            return Some(location);
        }
        if let Some(location) = self.xml_parameter_definition(text, offset) {
            return Some(location);
        }
        let (namespace, id) = xml_symbol_at(text, offset)?;
        let location = self.java_methods.get(&MapperKey { namespace, id })?;
        Some(Location::new(location.uri.clone(), location.range))
    }

    /// Resolves an `<include refid="...">` value to its `<sql>` definition.
    fn xml_refid_definition(&self, text: &str, offset: usize) -> Option<Location> {
        let refid = xml_refid_at(text, offset)?;
        let key = self.resolve_refid(xml_namespace(text).as_deref(), &refid)?;
        let location = self.xml_statements.get(&key)?;
        Some(Location::new(location.uri.clone(), location.range))
    }

    /// Resolves a refid to the key of an indexed `<sql>` statement.
    fn resolve_refid(&self, namespace: Option<&str>, refid: &str) -> Option<MapperKey> {
        let mut keys = Vec::new();
        // Local id within the current mapper
        if let Some(namespace) = namespace {
            keys.push(MapperKey {
                namespace: namespace.to_string(),
                id: refid.to_string(),
            });
        }
        // Fully qualified id from another mapper
        if let Some((namespace, id)) = refid.rsplit_once('.') {
            keys.push(MapperKey {
                namespace: namespace.to_string(),
                id: id.to_string(),
            });
        }
        keys.into_iter().find(|key| self.xml_statements.contains_key(key))
    }

    /// Finds every `<include refid>` pointing to the statement under the cursor.
    fn xml_references(&self, uri: &Url, position: Position, include_declaration: bool) -> Vec<Location> {
        let Some(text) = self.documents.get(uri) else {
            return Vec::new();
        };
        let offset = offset_at_position(text, position);
        //  Resolve target statement
        let target = match xml_refid_at(text, offset) {
            Some(refid) => self.resolve_refid(xml_namespace(text).as_deref(), &refid),
            None => xml_symbol_at(text, offset).map(|(namespace, id)| MapperKey { namespace, id }),
        };
        let Some(target) = target else {
            return Vec::new();
        };

        //  Collect matching includes
        let mut locations = Vec::new();
        let refid_re = Regex::new(r#"\brefid\s*=\s*[\"']([^\"']+)[\"']"#).unwrap();
        let mut documents: Vec<_> = self
            .documents
            .iter()
            .filter(|(uri, _)| uri.path().ends_with(".xml"))
            .collect();
        documents.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()));
        for (doc_uri, doc_text) in documents {
            let namespace = xml_namespace(doc_text);
            for capture in refid_re.captures_iter(doc_text) {
                let value = capture.get(1).unwrap();
                if self.resolve_refid(namespace.as_deref(), value.as_str().trim()).as_ref() != Some(&target) {
                    continue;
                }
                locations.push(Location::new(
                    doc_uri.clone(),
                    range_for_span(doc_text, value.start(), value.end()),
                ));
            }
        }
        if include_declaration {
            if let Some(location) = self.xml_statements.get(&target) {
                locations.insert(0, Location::new(location.uri.clone(), location.range));
            }
        }
        locations
    }

    fn xml_parameter_definition(&self, text: &str, offset: usize) -> Option<Location> {
        let expression = xml_parameter_at(text, offset)?;
        let (namespace, id) = xml_statement_symbol_at(text, offset)?;
        let key = MapperKey { namespace, id };
        if let Some(type_name) = self.xml_parameter_type_for(text, &key, offset) {
            let parts: Vec<&str> = expression.split('.').collect();
            let property = parts.last().copied()?;
            if parts.len() == 1 {
                if let Some(location) = self.field_location(&type_name, property) {
                    return Some(location);
                }
            }
        }
        let parameters = self.java_method_parameters.get(&key)?;
        let parts: Vec<&str> = expression.split('.').collect();
        let property = parts.last().copied()?;

        if parts.len() > 1 {
            let parameter = parameters
                .iter()
                .find(|parameter| parameter.name == parts[0] || parameter.alias.as_deref() == Some(parts[0]))?;
            return self.field_location(&parameter.type_name, property);
        }

        if let Some(parameter) = parameters
            .iter()
            .find(|parameter| parameter.name == property || parameter.alias.as_deref() == Some(property))
        {
            return Some(Location::new(parameter.location.uri.clone(), parameter.location.range));
        }

        if parameters.len() == 1 {
            if let Some(location) = self.field_location(&parameters[0].type_name, property) {
                return Some(location);
            }
        }
        parameters
            .iter()
            .find_map(|parameter| self.field_location(&parameter.type_name, property))
    }

    fn xml_parameter_type_for(&self, text: &str, key: &MapperKey, offset: usize) -> Option<String> {
        if let Some(type_name) = self.xml_parameter_types.get(key) {
            return Some(type_name.clone());
        }
        if key.id != "adminCondition" {
            return None;
        }
        let statement_re = Regex::new(
            r#"(?s)<(select|insert|update|delete)\b[^>]*?\bid\s*=\s*[\"']([^\"']+)[\"'][^>]*>.*?</(?:select|insert|update|delete)\s*>"#,
        )
        .unwrap();
        for capture in statement_re.captures_iter(text) {
            let whole = capture.get(0).unwrap();
            if !whole.as_str().contains("refid=\"adminCondition\"")
                && !whole.as_str().contains("refid='adminCondition'")
            {
                continue;
            }
            let Some(parameter_type) = xml_attribute(whole.as_str(), "parameterType") else {
                continue;
            };
            let statement_key = MapperKey {
                namespace: key.namespace.clone(),
                id: capture[2].to_string(),
            };
            if self.xml_parameter_types.contains_key(&statement_key) || offset < whole.end() {
                return Some(parameter_type);
            }
        }
        None
    }

    fn field_location(&self, type_name: &str, field: &str) -> Option<Location> {
        let normalized = normalize_type_name(type_name);
        let resolved = self
            .java_types
            .get(&normalized)
            .map(String::as_str)
            .unwrap_or(&normalized);
        let fields = self
            .java_fields
            .get(resolved)
            .or_else(|| self.java_fields.get(&normalized))?;
        let location = fields.get(field)?;
        Some(Location::new(location.uri.clone(), location.range))
    }
}

fn is_source_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|v| v.to_str()),
        Some("java") | Some("xml")
    )
}

fn offset_at_position(text: &str, position: Position) -> usize {
    let mut offset = 0;
    for (line, value) in text.split_inclusive('\n').enumerate() {
        if line == position.line as usize {
            let line_without_newline = value.strip_suffix('\n').unwrap_or(value);
            let mut utf16_offset = 0;
            for (byte_offset, character) in line_without_newline.char_indices() {
                if utf16_offset >= position.character as usize {
                    return offset + byte_offset;
                }
                utf16_offset += character.len_utf16();
            }
            return offset + line_without_newline.len();
        }
        offset += value.len();
    }
    text.len()
}

fn position_at_offset(text: &str, offset: usize) -> Position {
    let offset = offset.min(text.len());
    let prefix = &text[..offset];
    Position::new(
        prefix.bytes().filter(|b| *b == b'\n').count() as u32,
        prefix
            .rsplit('\n')
            .next()
            .unwrap_or_default()
            .encode_utf16()
            .count() as u32,
    )
}

fn range_for_span(text: &str, start: usize, end: usize) -> Range {
    Range::new(
        position_at_offset(text, start),
        position_at_offset(text, end),
    )
}

fn identifier_at(text: &str, offset: usize) -> Option<(usize, String)> {
    let bytes = text.as_bytes();
    let mut start = offset.min(bytes.len());
    while start > 0 && (bytes[start - 1].is_ascii_alphanumeric() || bytes[start - 1] == b'_') {
        start -= 1;
    }
    let mut end = offset.min(bytes.len());
    while end < bytes.len() && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_') {
        end += 1;
    }
    if start == end {
        None
    } else {
        Some((start, text[start..end].to_string()))
    }
}

fn matching_delimiter(text: &str, open: usize, opening: u8, closing: u8) -> Option<usize> {
    let bytes = text.as_bytes();
    if bytes.get(open) != Some(&opening) {
        return None;
    }
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().skip(open) {
        if *byte == opening {
            depth += 1;
        } else if *byte == closing {
            depth = depth.checked_sub(1)?;
            if depth == 0 {
                return Some(index);
            }
        }
    }
    None
}

fn parse_parameters(
    params: &str,
    base_offset: usize,
    full_text: &str,
    uri: &Url,
    package: &str,
    imports: &HashMap<String, String>,
) -> Vec<JavaParameter> {
    let annotation_re = Regex::new(r"@[A-Za-z_$][\w$]*(?:\s*\([^)]*\))?\s*").unwrap();
    let modifier_re = Regex::new(r"\b(?:final|volatile|transient)\b\s*").unwrap();
    let alias_re = Regex::new(r#"@Param\s*\(\s*[\"']([^\"']+)[\"']\s*\)"#).unwrap();
    split_parameter_spans(params)
        .into_iter()
        .filter_map(|(start, end)| {
            let fragment = &params[start..end];
            let mut name_end = fragment.len();
            while name_end > 0 && fragment.as_bytes()[name_end - 1].is_ascii_whitespace() {
                name_end -= 1;
            }
            let mut name_start = name_end;
            while name_start > 0
                && (fragment.as_bytes()[name_start - 1].is_ascii_alphanumeric()
                    || fragment.as_bytes()[name_start - 1] == b'_'
                    || fragment.as_bytes()[name_start - 1] == b'$')
            {
                name_start -= 1;
            }
            if name_start == name_end {
                return None;
            }
            let name = fragment[name_start..name_end].to_string();
            let raw_type = modifier_re
                .replace_all(&annotation_re.replace_all(&fragment[..name_start], " "), " ")
                .trim()
                .to_string();
            if raw_type.is_empty() {
                return None;
            }
            let type_name = resolve_parameter_type(&raw_type, package, imports);
            let name_absolute = base_offset + start + name_start;
            Some(JavaParameter {
                name,
                alias: alias_re.captures(fragment).map(|capture| capture[1].to_string()),
                type_name,
                location: SymbolLocation {
                    uri: uri.clone(),
                    range: range_for_span(full_text, name_absolute, name_absolute + name_end - name_start),
                },
            })
        })
        .collect()
}

fn resolve_parameter_type(raw_type: &str, package: &str, imports: &HashMap<String, String>) -> String {
    let normalized = normalize_type_name(raw_type);
    if let Some(full) = imports.get(&normalized) {
        return full.clone();
    }
    if normalized.contains('.') || package.is_empty() {
        normalized
    } else {
        format!("{package}.{normalized}")
    }
}

fn normalize_type_name(type_name: &str) -> String {
    let compact = type_name.split_whitespace().collect::<String>();
    let without_generics = compact.split('<').next().unwrap_or(&compact);
    without_generics
        .trim_start_matches("?extends")
        .trim_start_matches("?super")
        .trim_end_matches("...")
        .trim_end_matches("[]")
        .to_string()
}

fn split_parameter_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    let mut angle_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut bracket_depth = 0usize;
    for (index, character) in text.char_indices() {
        match character {
            '<' => angle_depth += 1,
            '>' => angle_depth = angle_depth.saturating_sub(1),
            '(' => paren_depth += 1,
            ')' => paren_depth = paren_depth.saturating_sub(1),
            '[' => bracket_depth += 1,
            ']' => bracket_depth = bracket_depth.saturating_sub(1),
            ',' if angle_depth == 0 && paren_depth == 0 && bracket_depth == 0 => {
                spans.push((start, index));
                start = index + character.len_utf8();
            }
            _ => {}
        }
    }
    if start < text.len() {
        spans.push((start, text.len()));
    }
    spans
}

fn xml_parameter_at(text: &str, offset: usize) -> Option<String> {
    let parameter_re = Regex::new(r"#\{\s*([A-Za-z_]\w*(?:\.[A-Za-z_]\w*)?)").unwrap();
    if let Some(result) = parameter_re.captures_iter(text).find_map(|capture| {
        let whole = capture.get(0).unwrap();
        (whole.start() <= offset && offset <= whole.end()).then(|| capture[1].to_string())
    }) {
        return Some(result);
    }

    let test_re = Regex::new(r#"\btest\s*=\s*[\"']([^\"']+)[\"']"#).unwrap();
    for capture in test_re.captures_iter(text) {
        let expression = capture.get(1).unwrap();
        if expression.start() <= offset && offset <= expression.end() {
            return identifier_at(text, offset).map(|(_, name)| name);
        }
    }
    None
}

/// Returns the `refid` attribute value under the cursor.
fn xml_refid_at(text: &str, offset: usize) -> Option<String> {
    let refid_re = Regex::new(r#"\brefid\s*=\s*[\"']([^\"']+)[\"']"#).unwrap();
    let refid = refid_re.captures_iter(text).find_map(|capture| {
        let whole = capture.get(0).unwrap();
        (whole.start() <= offset && offset <= whole.end()).then(|| capture[1].trim().to_string())
    });
    refid
}

/// Returns the namespace of the mapper in this XML document.
fn xml_namespace(text: &str) -> Option<String> {
    Regex::new(r#"(?s)<mapper\b[^>]*?\bnamespace\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#)
        .unwrap()
        .captures(text)
        .map(|capture| capture[1].to_string())
}

fn xml_attribute(tag: &str, name: &str) -> Option<String> {
    let pattern = Regex::new(&format!(
        r#"(?s)\b{}\s*=\s*[\"']([^\"']+)[\"']"#,
        regex::escape(name)
    ))
    .ok()?;
    pattern.captures(tag).map(|capture| capture[1].to_string())
}

fn xml_statement_symbol_at(text: &str, offset: usize) -> Option<(String, String)> {
    let opening_re = Regex::new(
        r#"(?s)<(select|insert|update|delete|sql)\b[^>]*?\bid\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#,
    )
    .unwrap();
    for capture in opening_re.captures_iter(text) {
        let opening = capture.get(0).unwrap();
        if opening.start() > offset {
            break;
        }
        let tag_name = capture.get(1).unwrap().as_str();
        let closing_re = Regex::new(&format!(r"(?is)</{}\s*>", tag_name)).unwrap();
        let end = closing_re
            .find_at(text, opening.end())
            .map(|match_| match_.end())
            .unwrap_or(text.len());
        if offset <= end {
            let namespace = Regex::new(
                r#"(?s)<mapper\b[^>]*?\bnamespace\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#,
            )
            .unwrap()
            .captures_iter(&text[..opening.start()])
            .last()
            .map(|capture| capture[1].to_string())?;
            return Some((namespace, capture[2].to_string()));
        }
    }
    None
}

fn receiver_type_matches(
    text: &str,
    receiver: &str,
    namespace: &str,
    types: &HashMap<String, String>,
) -> bool {
    let pattern = Regex::new(&format!(
        r"(?m)\b([A-Za-z_]\w*)\s+{}\b",
        regex::escape(receiver)
    ))
    .unwrap();
    let Some(capture) = pattern.captures(text) else {
        return true;
    };
    let declared = &capture[1];
    types
        .get(declared)
        .map(|v| v == namespace || v.rsplit('.').next() == namespace.rsplit('.').next())
        .unwrap_or_else(|| declared == namespace.rsplit('.').next().unwrap_or(namespace))
}

fn xml_symbol_at(text: &str, offset: usize) -> Option<(String, String)> {
    let tag_re = Regex::new(
        r#"(?s)<(select|insert|update|delete|sql)\b[^>]*?\bid\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#,
    )
    .unwrap();
    let mapper_re =
        Regex::new(r#"(?s)<mapper\b[^>]*?\bnamespace\s*=\s*[\"']([^\"']+)[\"'][^>]*>"#).unwrap();
    let mapper = mapper_re
        .captures_iter(text)
        .filter(|c| c.get(0).unwrap().start() <= offset)
        .last()?;
    let statement = tag_re.captures_iter(text).find(|c| {
        let range = c.get(0).unwrap().range();
        range.start <= offset && offset <= range.end
    })?;
    Some((mapper[1].to_string(), statement[2].to_string()))
}

struct Backend {
    client: Client,
    index: Arc<RwLock<WorkspaceIndex>>,
}

#[async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> std::result::Result<InitializeResult, tower_lsp::jsonrpc::Error> {
        let root_uri = params
            .workspace_folders
            .and_then(|folders| folders.into_iter().next().map(|folder| folder.uri))
            .or(params.root_uri);
        if let Some(root_uri) = root_uri {
            if let Ok(root) = root_uri.to_file_path() {
                self.index.write().await.rebuild(root);
            }
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Kind(
                    TextDocumentSyncKind::FULL,
                )),
                ..ServerCapabilities::default()
            },
            server_info: None,
        })
    }

    async fn initialized(&self, _: tower_lsp::lsp_types::InitializedParams) {
        let _ = self
            .client
            .log_message(
                tower_lsp::lsp_types::MessageType::INFO,
                "MyBatis LSP initialized",
            )
            .await;
    }

    async fn shutdown(&self) -> std::result::Result<(), tower_lsp::jsonrpc::Error> {
        Ok(())
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.index
            .write()
            .await
            .update_document(params.text_document.uri, params.text_document.text);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        if let Some(change) = params.content_changes.into_iter().last() {
            self.index
                .write()
                .await
                .update_document(params.text_document.uri, change.text);
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> std::result::Result<Option<GotoDefinitionResponse>, tower_lsp::jsonrpc::Error> {
        let uri = params.text_document_position_params.text_document.uri;
        let position = params.text_document_position_params.position;
        let index = self.index.read().await;
        let location = match uri.path().rsplit('.').next() {
            Some("xml") => index.xml_definition(&uri, position),
            _ => index.java_definition(&uri, position),
        };
        Ok(location.map(GotoDefinitionResponse::Scalar))
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> std::result::Result<Option<Vec<Location>>, tower_lsp::jsonrpc::Error> {
        let uri = params.text_document_position.text_document.uri;
        if !uri.path().ends_with(".xml") {
            return Ok(None);
        }
        let position = params.text_document_position.position;
        let index = self.index.read().await;
        let locations = index.xml_references(&uri, position, params.context.include_declaration);
        Ok((!locations.is_empty()).then_some(locations))
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();
    let (service, socket) = LspService::new(|client| Backend {
        client,
        index: Arc::new(RwLock::new(WorkspaceIndex::default())),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_mapper_java_and_xml() {
        let mut index = WorkspaceIndex::default();
        let java_uri = Url::parse("file:///C:/Mapper.java").unwrap();
        let xml_uri = Url::parse("file:///C:/Mapper.xml").unwrap();
        index.update_document(
            java_uri.clone(),
            "package demo;\npublic interface UserMapper {\n User findById(Long id);\n}".into(),
        );
        index.update_document(xml_uri.clone(), "<mapper namespace=\"demo.UserMapper\">\n<select id=\"findById\">select 1</select>\n</mapper>".into());
        let java = index
            .java_methods
            .get(&MapperKey {
                namespace: "demo.UserMapper".into(),
                id: "findById".into(),
            })
            .unwrap();
        assert_eq!(java.uri, java_uri);
        let xml = index
            .xml_statements
            .get(&MapperKey {
                namespace: "demo.UserMapper".into(),
                id: "findById".into(),
            })
            .unwrap();
        assert_eq!(xml.uri, xml_uri);
    }

    #[test]
    fn resolves_java_call_to_xml() {
        let mut index = WorkspaceIndex::default();
        let mapper_uri = Url::parse("file:///C:/UserMapper.java").unwrap();
        let service_uri = Url::parse("file:///C:/UserService.java").unwrap();
        let xml_uri = Url::parse("file:///C:/UserMapper.xml").unwrap();
        index.update_document(
            mapper_uri,
            "package demo;\npublic interface UserMapper {\n User findById(Long id);\n}".into(),
        );
        let service_text = "package demo;\nclass UserService {\n UserMapper mapper;\n void load() { mapper.findById(1L); }\n";
        index.update_document(service_uri.clone(), service_text.into());
        index.update_document(xml_uri.clone(), "<mapper namespace=\"demo.UserMapper\">\n<select id=\"findById\">select 1</select>\n</mapper>".into());
        let call_offset = service_text.find("findById").unwrap() + 2;
        let location = index
            .java_definition(&service_uri, position_at_offset(service_text, call_offset))
            .unwrap();
        assert_eq!(location.uri, xml_uri);
    }

    #[test]
    fn resolves_xml_to_java() {
        let mut index = WorkspaceIndex::default();
        let java_uri = Url::parse("file:///C:/UserMapper.java").unwrap();
        let xml_uri = Url::parse("file:///C:/UserMapper.xml").unwrap();
        index.update_document(
            java_uri.clone(),
            "package demo;\npublic interface UserMapper {\n User findById(Long id);\n}".into(),
        );
        index.update_document(xml_uri.clone(), "<mapper namespace=\"demo.UserMapper\">\n<select id=\"findById\">select 1</select>\n</mapper>".into());
        let location = index
            .xml_definition(&xml_uri, Position::new(1, 14))
            .unwrap();
        assert_eq!(location.uri, java_uri);
    }

    #[test]
    fn converts_utf16_positions() {
        let text = "中文😀mapper.findById();";
        let method_offset = text.find("findById").unwrap();
        let position = position_at_offset(text, method_offset);
        assert_eq!(position.character, 11);
        assert_eq!(offset_at_position(text, position), method_offset);
    }

    #[test]
    fn resolves_xml_parameter_to_entity_field() {
        let mut index = WorkspaceIndex::default();
        let entity_uri = Url::parse("file:///C:/User.java").unwrap();
        let mapper_uri = Url::parse("file:///C:/UserMapper.java").unwrap();
        let xml_uri = Url::parse("file:///C:/UserMapper.xml").unwrap();
        index.update_document(
            entity_uri.clone(),
            "package demo;\npublic class User {\n private String name;\n}".into(),
        );
        index.update_document(
            mapper_uri,
            "package demo;\npublic interface UserMapper {\n User find(User user);\n}".into(),
        );
        let xml = "<mapper namespace=\"demo.UserMapper\">\n<select id=\"find\">select * from user where name = #{name}</select>\n</mapper>";
        index.update_document(xml_uri.clone(), xml.into());
        let offset = xml.find("name}</select>").unwrap();
        let location = index
            .xml_definition(&xml_uri, position_at_offset(xml, offset))
            .unwrap();
        assert_eq!(location.uri, entity_uri);
        let entity = "package demo;\npublic class User {\n private String name;\n}";
        assert_eq!(location.range.start, position_at_offset(entity, entity.find("name").unwrap()));
    }

    #[test]
    fn resolves_nested_parameter_type_and_included_sql_field() {
        let mut index = WorkspaceIndex::default();
        let request_uri = Url::parse("file:///C:/LaunchCombineRequest.java").unwrap();
        let mapper_uri = Url::parse("file:///C:/LaunchCombineMapper.java").unwrap();
        let xml_uri = Url::parse("file:///C:/LaunchCombineMapper.xml").unwrap();
        let request = "package demo;\npublic class LaunchCombineRequest {\n    public static class ListSelectReq {\n        private String flowType;\n    }\n}";
        index.update_document(request_uri.clone(), request.into());
        index.update_document(
            mapper_uri,
            "package demo;\npublic interface LaunchCombineMapper {\n    void listSelect(LaunchCombineRequest.ListSelectReq request);\n}".into(),
        );
        let xml = "<mapper namespace=\"demo.LaunchCombineMapper\">\n<sql id=\"adminCondition\"><if test=\"flowType != null\">and flow_type_ = #{flowType}</if></sql>\n<select id=\"listSelect\" parameterType=\"demo.LaunchCombineRequest$ListSelectReq\"><include refid=\"adminCondition\"/></select>\n</mapper>";
        index.update_document(xml_uri.clone(), xml.into());
        let offset = xml.find("flowType}").unwrap();
        let location = index
            .xml_definition(&xml_uri, position_at_offset(xml, offset))
            .unwrap();
        assert_eq!(location.uri, request_uri);
        assert_eq!(location.range.start, position_at_offset(request, request.find("flowType").unwrap()));
        let test_offset = xml.find("flowType !=").unwrap();
        let test_location = index
            .xml_definition(&xml_uri, position_at_offset(xml, test_offset))
            .unwrap();
        assert_eq!(test_location.uri, request_uri);
    }

    #[test]
    fn resolves_include_refid_to_sql() {
        let mut index = WorkspaceIndex::default();
        let xml_uri = Url::parse("file:///C:/UserMapper.xml").unwrap();
        let other_uri = Url::parse("file:///C:/CommonMapper.xml").unwrap();
        let other = "<mapper namespace=\"demo.CommonMapper\">\n<sql id=\"columns\">id, name</sql>\n</mapper>";
        index.update_document(other_uri.clone(), other.into());
        let xml = "<mapper namespace=\"demo.UserMapper\">\n<sql id=\"base\">select 1</sql>\n<select id=\"find\">\n<if test=\"a\"><include refid=\"base\" /></if>\n<include refid=\"demo.CommonMapper.columns\"/>\n</select>\n</mapper>";
        index.update_document(xml_uri.clone(), xml.into());

        let local = index
            .xml_definition(&xml_uri, position_at_offset(xml, xml.find("base\" /").unwrap() + 1))
            .unwrap();
        assert_eq!(local.uri, xml_uri);
        assert_eq!(local.range.start, position_at_offset(xml, xml.find("base\">").unwrap()));

        let qualified = index
            .xml_definition(&xml_uri, position_at_offset(xml, xml.find("CommonMapper.columns").unwrap()))
            .unwrap();
        assert_eq!(qualified.uri, other_uri);
        assert_eq!(qualified.range.start, position_at_offset(other, other.find("columns").unwrap()));
    }

    #[test]
    fn finds_include_references_of_sql() {
        let mut index = WorkspaceIndex::default();
        let xml_uri = Url::parse("file:///C:/UserMapper.xml").unwrap();
        let other_uri = Url::parse("file:///C:/OrderMapper.xml").unwrap();
        let xml = "<mapper namespace=\"demo.UserMapper\">
<sql id=\"base\">select 1</sql>
<select id=\"a\"><include refid=\"base\"/></select>
<select id=\"b\"><if test=\"x\"><include refid=\"base\" /></if></select>
</mapper>";
        let other = "<mapper namespace=\"demo.OrderMapper\">
<sql id=\"base\">select 2</sql>
<select id=\"c\"><include refid=\"base\"/><include refid=\"demo.UserMapper.base\"/></select>
</mapper>";
        index.update_document(xml_uri.clone(), xml.into());
        index.update_document(other_uri.clone(), other.into());

        let on_sql = position_at_offset(xml, xml.find("base\">select").unwrap() + 1);
        let refs = index.xml_references(&xml_uri, on_sql, false);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs.iter().filter(|l| l.uri == other_uri).count(), 1);

        let on_include = position_at_offset(xml, xml.find("base\"/>").unwrap());
        let with_decl = index.xml_references(&xml_uri, on_include, true);
        assert_eq!(with_decl.len(), 4);
        assert_eq!(with_decl[0].range.start, position_at_offset(xml, xml.find("base\">select").unwrap()));
    }
}

