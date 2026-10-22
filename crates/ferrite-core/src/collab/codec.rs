//! Conversions between file content (open formats on disk) and live CRDT documents.
//!
//! Files stay in plain formats — Markdown/text, CSV, and small JSON documents for mind maps
//! and canvases — so they remain useful to any app, any AI and any sync client. A CRDT document
//! only exists while someone is editing; it is seeded from the file and written back to it.
//!
//! The shared-type layout here is mirrored exactly by the browser editors (`web/js/collab.js`):
//!
//! | kind    | root types                                                        |
//! |---------|-------------------------------------------------------------------|
//! | text    | `Y.Text "content"`                                                 |
//! | sheet   | `Y.Map "cells"`: `"row,col"` → string (formulas start with `=`)   |
//! | mindmap | `Y.Map "meta"`, `Y.Map "nodes"`: id → `Y.Map` of flat fields       |
//! | canvas  | `Y.Map "meta"`, `Y.Map "items"` and `Y.Map "edges"`: id → `Y.Map` |

use super::DocKind;
use crate::{Error, Result};
use serde_json::{Map as JsonMap, Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use yrs::types::ToJson;
use yrs::{Any, Doc, GetString, Map, MapPrelim, MapRef, Out, ReadTxn, Text, Transact, TransactionMut};

pub const MINDMAP_FORMAT: &str = "ferrite.mindmap";
pub const CANVAS_FORMAT: &str = "ferrite.canvas";

pub fn json_to_any(v: &Value) -> Any {
    match v {
        Value::Null => Any::Null,
        Value::Bool(b) => Any::Bool(*b),
        Value::Number(n) => Any::Number(n.as_f64().unwrap_or(0.0).into()),
        Value::String(s) => Any::String(Arc::from(s.as_str())),
        Value::Array(a) => Any::Array(a.iter().map(json_to_any).collect::<Vec<_>>().into()),
        Value::Object(o) => Any::Map(Arc::new(o.iter().map(|(k, v)| (k.clone(), json_to_any(v))).collect::<HashMap<_, _>>())),
    }
}

pub fn any_to_json(a: &Any) -> Value {
    match a {
        Any::Null | Any::Undefined => Value::Null,
        Any::Bool(b) => json!(b),
        Any::Number(n) => {
            let f = n.as_f64().unwrap_or(0.0);
            if f.fract() == 0.0 && f.abs() < 9.0e15 { json!(f as i64) } else { json!(f) }
        }
        Any::String(s) => json!(s.as_ref()),
        Any::Buffer(b) => json!(b.as_ref()),
        Any::Array(a) => Value::Array(a.iter().map(any_to_json).collect()),
        Any::Map(m) => Value::Object(m.iter().map(|(k, v)| (k.clone(), any_to_json(v))).collect()),
    }
}

/// Inserts a flat JSON object as a nested `Y.Map` under `key`.
pub fn put_object(txn: &mut TransactionMut, parent: &MapRef, key: &str, obj: &JsonMap<String, Value>) -> MapRef {
    let prelim: MapPrelim = obj.iter().filter(|(k, _)| k.as_str() != "id").map(|(k, v)| (k.clone(), json_to_any(v))).collect();
    parent.insert(txn, key, prelim)
}

/// Reads every nested map of `parent` as `[{id, ...fields}]`, sorted by id for stable output.
pub fn read_objects<T: ReadTxn>(txn: &T, parent: &MapRef) -> Vec<Value> {
    let mut out: Vec<Value> = parent
        .iter(txn)
        .map(|(id, v)| {
            let mut obj = match v {
                Out::YMap(m) => match any_to_json(&m.to_json(txn)) {
                    Value::Object(o) => o,
                    _ => JsonMap::new(),
                },
                Out::Any(a) => match any_to_json(&a) {
                    Value::Object(o) => o,
                    _ => JsonMap::new(),
                },
                _ => JsonMap::new(),
            };
            obj.insert("id".into(), json!(id));
            Value::Object(obj)
        })
        .collect();
    out.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
    out
}

fn read_meta<T: ReadTxn>(txn: &T, meta: &MapRef) -> Value {
    any_to_json(&meta.to_json(txn))
}

/// Builds a CRDT document from file content.
pub fn load(kind: DocKind, data: &[u8]) -> Result<Doc> {
    let doc = Doc::new();
    match kind {
        DocKind::Text => {
            let text = doc.get_or_insert_text("content");
            let s = String::from_utf8_lossy(data);
            let mut txn = doc.transact_mut();
            text.insert(&mut txn, 0, &s);
        }
        DocKind::Sheet => {
            let cells = doc.get_or_insert_map("cells");
            let mut rdr = csv::ReaderBuilder::new().has_headers(false).flexible(true).from_reader(data);
            let mut txn = doc.transact_mut();
            for (r, rec) in rdr.records().enumerate() {
                let rec = rec.map_err(|e| Error::bad(format!("invalid CSV: {e}")))?;
                for (c, field) in rec.iter().enumerate() {
                    if !field.is_empty() {
                        cells.insert(&mut txn, format!("{r},{c}"), field);
                    }
                }
            }
        }
        DocKind::Mindmap | DocKind::Canvas => {
            let v: Value = if data.iter().all(u8::is_ascii_whitespace) { kind.empty_json() } else { serde_json::from_slice(data)? };
            let meta_ref = doc.get_or_insert_map("meta");
            let (coll_a, coll_b) = match kind {
                DocKind::Mindmap => ("nodes", None),
                _ => ("items", Some("edges")),
            };
            let a = doc.get_or_insert_map(coll_a);
            let b = coll_b.map(|n| doc.get_or_insert_map(n));
            let mut txn = doc.transact_mut();
            if let Some(m) = v.get("meta").and_then(Value::as_object) {
                for (k, val) in m {
                    meta_ref.insert(&mut txn, k.as_str(), json_to_any(val));
                }
            }
            if let Some(title) = v.get("title").and_then(Value::as_str) {
                meta_ref.insert(&mut txn, "title", title);
            }
            for obj in v.get(coll_a).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
                if let Some(id) = obj.get("id").and_then(Value::as_str) {
                    put_object(&mut txn, &a, id, obj);
                }
            }
            if let (Some(name), Some(b)) = (coll_b, &b) {
                for obj in v.get(name).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_object) {
                    if let Some(id) = obj.get("id").and_then(Value::as_str) {
                        put_object(&mut txn, b, id, obj);
                    }
                }
            }
        }
    }
    Ok(doc)
}

