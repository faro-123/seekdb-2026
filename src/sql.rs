//! Tokenized SQL subset; parsing always consumes the entire statement.
use crate::types::{Column, DataType, Literal, Schema};
use crate::{Error, Result};

#[derive(Clone, Debug)]
enum Token {
    Word(String),
    QuotedIdentifier(String),
    Number(String),
    Text(String),
    Symbol(char),
    Compare(String),
}
#[derive(Clone, Debug)]
pub(crate) enum Expr {
    Column(String),
    Distance(String, String),
}
#[derive(Clone, Debug)]
pub(crate) struct Projection {
    pub expr: Expr,
    pub alias: Option<String>,
}
#[derive(Clone, Debug)]
pub(crate) struct Condition {
    pub column: String,
    pub op: String,
    pub value: Literal,
}
#[derive(Clone, Debug)]
pub(crate) struct Select {
    pub table: String,
    pub projections: Vec<Projection>,
    pub star: bool,
    pub conditions: Vec<Condition>,
    pub order: Vec<(Expr, bool)>,
    pub limit: Option<usize>,
}
#[derive(Debug)]
pub(crate) enum Statement<'a> {
    Create(Schema),
    Index {
        name: String,
        table: String,
        column: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: InsertRows<'a>,
    },
    Update {
        table: String,
        assignments: Vec<(String, Literal)>,
        conditions: Vec<Condition>,
    },
    Delete {
        table: String,
        conditions: Vec<Condition>,
    },
    Select(Select),
    Begin,
    Commit,
    Rollback,
}

fn syntax(message: &str) -> Error {
    Error::new("unsupported_sql", message)
}

#[derive(Debug)]
struct Lexer<'a> {
    rest: &'a str,
}

impl<'a> Lexer<'a> {
    fn token(&mut self) -> Result<Option<Token>> {
        loop {
            let input = self.rest.trim_start_matches(char::is_whitespace);
            let Some(first) = input.chars().next() else {
                self.rest = "";
                return Ok(None);
            };
            if let Some(comment) = input.strip_prefix("--") {
                self.rest = comment.split_once('\n').map_or("", |(_, rest)| rest);
                continue;
            }
            if let Some(comment) = input.strip_prefix("/*") {
                let (_, rest) = comment
                    .split_once("*/")
                    .ok_or_else(|| syntax("unterminated comment"))?;
                self.rest = rest;
                continue;
            }
            let tail = &input[first.len_utf8()..];
            let (token, rest) = match first {
                '\'' => {
                    let mut start = 0;
                    let end = loop {
                        let quote = start
                            + tail[start..]
                                .find('\'')
                                .ok_or_else(|| syntax("unterminated string"))?;
                        if tail.as_bytes().get(quote + 1) != Some(&b'\'') {
                            break quote;
                        }
                        start = quote + 2;
                    };
                    let content = &tail[..end];
                    let text = if start == 0 {
                        content.to_owned()
                    } else {
                        content.replace("''", "'")
                    };
                    (Token::Text(text), &tail[end + 1..])
                }
                '\x60' => {
                    let (name, rest) = tail
                        .split_once('\x60')
                        .ok_or_else(|| syntax("unterminated identifier"))?;
                    if name.is_empty() {
                        return Err(syntax("empty identifier"));
                    }
                    (Token::QuotedIdentifier(name.to_lowercase()), rest)
                }
                '(' | ')' | ',' | ';' | '*' => (Token::Symbol(first), tail),
                '=' | '<' | '>' => {
                    let len = 1 + usize::from(tail.starts_with('='));
                    (Token::Compare(input[..len].to_owned()), &input[len..])
                }
                c if c.is_ascii_digit() || matches!(c, '-' | '+' | '.') => {
                    let len = 1 + tail
                        .find(|c: char| {
                            !c.is_ascii_digit() && !matches!(c, '.' | 'e' | 'E' | '+' | '-')
                        })
                        .unwrap_or(tail.len());
                    (Token::Number(input[..len].to_owned()), &input[len..])
                }
                c if c.is_alphabetic() || c == '_' => {
                    let len = c.len_utf8()
                        + tail
                            .find(|c: char| !c.is_alphanumeric() && c != '_')
                            .unwrap_or(tail.len());
                    (Token::Word(input[..len].to_lowercase()), &input[len..])
                }
                _ => return Err(syntax("unexpected character")),
            };
            self.rest = rest;
            return Ok(Some(token));
        }
    }
}

impl Iterator for Lexer<'_> {
    type Item = Result<Token>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.token() {
            Ok(token) => token.map(Ok),
            Err(error) => {
                self.rest = "";
                Some(Err(error))
            }
        }
    }
}

