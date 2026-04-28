//! TOML mutation primitives. Format-preserving via toml_edit.
//!
//! Value parsing convention (matches the help text in main.rs):
//!   * `true` / `false`            → boolean
//!   * `123`, `-7`                 → integer
//!   * `@something`                → literal string `something`
//!   * everything else             → string

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use toml_edit::{value, Array, DocumentMut, Item, Value};

pub fn set(args: &[String]) -> Result<()> {
    let (path, key, raw) = take3(args, "config set <file> <key> <value>")?;
    let mut doc = load(path)?;
    doc[key] = parse_scalar(raw);
    save(path, &doc)?;
    println!("set {key} in {}", path.display());
    Ok(())
}

pub fn unset(args: &[String]) -> Result<()> {
    let (path, key) = take2(args, "config unset <file> <key>")?;
    let mut doc = load(path)?;
    if doc.remove(key).is_some() {
        save(path, &doc)?;
        println!("unset {key} in {}", path.display());
    } else {
        println!("{key} not present in {} (no change)", path.display());
    }
    Ok(())
}

pub fn array_add(args: &[String]) -> Result<()> {
    let (path, key, raw) = take3(args, "config array-add <file> <key> <value>")?;
    let mut doc = load(path)?;

    if !doc.contains_key(key) || !doc[key].is_array() {
        doc[key] = Item::Value(Value::Array(Array::new()));
    }
    let arr = doc[key]
        .as_array_mut()
        .ok_or_else(|| anyhow!("{key} is not an array"))?;

    let new_val = parse_array_element(raw);
    if arr
        .iter()
        .any(|v| compact_repr(v) == compact_repr(&new_val))
    {
        println!("{key} already contains {raw} — no change");
        return Ok(());
    }
    arr.push(new_val);
    save(path, &doc)?;
    println!("appended {raw} to {key}");
    Ok(())
}

pub fn array_remove(args: &[String]) -> Result<()> {
    let (path, key, raw) = take3(args, "config array-remove <file> <key> <value>")?;
    let mut doc = load(path)?;

    let arr = doc
        .get_mut(key)
        .and_then(|i| i.as_array_mut())
        .ok_or_else(|| anyhow!("{key} is not an array (or not present)"))?;

    let target = parse_array_element(raw);
    let target_repr = compact_repr(&target);
    let before = arr.len();
    arr.retain(|v| compact_repr(v) != target_repr);
    let removed = before - arr.len();

    save(path, &doc)?;
    println!("removed {removed} occurrence(s) of {raw} from {key}");
    Ok(())
}

// ---- helpers -----------------------------------------------------------

fn load(path: &Path) -> Result<DocumentMut> {
    let content = fs::read_to_string(path).with_context(|| format!("read {path:?}"))?;
    content
        .parse::<DocumentMut>()
        .with_context(|| format!("parse TOML in {path:?}"))
}

fn save(path: &Path, doc: &DocumentMut) -> Result<()> {
    let mut tmp: PathBuf = path.to_path_buf();
    let new_ext = match path.extension().and_then(|e| e.to_str()) {
        Some(ext) => format!("{ext}.tmp"),
        None => "tmp".to_string(),
    };
    tmp.set_extension(new_ext);

    fs::write(&tmp, doc.to_string()).with_context(|| format!("write {tmp:?}"))?;
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(anyhow!("rename {tmp:?} → {path:?}: {e}"));
    }
    Ok(())
}

fn parse_scalar(raw: &str) -> Item {
    if raw == "true" {
        return value(true);
    }
    if raw == "false" {
        return value(false);
    }
    if let Ok(n) = raw.parse::<i64>() {
        return value(n);
    }
    if let Some(rest) = raw.strip_prefix('@') {
        return value(rest);
    }
    value(raw)
}

fn parse_array_element(raw: &str) -> Value {
    if raw == "true" {
        return Value::from(true);
    }
    if raw == "false" {
        return Value::from(false);
    }
    if let Ok(n) = raw.parse::<i64>() {
        return Value::from(n);
    }
    if let Some(rest) = raw.strip_prefix('@') {
        return Value::from(rest);
    }
    Value::from(raw)
}

/// Stringify a Value in a deterministic, comparison-friendly form so
/// `array-remove 1234` matches a stored integer 1234 regardless of
/// whether it was written with quotes, leading zeros, etc.
fn compact_repr(v: &Value) -> String {
    if let Some(i) = v.as_integer() {
        return format!("i:{i}");
    }
    if let Some(b) = v.as_bool() {
        return format!("b:{b}");
    }
    if let Some(s) = v.as_str() {
        return format!("s:{s}");
    }
    format!("?:{}", v.to_string().trim())
}

fn take2<'a>(args: &'a [String], usage: &str) -> Result<(&'a Path, &'a str)> {
    if args.len() != 2 {
        anyhow::bail!("usage: {usage}");
    }
    Ok((Path::new(&args[0]), args[1].as_str()))
}

fn take3<'a>(args: &'a [String], usage: &str) -> Result<(&'a Path, &'a str, &'a str)> {
    if args.len() != 3 {
        anyhow::bail!("usage: {usage}");
    }
    Ok((Path::new(&args[0]), args[1].as_str(), args[2].as_str()))
}