/// Serializes a CRDT document back to file content.
pub fn save(kind: DocKind, doc: &Doc) -> Result<Vec<u8>> {
    match kind {
        DocKind::Text => {
            let text = doc.get_or_insert_text("content");
            let txn = doc.transact();
            Ok(text.get_string(&txn).into_bytes())
        }
        DocKind::Sheet => {
            let cells = doc.get_or_insert_map("cells");
            let txn = doc.transact();
            let mut grid: Vec<Vec<String>> = Vec::new();
            for (key, v) in cells.iter(&txn) {
                let Some((r, c)) = key.split_once(',') else { continue };
                let (Ok(r), Ok(c)) = (r.parse::<usize>(), c.parse::<usize>()) else { continue };
                if r > 100_000 || c > 1_000 {
                    continue;
                }
                let val = match v {
                    Out::Any(Any::String(s)) => s.to_string(),
                    Out::Any(a) => any_to_json(&a).to_string(),
                    other => other.to_string(&txn),
                };
                if val.is_empty() {
                    continue;
                }
                if grid.len() <= r {
                    grid.resize(r + 1, Vec::new());
                }
                if grid[r].len() <= c {
                    grid[r].resize(c + 1, String::new());
                }
                grid[r][c] = val;
            }
            let width = grid.iter().map(Vec::len).max().unwrap_or(0);
            let mut w = csv::WriterBuilder::new().has_headers(false).flexible(true).from_writer(Vec::new());
            for mut row in grid {
                row.resize(width, String::new());
                w.write_record(&row).map_err(|e| Error::internal(e))?;
            }
            w.into_inner().map_err(|e| Error::internal(e))
        }
        DocKind::Mindmap => {
            let meta = doc.get_or_insert_map("meta");
            let nodes = doc.get_or_insert_map("nodes");
            let txn = doc.transact();
            let meta_v = read_meta(&txn, &meta);
            let v = json!({
                "format": MINDMAP_FORMAT,
                "version": 1,
                "title": meta_v.get("title").cloned().unwrap_or(json!("")),
                "meta": meta_v,
                "nodes": read_objects(&txn, &nodes),
            });
            Ok(serde_json::to_vec_pretty(&v)?)
        }
        DocKind::Canvas => {
            let meta = doc.get_or_insert_map("meta");
            let items = doc.get_or_insert_map("items");
            let edges = doc.get_or_insert_map("edges");
            let txn = doc.transact();
            let meta_v = read_meta(&txn, &meta);
            let v = json!({
                "format": CANVAS_FORMAT,
                "version": 1,
                "title": meta_v.get("title").cloned().unwrap_or(json!("")),
                "meta": meta_v,
                "items": read_objects(&txn, &items),
                "edges": read_objects(&txn, &edges),
            });
            Ok(serde_json::to_vec_pretty(&v)?)
        }
    }
}

