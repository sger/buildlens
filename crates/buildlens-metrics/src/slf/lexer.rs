use thiserror::Error;

#[derive(Debug, Error)]
pub enum SlfError {
    #[error("activity log is empty; Xcode may not have finished writing it")]
    Empty,
    #[error("not an SLF0 stream (bad magic)")]
    BadMagic,
    #[error("truncated token at byte {0}")]
    Truncated(usize),
    #[error("invalid token payload at byte {offset}: {reason}")]
    Invalid { offset: usize, reason: String },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Token<'a> {
    Int(u64),
    Double(Option<f64>),
    String(&'a [u8]),
    Json(&'a [u8]),
    ClassName(&'a str),
    ClassRef(u64),
    List(u64),
    Null,
}

pub struct Lexer<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Lexer<'a> {
    pub fn new(bytes: &'a [u8]) -> Result<Self, SlfError> {
        if bytes.is_empty() {
            return Err(SlfError::Empty);
        }
        if !bytes.starts_with(b"SLF0") {
            return Err(SlfError::BadMagic);
        }
        Ok(Self { bytes, pos: 4 })
    }

    pub fn position(&self) -> usize {
        self.pos
    }

    pub fn next_token(&mut self) -> Result<Option<Token<'a>>, SlfError> {
        if self.pos >= self.bytes.len() {
            return Ok(None);
        }
        let start = self.pos;
        let mut end = self.pos;
        while end < self.bytes.len() && self.bytes[end].is_ascii_hexdigit() {
            end += 1;
        }
        if end >= self.bytes.len() {
            return Err(SlfError::Truncated(start));
        }
        let prefix = &self.bytes[start..end];
        let sigil = self.bytes[end];
        self.pos = end + 1;
        match sigil {
            b'-' if prefix.is_empty() => Ok(Some(Token::Null)),
            b'#' => Ok(Some(Token::Int(self.decimal(prefix, start)?))),
            b'(' => Ok(Some(Token::List(self.decimal(prefix, start)?))),
            b'@' => Ok(Some(Token::ClassRef(self.decimal(prefix, start)?))),
            b'"' => Ok(Some(Token::String(self.payload(prefix, start)?))),
            b'*' => Ok(Some(Token::Json(self.payload(prefix, start)?))),
            b'%' => {
                let bytes = self.payload(prefix, start)?;
                let name = std::str::from_utf8(bytes).map_err(|_| SlfError::Invalid {
                    offset: start,
                    reason: "class name is not UTF-8".into(),
                })?;
                Ok(Some(Token::ClassName(name)))
            }
            b'^' => {
                if prefix.is_empty() {
                    return Ok(Some(Token::Double(None)));
                }
                let hex = std::str::from_utf8(prefix).expect("hex digits are ASCII");
                let bits = u64::from_str_radix(hex, 16).map_err(|_| SlfError::Invalid {
                    offset: start,
                    reason: format!("bad double payload '{hex}'"),
                })?;
                Ok(Some(Token::Double(Some(f64::from_bits(bits.swap_bytes())))))
            }
            other => Err(SlfError::Invalid {
                offset: start,
                reason: format!("unknown token sigil 0x{other:02x}"),
            }),
        }
    }

    fn decimal(&self, prefix: &[u8], offset: usize) -> Result<u64, SlfError> {
        let text = std::str::from_utf8(prefix).expect("hex digits are ASCII");
        text.parse().map_err(|_| SlfError::Invalid {
            offset,
            reason: format!("bad decimal prefix '{text}'"),
        })
    }

    fn payload(&mut self, prefix: &[u8], offset: usize) -> Result<&'a [u8], SlfError> {
        let length = self.decimal(prefix, offset)? as usize;
        if self.pos + length > self.bytes.len() {
            return Err(SlfError::Truncated(self.pos));
        }
        let payload = &self.bytes[self.pos..self.pos + length];
        self.pos += length;
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_tokens(input: &[u8]) -> Result<Vec<Token<'_>>, SlfError> {
        let mut lexer = Lexer::new(input)?;
        let mut tokens = Vec::new();
        while let Some(token) = lexer.next_token()? {
            tokens.push(token);
        }
        Ok(tokens)
    }

    #[test]
    fn rejects_bad_magic() {
        assert!(matches!(Lexer::new(b"PLST"), Err(SlfError::BadMagic)));
    }

    #[test]
    fn empty_log_reports_that_it_is_empty() {
        // Xcode creates the file before writing it; an empty log is a timing
        // problem, not a corrupt stream.
        let Err(error) = Lexer::new(b"") else {
            panic!("an empty stream must not lex");
        };
        assert!(matches!(error, SlfError::Empty));
        assert!(error.to_string().contains("empty"));
    }

    #[test]
    fn lexes_every_token_kind() {
        let tokens = all_tokens(b"SLF012#5\"hello2*{}3%Abc1@2(-^").unwrap();
        assert_eq!(
            tokens,
            vec![
                Token::Int(12),
                Token::String(b"hello"),
                Token::Json(b"{}"),
                Token::ClassName("Abc"),
                Token::ClassRef(1),
                Token::List(2),
                Token::Null,
                Token::Double(None),
            ]
        );
    }

    #[test]
    fn decodes_byte_swapped_double() {
        // 1.0f64 bits are 0x3ff0000000000000; SLF stores them byte-swapped.
        let bits = 1.0f64.to_bits().swap_bytes();
        let input = format!("SLF0{bits:x}^");
        let tokens = all_tokens(input.as_bytes()).unwrap();
        assert_eq!(tokens, vec![Token::Double(Some(1.0))]);
    }

    #[test]
    fn truncated_string_reports_offset() {
        assert!(matches!(
            all_tokens(b"SLF09\"short"),
            Err(SlfError::Truncated(_))
        ));
    }

    #[test]
    fn truncated_prefix_reports_offset() {
        assert!(matches!(
            all_tokens(b"SLF0123"),
            Err(SlfError::Truncated(4))
        ));
    }
}
