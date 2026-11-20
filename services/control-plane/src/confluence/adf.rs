//! Render a bounded Atlassian Document Format tree as inert text. Do not resolve links, macros or media.
use super::{Error, LIMIT};
use serde_json::Value;
use std::collections::BTreeSet;

pub fn text(body: &Value) -> Result<(String, BTreeSet<String>), Error> {
    if body["representation"] != "atlas_doc_format" {
        return Err(Error::Protocol);
    }
    let raw = body["value"].as_str().ok_or(Error::Protocol)?;
    if raw.len() > LIMIT {
        return Err(Error::Limit);
    }
    let doc: Value = serde_json::from_str(raw).map_err(|_| Error::Protocol)?;
    if doc["type"] != "doc" || doc["version"] != 1 {
        return Err(Error::Protocol);
    }
    let mut result = Projection::default();
    result.visit(&doc, 0)?;
    Ok((result.text, result.omitted))
}
#[derive(Default)]
struct Projection {
    text: String,
    omitted: BTreeSet<String>,
    nodes: usize,
}
impl Projection {
    fn append(&mut self, s: &str) -> Result<(), Error> {
        if self.text.len() + s.len() > LIMIT {
            return Err(Error::Limit);
        }
        self.text.push_str(s);
        Ok(())
    }
    fn visit(&mut self, node: &Value, depth: usize) -> Result<(), Error> {
        self.nodes += 1;
        if self.nodes > 16384 || depth > 32 {
            return Err(Error::Limit);
        }
        let kind = node["type"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)
            .ok_or(Error::Protocol)?;
        match kind {
            "text" => return self.append(node["text"].as_str().ok_or(Error::Protocol)?),
            "hardBreak" => return self.append("\n"),
            "rule" => return self.append("\n—\n"),
            "mention" => return self.append("[mention]"),
            "emoji" => {
                return self.append(node["attrs"]["shortName"].as_str().ok_or(Error::Protocol)?);
            }
            "doc" | "paragraph" | "heading" | "bulletList" | "orderedList" | "listItem"
            | "codeBlock" | "blockquote" | "table" | "tableRow" | "tableCell" | "tableHeader"
            | "panel" | "expand" | "nestedExpand" | "layoutSection" | "layoutColumn"
            | "taskList" | "taskItem" => {}
            _ => {
                self.omitted.insert(kind.to_owned());
                return Ok(());
            }
        }
        if let Some(children) = node.get("content") {
            for child in children.as_array().ok_or(Error::Protocol)? {
                self.visit(child, depth + 1)?;
            }
        } else if kind == "doc" {
            return Err(Error::Protocol);
        }
        if matches!(kind, "paragraph" | "heading" | "codeBlock" | "tableRow") {
            self.append("\n")?;
        }
        Ok(())
    }
}
