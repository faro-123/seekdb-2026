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
#[derive(Clone, Debug)]
pub(crate) enum Statement {
    Create(Schema),
    Index {
        name: String,
        table: String,
        column: String,
    },
    Insert {
        table: String,
        columns: Option<Vec<String>>,
        rows: Vec<Vec<Literal>>,
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

fn lex(sql: &str) -> Result<Vec<Token>> {
    let mut chars = sql.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(c) = chars.next() {
        if c.is_whitespace() {
            continue;
        }
        match c {
            '\'' => {
                let mut text = String::new();
                loop {
                    match chars.next() {
                        Some('\'') if chars.peek() == Some(&'\'') => {
                            chars.next();
                            text.push('\'');
                        }
                        Some('\'') => break,
                        Some(x) => text.push(x),
                        None => return Err(syntax("unterminated string")),
                    }
                }
                tokens.push(Token::Text(text));
            }
            '`' => {
                let mut name = String::new();
                loop {
                    match chars.next() {
                        Some('`') => break,
                        Some(x) => name.push(x),
                        None => return Err(syntax("unterminated identifier")),
                    }
                }
                if name.is_empty() {
                    return Err(syntax("empty identifier"));
                }
                tokens.push(Token::QuotedIdentifier(name.to_lowercase()));
            }
            '-' if chars.peek() == Some(&'-') => {
                for x in chars.by_ref() {
                    if x == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut closed = false;
                while let Some(x) = chars.next() {
                    if x == '*' && chars.peek() == Some(&'/') {
                        chars.next();
                        closed = true;
                        break;
                    }
                }
                if !closed {
                    return Err(syntax("unterminated comment"));
                }
            }
            '(' | ')' | ',' | ';' | '*' => tokens.push(Token::Symbol(c)),
            '=' | '<' | '>' => {
                let mut op = c.to_string();
                if chars.peek() == Some(&'=') {
                    op.push(chars.next().unwrap());
                }
                tokens.push(Token::Compare(op));
            }
            x if x.is_ascii_digit() || x == '-' || x == '+' || x == '.' => {
                let mut text = x.to_string();
                while chars
                    .peek()
                    .is_some_and(|x| x.is_ascii_digit() || matches!(x, '.' | 'e' | 'E' | '+' | '-'))
                {
                    text.push(chars.next().unwrap());
                }
                tokens.push(Token::Number(text));
            }
            x if x.is_alphabetic() || x == '_' => {
                let mut text = x.to_string();
                while chars
                    .peek()
                    .is_some_and(|x| x.is_alphanumeric() || *x == '_')
                {
                    text.push(chars.next().unwrap());
                }
                tokens.push(Token::Word(text.to_lowercase()));
            }
            _ => return Err(syntax("unexpected character")),
        }
    }
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}
impl Parser {
    fn word(&mut self, word: &str) -> bool {
        if matches!(self.tokens.get(self.pos),Some(Token::Word(w)) if w==word) {
            self.pos += 1;
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
        if matches!(self.tokens.get(self.pos),Some(Token::Symbol(c)) if *c==ch) {
            self.pos += 1;
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
        let value = self
            .tokens
            .get(self.pos)
            .cloned()
            .ok_or_else(|| syntax("unexpected end of SQL"))?;
        self.pos += 1;
        Ok(value)
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
            matches!(self.tokens.get(self.pos), Some(Token::Word(w)) if w == "l2_distance");
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
    fn statement(&mut self) -> Result<Statement> {
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
        if self.word("insert") {
            self.expect("into")?;
            let table = self.identifier()?;
            let columns = if self.symbol('(') {
                let mut names = vec![self.identifier()?];
                while self.symbol(',') {
                    names.push(self.identifier()?);
                }
                self.punctuation(')')?;
                Some(names)
            } else {
                None
            };
            self.expect("values")?;
            let mut rows = Vec::new();
            loop {
                self.punctuation('(')?;
                let mut row = vec![self.literal()?];
                while self.symbol(',') {
                    row.push(self.literal()?);
                }
                self.punctuation(')')?;
                rows.push(row);
                if !self.symbol(',') {
                    break;
                }
            }
            return Ok(Statement::Insert {
                table,
                columns,
                rows,
            });
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

pub(crate) fn parse(input: &str) -> Result<Statement> {
    let mut parser = Parser {
        tokens: lex(input)?,
        pos: 0,
    };
    let statement = parser.statement()?;
    parser.symbol(';');
    if parser.pos != parser.tokens.len() {
        return Err(syntax("trailing or unsupported SQL"));
    }
    Ok(statement)
}
