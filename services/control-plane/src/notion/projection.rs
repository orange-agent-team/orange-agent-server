use super::*;

pub(super) fn identifier(v: &Value) -> Result<String, Error> {
    id(v["id"].as_str().ok_or(Error::Protocol)?).map_err(|_| Error::Protocol)
}
pub(super) fn object(v: &Value, kind: &str, expected: &str) -> Result<(), Error> {
    if v["object"] != kind || identifier(v)? != expected {
        return Err(Error::Changed);
    }
    match v["in_trash"].as_bool() {
        Some(false) => Ok(()),
        Some(true) => Err(Error::Missing),
        None => Err(Error::Protocol),
    }
}
pub(super) fn parent(v: &Value, kind: &str, expected: &str) -> Result<(), Error> {
    let p = &v["parent"];
    if p["type"] != kind || p[kind].as_str().and_then(|s| id(s).ok()).as_deref() != Some(expected) {
        return Err(Error::Changed);
    }
    Ok(())
}
pub(super) fn list(v: &Value, limit: u8) -> Result<(&[Value], Option<&str>), Error> {
    if v["object"] != "list" {
        return Err(Error::Protocol);
    }
    let data = v["results"].as_array().ok_or(Error::Protocol)?;
    if data.len() > usize::from(limit) {
        return Err(Error::Protocol);
    }
    match (v["has_more"].as_bool(), v.get("next_cursor")) {
        (Some(false), Some(Value::Null)) => Ok((data, None)),
        (Some(true), Some(Value::String(s))) if !s.is_empty() && s.len() <= 1024 => {
            Ok((data, Some(s)))
        }
        _ => Err(Error::Protocol),
    }
}
fn rich_text(v: &Value) -> Result<String, Error> {
    let pieces = v.as_array().ok_or(Error::Protocol)?;
    let mut text = String::new();
    for part in pieces {
        let value = match part["type"].as_str() {
            Some("text" | "equation") => part["plain_text"].as_str().ok_or(Error::Protocol)?,
            Some("mention") => "[mention]",
            _ => return Err(Error::Protocol),
        };
        if text.len() + value.len() > MAX_BYTES {
            return Err(Error::Limit);
        }
        text.push_str(value);
    }
    Ok(text)
}
fn time(v: &Value) -> Result<&str, Error> {
    let raw = v.as_str().ok_or(Error::Protocol)?;
    time::OffsetDateTime::parse(raw, &time::format_description::well_known::Rfc3339)
        .map_err(|_| Error::Protocol)?;
    Ok(raw)
}
pub(super) fn summary(v: &Value) -> Result<Value, Error> {
    let props = v["properties"].as_object().ok_or(Error::Protocol)?;
    let mut titles = props.values().filter(|p| p["type"] == "title");
    let title = rich_text(&titles.next().ok_or(Error::Protocol)?["title"])?;
    if titles.next().is_some() {
        return Err(Error::Protocol);
    }
    Ok(
        json!({"page_id":identifier(v)?,"title":title,"last_edited_time":time(&v["last_edited_time"])?}),
    )
}
pub(super) fn page(v: &Value) -> Result<Value, Error> {
    let mut props = serde_json::Map::new();
    let mut title = None;
    for (name, p) in v["properties"].as_object().ok_or(Error::Protocol)? {
        let kind = p["type"].as_str().ok_or(Error::Protocol)?;
        let raw = p.get(kind).ok_or(Error::Protocol)?;
        let result = match kind {
            "title" | "rich_text" => {
                let t = rich_text(raw)?;
                if kind == "title" && title.replace(t.clone()).is_some() {
                    return Err(Error::Protocol);
                }
                json!(t)
            }
            "checkbox" => json!(raw.as_bool().ok_or(Error::Protocol)?),
            "number" if raw.is_number() || raw.is_null() => raw.clone(),
            "select" | "status" if raw.is_null() => Value::Null,
            "select" | "status" => json!(raw["name"].as_str().ok_or(Error::Protocol)?),
            "multi_select" => Value::Array(
                raw.as_array()
                    .ok_or(Error::Protocol)?
                    .iter()
                    .map(|x| x["name"].as_str().map(|v| json!(v)).ok_or(Error::Protocol))
                    .collect::<Result<_, _>>()?,
            ),
            "date" if raw.is_null() => Value::Null,
            "date" => {
                let start = raw["start"].as_str().ok_or(Error::Protocol)?;
                if !raw["end"].is_null() && !raw["end"].is_string() {
                    return Err(Error::Protocol);
                }
                json!({"start":start,"end":raw["end"]})
            }
            "created_time" | "last_edited_time" => json!(time(raw)?),
            "number" => return Err(Error::Protocol),
            _ => {
                props.insert(name.clone(), json!({"type":kind,"projected":false}));
                continue;
            }
        };
        props.insert(name.clone(), json!({"type":kind,"value":result}));
    }
    Ok(
        json!({"page_id":identifier(v)?,"title":title.ok_or(Error::Protocol)?,"last_edited_time":time(&v["last_edited_time"])?,"properties":props}),
    )
}
pub(super) fn traversable(v: &Value) -> Result<bool, Error> {
    let children = v["has_children"].as_bool().ok_or(Error::Protocol)?;
    let kind = v["type"].as_str().ok_or(Error::Protocol)?;
    let local = matches!(
        kind,
        "paragraph"
            | "heading_1"
            | "heading_2"
            | "heading_3"
            | "toggle"
            | "bulleted_list_item"
            | "numbered_list_item"
            | "to_do"
            | "quote"
            | "callout"
            | "column_list"
            | "column"
            | "table"
    );
    Ok(children && local)
}
pub(super) fn block(v: &Value) -> Result<Value, Error> {
    let kind = v["type"].as_str().ok_or(Error::Protocol)?;
    let data = v.get(kind).ok_or(Error::Protocol)?;
    let mut out = json!({"block_id":identifier(v)?,"type":kind,"last_edited_time":time(&v["last_edited_time"])?});
    match kind {
        "paragraph" | "heading_1" | "heading_2" | "heading_3" | "toggle" | "bulleted_list_item"
        | "numbered_list_item" | "to_do" | "quote" | "callout" | "code" => {
            out["text"] = json!(rich_text(&data["rich_text"])?);
            if kind == "to_do" {
                out["checked"] = json!(data["checked"].as_bool().ok_or(Error::Protocol)?);
            }
        }
        "table_row" => {
            out["cells"] = Value::Array(
                data["cells"]
                    .as_array()
                    .ok_or(Error::Protocol)?
                    .iter()
                    .map(|cell| rich_text(cell).map(Value::String))
                    .collect::<Result<_, _>>()?,
            )
        }
        "equation" => {
            out["expression"] = json!(data["expression"].as_str().ok_or(Error::Protocol)?)
        }
        "column_list" | "column" | "divider" | "table" => {}
        _ => out["projected"] = json!(false),
    }
    Ok(out)
}
pub(super) fn filter(f: &Filter) -> Result<Value, Error> {
    if f.property.trim().is_empty()
        || f.property.len() > 200
        || f.property.chars().any(char::is_control)
    {
        return Err(Error::Invalid);
    }
    match f.kind.as_str() {
        "title" | "rich_text" | "status" | "select"
            if f.equals.as_str().is_some_and(|s| s.len() <= 1024) => {}
        "number" if f.equals.is_number() => {}
        "checkbox" if f.equals.is_boolean() => {}
        _ => return Err(Error::Invalid),
    }
    let mut v = json!({"property":f.property});
    v[&f.kind] = json!({"equals":f.equals});
    Ok(v)
}