#[derive(Debug)]
struct Parser<'a> {
    tokens: std::iter::Peekable<Lexer<'a>>,
}

impl<'a> Parser<'a> {
    fn word(&mut self, word: &str) -> bool {
        if matches!(self.tokens.peek(), Some(Ok(Token::Word(w))) if w == word) {
            self.tokens.next();
            true
        } else {
            false
        }
    }
    fn expect(&mut self, word: &str) -> Result<()> {
        if self.word(word) {
            Ok(())
        } else {
            Err(syntax(&format!("expected {word}")))
        }
    }
    fn symbol(&mut self, ch: char) -> bool {
        if matches!(self.tokens.peek(), Some(Ok(Token::Symbol(c))) if *c == ch) {
            self.tokens.next();
            true
        } else {
            false
        }
    }
    fn punctuation(&mut self, ch: char) -> Result<()> {
        if self.symbol(ch) {
            Ok(())
        } else {
            Err(syntax("missing punctuation"))
        }
    }
    fn take(&mut self) -> Result<Token> {
        self.tokens
            .next()
            .ok_or_else(|| syntax("unexpected end of SQL"))?
    }
    fn finish(&mut self) -> Result<()> {
        self.symbol(';');
        match self.tokens.next() {
            None => Ok(()),
            Some(Err(error)) => Err(error),
            Some(Ok(_)) => Err(syntax("trailing or unsupported SQL")),
        }
    }
    fn identifier(&mut self) -> Result<String> {
        match self.take()? {
            Token::Word(w) | Token::QuotedIdentifier(w) => Ok(w),
            _ => Err(syntax("expected identifier")),
        }
    }
    fn literal(&mut self) -> Result<Literal> {
        match self.take()? {
            Token::Number(n) => Ok(Literal::Number(n)),
            Token::Text(s) => Ok(Literal::Text(s)),
            _ => Err(syntax("expected literal")),
        }
    }
    fn size(&mut self) -> Result<usize> {
        if let Token::Number(n) = self.take()? {
            n.parse().map_err(|_| syntax("expected nonnegative size"))
        } else {
            Err(syntax("expected size"))
        }
    }
    fn expr(&mut self) -> Result<Expr> {
        let is_distance =
            matches!(self.tokens.peek(), Some(Ok(Token::Word(w))) if w == "l2_distance");
        let name = self.identifier()?;
        if is_distance && self.symbol('(') {
            let col = self.identifier()?;
            self.punctuation(',')?;
            let Literal::Text(v) = self.literal()? else {
                return Err(syntax("distance needs a vector string"));
            };
            self.punctuation(')')?;
            Ok(Expr::Distance(col, v))
        } else {
            Ok(Expr::Column(name))
        }
    }
    fn conditions(&mut self) -> Result<Vec<Condition>> {
        let mut result = Vec::new();
        if self.word("where") {
            loop {
                let column = self.identifier()?;
                let Token::Compare(op) = self.take()? else {
                    return Err(syntax("expected comparison"));
                };
                if !matches!(op.as_str(), "=" | "<" | "<=" | ">" | ">=") {
                    return Err(syntax("unsupported comparison"));
                }
                result.push(Condition {
                    column,
                    op,
                    value: self.literal()?,
                });
                if !self.word("and") {
                    break;
                }
            }
        }
        Ok(result)
    }
    fn statement(&mut self) -> Result<Statement<'a>> {
        if self.word("begin") {
            self.word("transaction");
            return Ok(Statement::Begin);
        }
        if self.word("commit") {
            return Ok(Statement::Commit);
        }
        if self.word("rollback") {
            return Ok(Statement::Rollback);
        }
        if self.word("create") {
            if self.word("vector") {
                self.expect("index")?;
                let name = self.identifier()?;
                self.expect("on")?;
                let table = self.identifier()?;
                self.punctuation('(')?;
                let column = self.identifier()?;
                self.punctuation(')')?;
                return Ok(Statement::Index {
                    name,
                    table,
                    column,
                });
            }
            self.expect("table")?;
            let name = self.identifier()?;
            self.punctuation('(')?;
            let mut columns: Vec<Column> = Vec::new();
            loop {
                let col = self.identifier()?;
                if columns.iter().any(|c| c.name == col) {
                    return Err(syntax("duplicate column"));
                }
                let kind = if self.word("bigint") {
                    DataType::Bigint
                } else {
                    let typ = self.identifier()?;
                    self.punctuation('(')?;
                    let n = self.size()?;
                    self.punctuation(')')?;
                    if n == 0 {
                        return Err(syntax("type size must be positive"));
                    }
                    match typ.as_str() {
                        "varchar" => DataType::Varchar(n),
                        "vector" => DataType::Vector(n),
                        _ => return Err(syntax("unsupported type")),
                    }
                };
                let primary = if self.word("primary") {
                    self.expect("key")?;
                    true
                } else {
                    false
                };
                columns.push(Column {
                    name: col,
                    kind,
                    primary,
                });
                if !self.symbol(',') {
                    break;
                }
            }
            self.punctuation(')')?;
            let primary: Vec<_> = columns
                .iter()
                .enumerate()
                .filter(|(_, c)| c.primary)
                .collect();
            if primary.len() != 1 || !matches!(primary[0].1.kind, DataType::Bigint) {
                return Err(syntax("exactly one BIGINT primary key required"));
            }
            if columns
                .iter()
                .filter(|c| matches!(c.kind, DataType::Vector(_)))
                .count()
                > 1
            {
                return Err(syntax("only one vector column permitted"));
            }
            let primary = primary[0].0;
            return Ok(Statement::Create(Schema {
                name,
                columns,
                primary,
                indexes: Vec::new(),
            }));
        }
        if self.word("update") {
            let table = self.identifier()?;
            self.expect("set")?;
            let mut assignments = Vec::new();
            loop {
                let name = self.identifier()?;
                if !matches!(self.take()?,Token::Compare(c) if c=="=") {
                    return Err(syntax("expected assignment"));
                }
                assignments.push((name, self.literal()?));
                if !self.symbol(',') {
                    break;
                }
            }
            return Ok(Statement::Update {
                table,
                assignments,
                conditions: self.conditions()?,
            });
        }
        if self.word("delete") {
            self.expect("from")?;
            let table = self.identifier()?;
            return Ok(Statement::Delete {
                table,
                conditions: self.conditions()?,
            });
        }
        if self.word("select") {
            let star = self.symbol('*');
            let mut projections = Vec::new();
            if !star {
                loop {
                    let expr = self.expr()?;
                    let alias = if self.word("as") {
                        Some(self.identifier()?)
                    } else {
                        None
                    };
                    projections.push(Projection { expr, alias });
                    if !self.symbol(',') {
                        break;
                    }
                }
            }
            self.expect("from")?;
            let table = self.identifier()?;
            let conditions = self.conditions()?;
            let mut order = Vec::new();
            if self.word("order") {
                self.expect("by")?;
                loop {
                    let expr = self.expr()?;
                    let descending = if self.word("desc") {
                        true
                    } else {
                        self.word("asc");
                        false
                    };
                    order.push((expr, descending));
                    if !self.symbol(',') {
                        break;
                    }
                }
            }
            let limit = if self.word("limit") {
                Some(self.size()?)
            } else {
                None
            };
            return Ok(Statement::Select(Select {
                table,
                projections,
                star,
                conditions,
                order,
                limit,
            }));
        }
        Err(syntax("statement not supported"))
    }
}

