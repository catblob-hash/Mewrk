//! A Jupyter notebook, read into cells the viewer can draw (upstream's `lib/notebook.ts`). Only what a reader sees is
//! kept. nbformat 4 is the format in use; nbformat 3's worksheets are flattened so an old notebook still opens.

use serde_json::{Map, Value};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub enum NotebookOutput {
    Stream { name: String, text: String },
    Data { execution_count: Option<f64>, data: BTreeMap<String, String>, result: bool },
    Error { name: String, value: String, traceback: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum NotebookCell {
    Markdown { source: String, attachments: BTreeMap<String, BTreeMap<String, String>> },
    Code { source: String, execution_count: Option<f64>, outputs: Vec<NotebookOutput> },
    Raw { source: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Notebook {
    pub language: String,
    pub cells: Vec<NotebookCell>,
}

/// nbformat stores multi-line strings as arrays of lines, each with its own newline.
fn joined(value: Option<&Value>) -> String {
    match value {
        Some(Value::Array(parts)) => parts.iter().filter_map(Value::as_str).collect(),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

fn empty() -> &'static Map<String, Value> {
    static EMPTY: std::sync::OnceLock<Map<String, Value>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(Map::new)
}

fn record(value: Option<&Value>) -> &Map<String, Value> {
    value.and_then(Value::as_object).unwrap_or(empty())
}

fn mime_bundle(value: Option<&Value>) -> BTreeMap<String, String> {
    record(value)
        .iter()
        .map(|(kind, content)| {
            let text = if content.is_object() { serde_json::to_string_pretty(content).unwrap_or_default() } else { joined(Some(content)) };
            (kind.clone(), text)
        })
        .collect()
}

fn output_of(raw: &Value, v3_display: bool) -> Option<NotebookOutput> {
    let output = raw.as_object()?;
    let kind = output.get("output_type").and_then(Value::as_str)?;
    Some(match kind {
        "stream" => NotebookOutput::Stream { name: output.get("name").and_then(Value::as_str).unwrap_or("stdout").to_owned(), text: joined(output.get("text")) },
        "execute_result" => NotebookOutput::Data { execution_count: output.get("execution_count").and_then(Value::as_f64), data: mime_bundle(output.get("data")), result: true },
        "display_data" if !v3_display => NotebookOutput::Data { execution_count: output.get("execution_count").and_then(Value::as_f64), data: mime_bundle(output.get("data")), result: false },
        // nbformat 3 kept each MIME type as its own key on the output itself.
        "pyout" | "display_data" => {
            let mut data = BTreeMap::new();
            for (key, value) in output {
                if matches!(key.as_str(), "output_type" | "prompt_number" | "metadata") {
                    continue;
                }
                let kind = match key.as_str() {
                    "text" => "text/plain",
                    "html" => "text/html",
                    "png" => "image/png",
                    "jpeg" => "image/jpeg",
                    "svg" => "image/svg+xml",
                    "latex" => "text/latex",
                    "json" => "application/json",
                    "javascript" => "application/javascript",
                    other => other,
                };
                data.insert(kind.to_owned(), joined(Some(value)));
            }
            NotebookOutput::Data { execution_count: output.get("prompt_number").and_then(Value::as_f64), data, result: true }
        }
        "error" | "pyerr" => NotebookOutput::Error {
            name: output.get("ename").and_then(Value::as_str).unwrap_or("Error").to_owned(),
            value: output.get("evalue").and_then(Value::as_str).unwrap_or("").to_owned(),
            traceback: output.get("traceback").and_then(Value::as_array).map_or(String::new(), |t| t.iter().map(|l| l.as_str().map_or_else(|| l.to_string(), str::to_owned)).collect::<Vec<_>>().join("\n")),
        },
        _ => return None,
    })
}

fn cell_of(raw: &Value) -> Option<NotebookCell> {
    let cell = raw.as_object()?;
    let source = joined(cell.get("source").or_else(|| cell.get("input")));
    Some(match cell.get("cell_type").and_then(Value::as_str)? {
        "markdown" => {
            let attachments = record(cell.get("attachments")).iter().map(|(name, bundle)| (name.clone(), record(Some(bundle)).iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_owned())).collect())).collect();
            NotebookCell::Markdown { source, attachments }
        }
        "heading" => {
            let level = cell.get("level").and_then(Value::as_f64).unwrap_or(1.0).clamp(1.0, 6.0) as usize;
            NotebookCell::Markdown { source: format!("{} {source}", "#".repeat(level)), attachments: BTreeMap::new() }
        }
        "code" => NotebookCell::Code {
            source,
            execution_count: cell.get("execution_count").filter(|v| !v.is_null()).or_else(|| cell.get("prompt_number")).and_then(Value::as_f64),
            outputs: cell.get("outputs").and_then(Value::as_array).map_or(Vec::new(), |o| {
                o.iter().filter_map(|output| {
                    let v3 = output.get("output_type").and_then(Value::as_str) == Some("display_data") && output.get("data").is_none();
                    output_of(output, v3)
                }).collect()
            }),
        },
        "raw" => NotebookCell::Raw { source },
        _ => return None,
    })
}

pub fn parse_notebook(text: &str) -> Result<Notebook, String> {
    let document: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let metadata = record(document.get("metadata"));
    let language = record(metadata.get("kernelspec")).get("language").filter(|v| !v.is_null())
        .or_else(|| record(metadata.get("language_info")).get("name").filter(|v| !v.is_null()))
        .or_else(|| metadata.get("language").filter(|v| !v.is_null()))
        .and_then(Value::as_str).unwrap_or("python").to_lowercase();
    let raw: Vec<&Value> = if let Some(cells) = document.get("cells").and_then(Value::as_array) {
        cells.iter().collect()
    } else if let Some(sheets) = document.get("worksheets").and_then(Value::as_array) {
        sheets.iter().filter_map(|s| s.get("cells").and_then(Value::as_array)).flatten().collect()
    } else {
        Vec::new()
    };
    Ok(Notebook { language, cells: raw.into_iter().filter_map(cell_of).collect() })
}

/// The first of these a bundle has is the one shown: the richest form the viewer can draw.
pub const OUTPUT_PREFERENCE: &[&str] = &["image/png", "image/jpeg", "image/gif", "image/webp", "image/svg+xml", "text/html", "text/markdown", "text/latex", "application/json", "text/plain"];

/// The Markdown a notebook's text cells hold, for the pictures they reference.
pub fn notebook_markdown(content: &str) -> String {
    let Ok(notebook) = parse_notebook(content) else { return String::new() };
    notebook.cells.iter().filter_map(|c| if let NotebookCell::Markdown { source, .. } = c { Some(source.as_str()) } else { None }).collect::<Vec<_>>().join("\n\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_nbformat_4_cells_joining_line_arrays() {
        let notebook = parse_notebook(&json!({
            "nbformat": 4, "metadata": { "language_info": { "name": "R" } },
            "cells": [
                { "cell_type": "markdown", "source": ["# T\n", "x"], "attachments": { "a.png": { "image/png": "AA" } } },
                { "cell_type": "code", "execution_count": 1, "source": ["1 + 1"], "outputs": [
                    { "output_type": "execute_result", "execution_count": 1, "data": { "text/plain": ["2"], "application/json": { "a": 1 } } },
                    { "output_type": "stream", "name": "stderr", "text": "warn\n" }
                ] },
                { "cell_type": "raw", "source": "raw" },
                { "cell_type": "unknown", "source": "?" }
            ]
        }).to_string()).unwrap();
        assert_eq!(notebook.language, "r");
        assert_eq!(notebook.cells.len(), 3);
        let NotebookCell::Markdown { source, attachments } = &notebook.cells[0] else { panic!() };
        assert_eq!(source, "# T\nx");
        assert_eq!(attachments["a.png"]["image/png"], "AA");
        let NotebookCell::Code { outputs, execution_count, .. } = &notebook.cells[1] else { panic!() };
        assert_eq!(*execution_count, Some(1.0));
        let NotebookOutput::Data { data, result, .. } = &outputs[0] else { panic!() };
        assert!(*result);
        assert_eq!(data["text/plain"], "2");
        assert_eq!(data["application/json"], "{\n  \"a\": 1\n}");
        assert_eq!(outputs[1], NotebookOutput::Stream { name: "stderr".into(), text: "warn\n".into() });
        assert_eq!(notebook.cells[2], NotebookCell::Raw { source: "raw".into() });
    }

    #[test]
    fn flattens_nbformat_3_worksheets() {
        let notebook = parse_notebook(&json!({
            "nbformat": 3,
            "worksheets": [{ "cells": [
                { "cell_type": "heading", "level": 2, "source": "Old" },
                { "cell_type": "code", "input": "x", "prompt_number": 5, "outputs": [
                    { "output_type": "pyout", "prompt_number": 5, "text": ["1"], "png": "AA" },
                    { "output_type": "pyerr", "ename": "E", "evalue": "v", "traceback": ["t1", "t2"] }
                ] }
            ] }]
        }).to_string()).unwrap();
        assert_eq!(notebook.language, "python");
        assert_eq!(notebook.cells[0], NotebookCell::Markdown { source: "## Old".into(), attachments: BTreeMap::new() });
        let NotebookCell::Code { source, execution_count, outputs } = &notebook.cells[1] else { panic!() };
        assert_eq!((source.as_str(), *execution_count), ("x", Some(5.0)));
        let NotebookOutput::Data { data, result: true, execution_count: Some(c) } = &outputs[0] else { panic!() };
        assert_eq!(*c, 5.0);
        assert_eq!(data["text/plain"], "1");
        assert_eq!(data["image/png"], "AA");
        assert_eq!(outputs[1], NotebookOutput::Error { name: "E".into(), value: "v".into(), traceback: "t1\nt2".into() });
    }

    #[test]
    fn errors_on_text_that_is_not_json_for_the_viewer_to_report() {
        assert!(parse_notebook("{nope").is_err());
    }
}