/// Renders a mind map as an indented Markdown outline (handy for AI and export).
pub fn mindmap_outline(v: &Value) -> String {
    let nodes = v["nodes"].as_array().cloned().unwrap_or_default();
    let mut children: HashMap<String, Vec<&Value>> = HashMap::new();
    for n in &nodes {
        children.entry(n["parent"].as_str().unwrap_or("").to_string()).or_default().push(n);
    }
    for list in children.values_mut() {
        list.sort_by(|a, b| a["order"].as_f64().unwrap_or(0.0).total_cmp(&b["order"].as_f64().unwrap_or(0.0)));
    }
    fn walk(id: &str, depth: usize, children: &HashMap<String, Vec<&Value>>, out: &mut String, seen: &mut Vec<String>) {
        if seen.iter().any(|s| s == id) || depth > 64 {
            return;
        }
        seen.push(id.to_string());
        for n in children.get(id).into_iter().flatten() {
            let nid = n["id"].as_str().unwrap_or("");
            out.push_str(&format!("{}- {}\n", "  ".repeat(depth), n["text"].as_str().unwrap_or("")));
            walk(nid, depth + 1, children, out, seen);
        }
    }
    let mut out = String::new();
    walk("", 0, &children, &mut out, &mut Vec::new());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        let doc = load(DocKind::Text, "# Hello\nworld ✓".as_bytes()).unwrap();
        assert_eq!(save(DocKind::Text, &doc).unwrap(), "# Hello\nworld ✓".as_bytes());
    }

    #[test]
    fn sheet_round_trip() {
        let csv = "a,b,c\n1,,=SUM(A2:B2)\n\"x,y\",2,\n";
        let doc = load(DocKind::Sheet, csv.as_bytes()).unwrap();
        let out = String::from_utf8(save(DocKind::Sheet, &doc).unwrap()).unwrap();
        assert_eq!(out, "a,b,c\n1,,=SUM(A2:B2)\n\"x,y\",2,\n");
    }

    #[test]
    fn mindmap_round_trip_and_outline() {
        let src = json!({"format": MINDMAP_FORMAT, "version": 1, "title": "Plan", "nodes": [
            {"id": "root", "parent": "", "text": "Plan", "order": 0},
            {"id": "b", "parent": "root", "text": "Second", "order": 2},
            {"id": "a", "parent": "root", "text": "First", "order": 1, "color": "#f00"},
        ]});
        let doc = load(DocKind::Mindmap, src.to_string().as_bytes()).unwrap();
        let out: Value = serde_json::from_slice(&save(DocKind::Mindmap, &doc).unwrap()).unwrap();
        assert_eq!(out["title"], "Plan");
        assert_eq!(out["nodes"].as_array().unwrap().len(), 3);
        assert_eq!(out["nodes"][0]["id"], "a");
        assert_eq!(out["nodes"][0]["color"], "#f00");
        assert_eq!(mindmap_outline(&out), "- Plan\n  - First\n  - Second\n");
    }

    #[test]
    fn canvas_round_trip() {
        let src = json!({"format": CANVAS_FORMAT, "items": [{"id": "n1", "type": "note", "x": 10, "y": 20.5, "text": "hi"}],
                         "edges": [{"id": "e1", "from": "n1", "to": "n2"}]});
        let doc = load(DocKind::Canvas, src.to_string().as_bytes()).unwrap();
        let out: Value = serde_json::from_slice(&save(DocKind::Canvas, &doc).unwrap()).unwrap();
        assert_eq!(out["items"][0]["y"], 20.5);
        assert_eq!(out["items"][0]["x"], 10);
        assert_eq!(out["edges"][0]["to"], "n2");
    }
}