#[derive(Debug)]
pub(crate) struct InsertRows<'a> {
    parser: Parser<'a>,
    finished: bool,
}

impl InsertRows<'_> {
    fn row(&mut self) -> Result<Vec<Literal>> {
        self.parser.punctuation('(')?;
        let mut row = vec![self.parser.literal()?];
        while self.parser.symbol(',') {
            row.push(self.parser.literal()?);
        }
        self.parser.punctuation(')')?;
        if !self.parser.symbol(',') {
            self.parser.finish()?;
            self.finished = true;
        }
        Ok(row)
    }
}

impl Iterator for InsertRows<'_> {
    type Item = Result<Vec<Literal>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        let row = self.row();
        if row.is_err() {
            self.finished = true;
        }
        Some(row)
    }
}

pub(crate) fn parse(input: &str) -> Result<Statement<'_>> {
    let mut parser = Parser {
        tokens: Lexer { rest: input }.peekable(),
    };
    if parser.word("insert") {
        parser.expect("into")?;
        let table = parser.identifier()?;
        let columns = if parser.symbol('(') {
            let mut names = vec![parser.identifier()?];
            while parser.symbol(',') {
                names.push(parser.identifier()?);
            }
            parser.punctuation(')')?;
            Some(names)
        } else {
            None
        };
        parser.expect("values")?;
        return Ok(Statement::Insert {
            table,
            columns,
            rows: InsertRows {
                parser,
                finished: false,
            },
        });
    }
    let statement = parser.statement()?;
    parser.finish()?;
    Ok(statement)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_defers_row_syntax_until_iteration() {
        // The parser hands the engine a bounded stream after the INSERT header.
        // The engine must finish it before publishing the statement.
        assert!(matches!(
            parse("/* prefix */ INSERT INTO `T` (id,s) VALUES (1,'can''t'),(2,'unfinished"),
            Ok(Statement::Insert { .. })
        ));
    }
}
