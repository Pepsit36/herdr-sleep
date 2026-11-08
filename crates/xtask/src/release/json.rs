//! A minimal JSON reader and string quoter, enough for the few GitHub API
//! answers the release reads. The tooling takes no HTTP or JSON dependency.

/// A parsed JSON value.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Number(f64),
    String(String),
    Array(Vec<Json>),
    Object(Vec<(String, Json)>),
}

impl Json {
    pub fn parse(text: &str) -> Result<Json, String> {
        let mut parser = Parser {
            chars: text.chars().collect(),
            at: 0,
        };
        let value = parser.value()?;
        parser.skip_space();
        if parser.at != parser.chars.len() {
            return Err(parser.fail("trailing characters"));
        }
        Ok(value)
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Number(number) if *number >= 0.0 && number.fract() == 0.0 => Some(*number as u64),
            _ => None,
        }
    }

    pub fn as_array(&self) -> &[Json] {
        match self {
            Json::Array(items) => items,
            _ => &[],
        }
    }
}

/// `text` as a JSON string literal, quotes included.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

struct Parser {
    chars: Vec<char>,
    at: usize,
}

impl Parser {
    fn fail(&self, what: &str) -> String {
        format!("invalid JSON at character {}: {what}", self.at)
    }

    fn skip_space(&mut self) {
        while self.chars.get(self.at).is_some_and(|c| c.is_whitespace()) {
            self.at += 1;
        }
    }

    fn eat(&mut self, expected: char) -> Result<(), String> {
        self.skip_space();
        if self.chars.get(self.at) == Some(&expected) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.fail(&format!("expected '{expected}'")))
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        let end = self.at + word.chars().count();
        if self
            .chars
            .get(self.at..end)
            .map(|s| s.iter().collect::<String>())
            == Some(word.into())
        {
            self.at = end;
            Ok(value)
        } else {
            Err(self.fail("unknown literal"))
        }
    }

    fn value(&mut self) -> Result<Json, String> {
        self.skip_space();
        match self.chars.get(self.at) {
            Some('{') => self.object(),
            Some('[') => self.array(),
            Some('"') => self.string().map(Json::String),
            Some('t') => self.literal("true", Json::Bool(true)),
            Some('f') => self.literal("false", Json::Bool(false)),
            Some('n') => self.literal("null", Json::Null),
            Some(c) if c.is_ascii_digit() || *c == '-' => self.number(),
            _ => Err(self.fail("unexpected input")),
        }
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.at;
        while self
            .chars
            .get(self.at)
            .is_some_and(|c| c.is_ascii_digit() || "+-.eE".contains(*c))
        {
            self.at += 1;
        }
        let text: String = self.chars[start..self.at].iter().collect();
        text.parse()
            .map(Json::Number)
            .map_err(|_| self.fail("bad number"))
    }

    fn string(&mut self) -> Result<String, String> {
        self.eat('"')?;
        let mut out = String::new();
        loop {
            let c = *self
                .chars
                .get(self.at)
                .ok_or_else(|| self.fail("open string"))?;
            self.at += 1;
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let escaped = *self
                        .chars
                        .get(self.at)
                        .ok_or_else(|| self.fail("open escape"))?;
                    self.at += 1;
                    match escaped {
                        '"' | '\\' | '/' => out.push(escaped),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'u' => out.push(self.unicode()?),
                        _ => return Err(self.fail("bad escape")),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let digits: String = self
            .chars
            .get(self.at..self.at + 4)
            .ok_or_else(|| self.fail("short unicode escape"))?
            .iter()
            .collect();
        self.at += 4;
        u32::from_str_radix(&digits, 16).map_err(|_| self.fail("bad unicode escape"))
    }

    fn unicode(&mut self) -> Result<char, String> {
        let first = self.hex4()?;
        let code = if (0xD800..0xDC00).contains(&first) {
            if self.chars.get(self.at..self.at + 2) != Some(&['\\', 'u']) {
                return Err(self.fail("lone surrogate"));
            }
            self.at += 2;
            let second = self.hex4()?;
            0x10000 + ((first - 0xD800) << 10) + second.wrapping_sub(0xDC00)
        } else {
            first
        };
        char::from_u32(code).ok_or_else(|| self.fail("bad code point"))
    }

    fn array(&mut self) -> Result<Json, String> {
        self.eat('[')?;
        let mut items = Vec::new();
        self.skip_space();
        if self.chars.get(self.at) == Some(&']') {
            self.at += 1;
            return Ok(Json::Array(items));
        }
        loop {
            items.push(self.value()?);
            self.skip_space();
            match self.chars.get(self.at) {
                Some(',') => self.at += 1,
                Some(']') => {
                    self.at += 1;
                    return Ok(Json::Array(items));
                }
                _ => return Err(self.fail("expected ',' or ']'")),
            }
        }
    }

    fn object(&mut self) -> Result<Json, String> {
        self.eat('{')?;
        let mut fields = Vec::new();
        self.skip_space();
        if self.chars.get(self.at) == Some(&'}') {
            self.at += 1;
            return Ok(Json::Object(fields));
        }
        loop {
            self.skip_space();
            let key = self.string()?;
            self.eat(':')?;
            fields.push((key, self.value()?));
            self.skip_space();
            match self.chars.get(self.at) {
                Some(',') => self.at += 1,
                Some('}') => {
                    self.at += 1;
                    return Ok(Json::Object(fields));
                }
                _ => return Err(self.fail("expected ',' or '}'")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_shape_of_a_release() {
        let json = Json::parse(
            r#"{"id": 42, "tag_name": "v1.0.0", "assets": [{"name": "a\"b", "size": 7, "id": 1}], "x": null, "ok": true}"#,
        )
        .unwrap();
        assert_eq!(json.get("id").and_then(Json::as_u64), Some(42));
        let assets = json.get("assets").unwrap().as_array();
        assert_eq!(assets[0].get("name").and_then(Json::as_str), Some("a\"b"));
        assert_eq!(assets[0].get("size").and_then(Json::as_u64), Some(7));
        assert_eq!(json.get("x"), Some(&Json::Null));
    }

    #[test]
    fn reads_unicode_escapes() {
        let json = Json::parse(r#""café 😀""#).unwrap();
        assert_eq!(json.as_str(), Some("café 😀"));
    }

    #[test]
    fn refuses_malformed_input() {
        for text in ["", "{", "[1,", "{\"a\" 1}", "tru", "1 2", "\"open"] {
            assert!(Json::parse(text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn quoting_round_trips() {
        let raw = "line\n\"quoted\" \\ tab\t \u{1} é";
        assert_eq!(Json::parse(&quote(raw)).unwrap().as_str(), Some(raw));
    }
}
