//! Markdown files with a YAML frontmatter block:
//!
//! ```text
//! ---
//! key: value
//! ---
//! body
//! ```

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use yaml_serde::Value;

/// Splits a document into its raw YAML frontmatter and its body.
pub fn split(content: &str) -> Result<(&str, &str)> {
    let content = content.trim_start_matches('\u{feff}');
    let rest = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))
        .context("missing frontmatter: the file must start with a `---` line")?;

    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end() == "---" {
            return Ok((&rest[..offset], &rest[offset + line.len()..]));
        }
        offset += line.len();
    }
    bail!("unterminated frontmatter: missing closing `---` line")
}

/// Parses a document into `T` plus its trimmed body, rejecting any
/// frontmatter key not in `allowed` so typos surface as errors.
pub fn parse<T: DeserializeOwned>(content: &str, allowed: &[&str]) -> Result<(T, String)> {
    let (yaml, body) = split(content)?;
    let value: Value = yaml_serde::from_str(yaml).context("invalid YAML in frontmatter")?;
    let Value::Mapping(map) = &value else {
        bail!("frontmatter must be a YAML mapping");
    };
    for key in map.keys() {
        let key = key.as_str().context("frontmatter keys must be strings")?;
        if !allowed.contains(&key) {
            bail!("unknown field `{key}`");
        }
    }
    let parsed = yaml_serde::from_value(value)?;
    Ok((parsed, body.trim().to_string()))
}

/// Renders `frontmatter` and `body` back into a document.
pub fn render<T: serde::Serialize>(frontmatter: &T, body: &str) -> Result<String> {
    let yaml = yaml_serde::to_string(frontmatter)?;
    Ok(format!("---\n{yaml}---\n\n{}\n", body.trim()))
}

/// Returns true if the frontmatter contains `key`. Used to tell tasks from
/// schedules without fully parsing them.
pub fn has_key(content: &str, key: &str) -> bool {
    split(content)
        .ok()
        .and_then(|(yaml, _)| yaml_serde::from_str::<Value>(yaml).ok())
        .is_some_and(|v| v.get(key).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_frontmatter_and_body() {
        let (fm, body) = split("---\na: 1\n---\n\nhello\n").unwrap();
        assert_eq!(fm, "a: 1\n");
        assert_eq!(body, "\nhello\n");
    }

    #[test]
    fn handles_crlf() {
        let (fm, body) = split("---\r\na: 1\r\n---\r\nhello").unwrap();
        assert_eq!(fm, "a: 1\r\n");
        assert_eq!(body, "hello");
    }

    #[test]
    fn rejects_missing_or_unterminated_frontmatter() {
        assert!(split("hello").is_err());
        assert!(split("---\na: 1\n").is_err());
    }

    #[test]
    fn rejects_unknown_keys() {
        #[derive(serde::Deserialize)]
        struct T {
            #[allow(dead_code)]
            a: u32,
        }
        let err = parse::<T>("---\na: 1\nb: 2\n---\n", &["a"]).err().unwrap();
        assert!(err.to_string().contains("unknown field `b`"));
    }
}
