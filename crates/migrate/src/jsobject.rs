//! Reads the exported object of a JavaScript config file when it is a plain literal: strings,
//! numbers, booleans, `null`, arrays and objects, with comments and trailing commas. Anything
//! computed (variables, calls, spreads, `${}` templates) is refused, so the result is exactly
//! what the file exports.

use serde_json::{Map, Number, Value};

/// The value after `module.exports =` or `export default`.
pub(crate) fn exported(source: &str) -> Result<Value, String> {
    let start = ["module.exports", "export default"]
        .iter()
        .find_map(|marker| source.find(marker).map(|at| at + marker.len()))
        .ok_or("no `module.exports =` or `export default`")?;
    let mut parser = Parser {
        text: source.as_bytes(),
        at: start,
    };
    parser.skip();
    if source[parser.at..].starts_with('=') {
        parser.at += 1;
    }
    let value = parser.value()?;
    parser.skip();
    if parser.peek() == Some(b';') {
        parser.at += 1;
    }
    parser.skip();
    if parser.at < parser.text.len() {
        return Err(parser.error("code after the exported value"));
    }
    Ok(value)
}

/// A cursor over the source.
struct Parser<'a> {
    text: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    /// The next byte, if any.
    fn peek(&self) -> Option<u8> {
        self.text.get(self.at).copied()
    }

    /// An error naming the line.
    fn error(&self, what: &str) -> String {
        let line = self.text[..self.at.min(self.text.len())]
            .iter()
            .filter(|b| **b == b'\n')
            .count()
            + 1;
        format!("{what} on line {line}")
    }

    /// Skips whitespace and comments.
    fn skip(&mut self) {
        loop {
            match (self.peek(), self.text.get(self.at + 1).copied()) {
                (Some(b' ' | b'\t' | b'\n' | b'\r'), _) => self.at += 1,
                (Some(b'/'), Some(b'/')) => {
                    while self.peek().is_some_and(|b| b != b'\n') {
                        self.at += 1;
                    }
                }
                (Some(b'/'), Some(b'*')) => {
                    self.at += 2;
                    while self.at < self.text.len() && !self.text[self.at..].starts_with(b"*/") {
                        self.at += 1;
                    }
                    self.at = (self.at + 2).min(self.text.len());
                }
                _ => return,
            }
        }
    }

    /// One value.
    fn value(&mut self) -> Result<Value, String> {
        self.skip();
        match self.peek() {
            Some(b'{') => self.object(),
            Some(b'[') => self.array(),
            Some(quote @ (b'"' | b'\'' | b'`')) => self.string(quote).map(Value::String),
            Some(b'-' | b'0'..=b'9' | b'.') => self.number(),
            Some(_) => {
                let word = self.word();
                match word.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" | "undefined" => Ok(Value::Null),
                    "" => Err(self.error("unexpected character")),
                    other => Err(self.error(&format!("computed value `{other}`"))),
                }
            }
            None => Err(self.error("unexpected end")),
        }
    }

    /// An identifier-like word, possibly empty.
    fn word(&mut self) -> String {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
        {
            self.at += 1;
        }
        String::from_utf8_lossy(&self.text[start..self.at]).into_owned()
    }

    /// `{ key: value, ... }`.
    fn object(&mut self) -> Result<Value, String> {
        self.at += 1;
        let mut map = Map::new();
        loop {
            self.skip();
            match self.peek() {
                Some(b'}') => {
                    self.at += 1;
                    return Ok(Value::Object(map));
                }
                Some(quote @ (b'"' | b'\'')) => {
                    let key = self.string(quote)?;
                    map.insert(key, self.after_key()?);
                }
                Some(b'.') => return Err(self.error("spread")),
                Some(_) => {
                    let key = self.word();
                    if key.is_empty() {
                        return Err(self.error("unexpected character in an object"));
                    }
                    map.insert(key, self.after_key()?);
                }
                None => return Err(self.error("unterminated object")),
            }
            self.skip();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b'}') => {}
                _ => return Err(self.error("expected `,` or `}`")),
            }
        }
    }

    /// `: value` after an object key.
    fn after_key(&mut self) -> Result<Value, String> {
        self.skip();
        if self.peek() != Some(b':') {
            return Err(self.error("shorthand property or method"));
        }
        self.at += 1;
        self.value()
    }

    /// `[ value, ... ]`.
    fn array(&mut self) -> Result<Value, String> {
        self.at += 1;
        let mut items = Vec::new();
        loop {
            self.skip();
            if self.peek() == Some(b']') {
                self.at += 1;
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.skip();
            match self.peek() {
                Some(b',') => self.at += 1,
                Some(b']') => {}
                _ => return Err(self.error("expected `,` or `]`")),
            }
        }
    }

    /// A quoted string; a template literal only without `${`.
    fn string(&mut self, quote: u8) -> Result<String, String> {
        self.at += 1;
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(self.error("unterminated string")),
                Some(b) if b == quote => {
                    self.at += 1;
                    return String::from_utf8(out).map_err(|_| self.error("invalid UTF-8"));
                }
                Some(b'$') if quote == b'`' && self.text.get(self.at + 1) == Some(&b'{') => {
                    return Err(self.error("template literal with ${}"));
                }
                Some(b'\\') => {
                    self.at += 1;
                    let escaped = self
                        .peek()
                        .ok_or_else(|| self.error("unterminated string"))?;
                    out.push(match escaped {
                        b'n' => b'\n',
                        b't' => b'\t',
                        b'r' => b'\r',
                        other => other,
                    });
                    self.at += 1;
                }
                Some(b) => {
                    out.push(b);
                    self.at += 1;
                }
            }
        }
    }

    /// A decimal number.
    fn number(&mut self) -> Result<Value, String> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|b| b.is_ascii_digit() || matches!(b, b'-' | b'+' | b'.' | b'e' | b'E'))
        {
            self.at += 1;
        }
        let text = String::from_utf8_lossy(&self.text[start..self.at]).into_owned();
        if let Ok(int) = text.parse::<i64>() {
            return Ok(Value::Number(int.into()));
        }
        text.parse::<f64>()
            .ok()
            .and_then(Number::from_f64)
            .map(Value::Number)
            .ok_or_else(|| self.error(&format!("number {text}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_plain_literals_and_refuses_computed_ones() {
        let value = exported(
            "// acme\nmodule.exports = {\n  apps: [{ name: 'api', script: \"server.js\", instances: 2, }],\n};\n",
        )
        .expect("static");
        assert_eq!(value["apps"][0]["name"], "api");
        assert_eq!(value["apps"][0]["instances"], 2);
        assert!(exported("module.exports = { apps: [{ cwd: __dirname }] }").is_err());
        assert!(exported("export default { env: { A: `x${y}` } }").is_err());
    }
}
