//! Deterministic formula evaluation for the safe MVP subset.

use std::collections::{HashMap, HashSet};

use crate::{Cell, CellErrorType, Workbook};

const MAX_RANGE_CELLS: u64 = 10_000;

/// Canonical Excel error literal strings recognized when parsing a `#...`
/// token in formula text (e.g. the `#N/A` inside `ISNA(#N/A)`). The first
/// seven are pulled straight from `CellErrorType::as_str`, the crate's
/// single source of truth for error-code display strings, so this list
/// can't drift from it. `#GETTING_DATA` is the legacy literal spelling
/// `CellErrorType::from_excel_error` also accepts as input (its canonical
/// *display* form is `#DATA!`, already covered by the `GettingData` slot
/// below); it's listed explicitly since that's the historical spelling
/// formulas actually use as a literal.
fn error_literals() -> [&'static str; 8] {
    [
        CellErrorType::Null.as_str(),
        CellErrorType::Div0.as_str(),
        CellErrorType::Value.as_str(),
        CellErrorType::Ref.as_str(),
        CellErrorType::Name.as_str(),
        CellErrorType::Num.as_str(),
        CellErrorType::NA.as_str(),
        "#GETTING_DATA",
    ]
}

/// Bound on recursive-descent parser nesting (parentheses, function-call
/// arguments, and chained unary operators). Each nested level costs roughly
/// eight stack frames walking the precedence chain
/// (parse_comparison -> parse_concat -> parse_add -> parse_mul ->
/// parse_power -> parse_unary -> parse_postfix -> parse_primary), and this
/// crate's small, mostly-scalar locals keep debug-build frames well under
/// 1 KiB each. 128 levels is therefore at most ~1k frames / ~1 MiB of stack
/// in the worst case -- generous for real-world formulas (which rarely
/// nest more than a handful of levels deep) while staying far inside even a
/// constrained 2 MiB thread stack.
const MAX_EXPR_DEPTH: usize = 128;

/// Result of evaluating a formula cell.
#[derive(Debug, Clone, PartialEq)]
pub enum FormulaEvaluation {
    /// The formula was evaluated deterministically.
    Computed(Cell),
    /// The evaluator returned the reader's cached value and a typed reason.
    Fallback {
        /// Cached value stored in the workbook.
        cached: Cell,
        /// Why the formula was not evaluated.
        reason: FormulaUnsupportedReason,
    },
}

/// Why a formula could not be evaluated deterministically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormulaUnsupportedReason {
    /// Function is not implemented by the deterministic evaluator.
    UnsupportedFunction,
    /// Function is volatile or environment-dependent.
    Volatile,
    /// Formula references an external workbook.
    ExternalRef,
    /// Circular dependency detected.
    CircularReference,
    /// Defined name or bare identifier could not be resolved.
    UnresolvedName,
    /// Formula text is outside the MVP grammar.
    UnparsableExpression,
    /// Array or dynamic-array semantics are required.
    ArraySemantics,
    /// Reference range exceeded the evaluator's bounded traversal limit.
    RangeTooLarge,
    /// Referenced worksheet does not exist.
    SheetNotFound,
    /// Formula nesting (parentheses, function calls, or unary operators)
    /// exceeded the evaluator's bounded recursion depth.
    ExpressionTooComplex,
}

impl Workbook {
    /// Evaluate one worksheet cell.
    ///
    /// The MVP evaluates deterministic literal formulas only. Unsupported
    /// formulas return their cached value with a typed reason.
    pub fn evaluate_cell(&self, sheet: &str, row: u32, col: u16) -> FormulaEvaluation {
        let Some(sheet) = self.sheet_by_name(sheet) else {
            return FormulaEvaluation::Fallback {
                cached: Cell::Text(String::new()),
                reason: FormulaUnsupportedReason::SheetNotFound,
            };
        };
        let Some(cell) = sheet.cell(row, col) else {
            return FormulaEvaluation::Computed(Cell::Text(String::new()));
        };
        let Cell::Formula { formula: _, cached } = cell else {
            return FormulaEvaluation::Computed(cell.clone());
        };
        let mut state = EvalState::default();
        match evaluate_cell_inner(self, sheet.name.as_str(), row, col, &mut state) {
            Ok(cell) => FormulaEvaluation::Computed(cell),
            Err(reason) => FormulaEvaluation::Fallback {
                cached: (**cached).clone(),
                reason,
            },
        }
    }
}

#[derive(Debug, Default)]
struct EvalState {
    visiting: HashSet<(String, u32, u16)>,
    memo: HashMap<(String, u32, u16), Cell>,
}

fn evaluate_cell_inner(
    workbook: &Workbook,
    sheet_name: &str,
    row: u32,
    col: u16,
    state: &mut EvalState,
) -> std::result::Result<Cell, FormulaUnsupportedReason> {
    let key = (sheet_name.to_string(), row, col);
    if let Some(cell) = state.memo.get(&key) {
        return Ok(cell.clone());
    }
    if !state.visiting.insert(key.clone()) {
        return Err(FormulaUnsupportedReason::CircularReference);
    }
    let result = (|| {
        let sheet = workbook
            .sheet_by_name(sheet_name)
            .ok_or(FormulaUnsupportedReason::SheetNotFound)?;
        let Some(cell) = sheet.cell(row, col) else {
            return Ok(Cell::Text(String::new()));
        };
        match cell {
            Cell::Formula { formula, .. } => {
                evaluate_formula_with_refs(formula, |request| match request {
                    RefRequest::Cell { sheet, reference } => {
                        let target_sheet_name = sheet.as_deref().unwrap_or(sheet_name);
                        let target_sheet = workbook
                            .sheet_by_name(target_sheet_name)
                            .ok_or(FormulaUnsupportedReason::SheetNotFound)?;
                        let (ref_row, ref_col) = parse_a1_ref(&reference)
                            .ok_or(FormulaUnsupportedReason::UnparsableExpression)?;
                        if target_sheet.cell(ref_row, ref_col).is_none() {
                            Ok(Value::Blank)
                        } else {
                            let cell = evaluate_cell_inner(
                                workbook,
                                target_sheet_name,
                                ref_row,
                                ref_col,
                                state,
                            )?;
                            Ok(cell_to_value(&cell))
                        }
                    }
                    RefRequest::Range { sheet, start, end } => {
                        let target_sheet_name = sheet.as_deref().unwrap_or(sheet_name);
                        let target_sheet = workbook
                            .sheet_by_name(target_sheet_name)
                            .ok_or(FormulaUnsupportedReason::SheetNotFound)?;
                        let (raw_start_row, raw_start_col) = parse_a1_ref(&start)
                            .ok_or(FormulaUnsupportedReason::UnparsableExpression)?;
                        let (raw_end_row, raw_end_col) = parse_a1_ref(&end)
                            .ok_or(FormulaUnsupportedReason::UnparsableExpression)?;
                        let start_row = raw_start_row.min(raw_end_row);
                        let end_row = raw_start_row.max(raw_end_row);
                        let start_col = raw_start_col.min(raw_end_col);
                        let end_col = raw_start_col.max(raw_end_col);
                        let row_count = end_row.saturating_sub(start_row).saturating_add(1) as u64;
                        let col_count = end_col.saturating_sub(start_col).saturating_add(1) as u64;
                        let total_cells = row_count.saturating_mul(col_count);
                        if total_cells > MAX_RANGE_CELLS {
                            return Err(FormulaUnsupportedReason::RangeTooLarge);
                        }
                        let mut values = Vec::with_capacity(total_cells as usize);
                        for r in start_row..=end_row {
                            for c in start_col..=end_col {
                                let value = if target_sheet.cell(r, c).is_some() {
                                    let cell = evaluate_cell_inner(
                                        workbook,
                                        target_sheet_name,
                                        r,
                                        c,
                                        state,
                                    )?;
                                    cell_to_value(&cell)
                                } else {
                                    Value::Blank
                                };
                                values.push(value);
                            }
                        }
                        Ok(Value::Range(values))
                    }
                })
            }
            _ => Ok(cell.clone()),
        }
    })();
    state.visiting.remove(&key);
    if let Ok(cell) = &result {
        state.memo.insert(key, cell.clone());
    }
    result
}

fn evaluate_formula_with_refs(
    formula: &str,
    mut resolve_ref: impl FnMut(RefRequest) -> std::result::Result<Value, FormulaUnsupportedReason>,
) -> std::result::Result<Cell, FormulaUnsupportedReason> {
    let formula = formula.trim().strip_prefix('=').unwrap_or(formula.trim());
    if formula.contains('[') {
        return Err(FormulaUnsupportedReason::ExternalRef);
    }
    let mut parser = Parser::new(formula, &mut resolve_ref);
    let value = parser.parse_comparison()?;
    parser.skip_ws();
    if parser.eof() {
        Ok(value.into_cell())
    } else {
        Err(FormulaUnsupportedReason::UnparsableExpression)
    }
}

fn cell_to_value(cell: &Cell) -> Value {
    match cell {
        Cell::Text(s) => Value::Text(s.clone()),
        Cell::Number(n) | Cell::Date(n) => Value::Number(*n),
        Cell::Bool(b) => Value::Bool(*b),
        Cell::Error(e) => Value::Error(e.clone()),
        Cell::Formula { cached, .. } => cell_to_value(cached),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Value {
    Number(f64),
    Text(String),
    Bool(bool),
    Error(String),
    Blank,
    Range(Vec<Value>),
}

impl Value {
    fn into_cell(self) -> Cell {
        match self {
            Value::Number(n) => Cell::Number(n),
            Value::Text(s) => Cell::Text(s),
            Value::Bool(b) => Cell::Bool(b),
            Value::Error(e) => Cell::Error(e),
            Value::Blank => Cell::Text(String::new()),
            Value::Range(_) => Cell::Error("#VALUE!".to_string()),
        }
    }

    fn as_number(&self) -> std::result::Result<f64, Value> {
        match self {
            Value::Number(n) => Ok(*n),
            Value::Blank => Ok(0.0),
            Value::Bool(b) => Ok(if *b { 1.0 } else { 0.0 }),
            Value::Text(s) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| Value::Error("#VALUE!".to_string())),
            Value::Error(e) => Err(Value::Error(e.clone())),
            Value::Range(_) => Err(Value::Error("#VALUE!".to_string())),
        }
    }

    fn as_text(&self) -> std::result::Result<String, Value> {
        match self {
            Value::Number(n) => Ok(crate::format_number(*n)),
            Value::Text(s) => Ok(s.clone()),
            Value::Blank => Ok(String::new()),
            Value::Bool(b) => Ok(if *b { "TRUE" } else { "FALSE" }.to_string()),
            Value::Error(e) => Err(Value::Error(e.clone())),
            Value::Range(_) => Err(Value::Error("#VALUE!".to_string())),
        }
    }

    fn as_bool(&self) -> std::result::Result<bool, Value> {
        match self {
            Value::Bool(b) => Ok(*b),
            Value::Number(n) => {
                if n.is_finite() {
                    Ok(*n != 0.0)
                } else {
                    Err(Value::Error("#NUM!".to_string()))
                }
            }
            Value::Text(text) => {
                let text = text.trim();
                if text.is_empty() || text.eq_ignore_ascii_case("FALSE") {
                    Ok(false)
                } else if text.eq_ignore_ascii_case("TRUE") {
                    Ok(true)
                } else {
                    match text.parse::<f64>() {
                        Ok(n) if n.is_finite() => Ok(n != 0.0),
                        Ok(_) => Err(Value::Error("#NUM!".to_string())),
                        Err(_) => Err(Value::Error("#VALUE!".to_string())),
                    }
                }
            }
            Value::Blank => Ok(false),
            Value::Error(e) => Err(Value::Error(e.clone())),
            Value::Range(_) => Err(Value::Error("#VALUE!".to_string())),
        }
    }
}

enum RefRequest {
    Cell {
        sheet: Option<String>,
        reference: String,
    },
    Range {
        sheet: Option<String>,
        start: String,
        end: String,
    },
}

struct Parser<'a, 'r> {
    input: &'a str,
    pos: usize,
    depth: usize,
    resolve_ref:
        &'r mut dyn FnMut(RefRequest) -> std::result::Result<Value, FormulaUnsupportedReason>,
}

impl<'a, 'r> Parser<'a, 'r> {
    fn new(
        input: &'a str,
        resolve_ref: &'r mut dyn FnMut(
            RefRequest,
        )
            -> std::result::Result<Value, FormulaUnsupportedReason>,
    ) -> Self {
        Self {
            input,
            pos: 0,
            depth: 0,
            resolve_ref,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Enter a bounded recursion level. Must be paired with `exit_depth`
    /// whenever this returns `Ok`. Returns a typed error instead of letting
    /// adversarially nested input (deep parens, nested function calls, or
    /// long unary-operator chains) recurse past a safe stack budget.
    fn enter_depth(&mut self) -> std::result::Result<(), FormulaUnsupportedReason> {
        self.depth += 1;
        if self.depth > MAX_EXPR_DEPTH {
            self.depth -= 1;
            return Err(FormulaUnsupportedReason::ExpressionTooComplex);
        }
        Ok(())
    }

    fn exit_depth(&mut self) {
        self.depth -= 1;
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(c) if c.is_whitespace()) {
            self.bump();
        }
    }

    fn parse_comparison(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        let mut left = self.parse_concat()?;
        loop {
            self.skip_ws();
            let Some(op) = self.consume_comparison_op() else {
                return Ok(left);
            };
            let right = self.parse_concat()?;
            left = compare_values(left, right, op);
        }
    }

    fn parse_concat(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        let mut left = self.parse_add()?;
        loop {
            self.skip_ws();
            if !self.consume_char('&') {
                return Ok(left);
            }
            let right = self.parse_add()?;
            left = binary_text(left, right);
        }
    }

    fn parse_add(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        let mut left = self.parse_mul()?;
        loop {
            self.skip_ws();
            if self.consume_char('+') {
                left = binary_number(left, self.parse_mul()?, |a, b| a + b);
            } else if self.consume_char('-') {
                left = binary_number(left, self.parse_mul()?, |a, b| a - b);
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_mul(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        let mut left = self.parse_power()?;
        loop {
            self.skip_ws();
            if self.consume_char('*') {
                left = binary_number(left, self.parse_power()?, |a, b| a * b);
            } else if self.consume_char('/') {
                let right = self.parse_power()?;
                left = match right.as_number() {
                    Ok(0.0) => Value::Error("#DIV/0!".to_string()),
                    Ok(_) => binary_number(left, right, |a, b| a / b),
                    Err(e) => e,
                };
            } else {
                return Ok(left);
            }
        }
    }

    fn parse_power(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        // Excel evaluates chained '^' left-to-right ("2^3^2" == (2^3)^2 ==
        // 64), so fold left-associatively instead of self-recursing on the
        // right operand (which both mis-associated and made the operator an
        // unbounded adversarial-recursion path).
        let mut left = self.parse_unary()?;
        loop {
            self.skip_ws();
            if !self.consume_char('^') {
                return Ok(left);
            }
            let right = self.parse_unary()?;
            left = binary_power(left, right);
        }
    }

    fn parse_unary(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        self.enter_depth()?;
        let result = self.parse_unary_inner();
        self.exit_depth();
        result
    }

    fn parse_unary_inner(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        self.skip_ws();
        if self.consume_char('+') {
            return self.parse_unary();
        }
        if self.consume_char('-') {
            return Ok(match self.parse_unary()?.as_number() {
                Ok(n) => Value::Number(-n),
                Err(e) => e,
            });
        }
        self.parse_postfix()
    }

    fn parse_postfix(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        let mut value = self.parse_primary()?;
        loop {
            self.skip_ws();
            if self.consume_char('%') {
                value = match value.as_number() {
                    Ok(n) => Value::Number(n / 100.0),
                    Err(e) => e,
                };
            } else {
                return Ok(value);
            }
        }
    }

    fn parse_primary(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        // Bound recursion here: this is the single re-entry point for both
        // parenthesized sub-expressions ('(' below) and function-call
        // arguments (parsed via parse_comparison in parse_identifier), so
        // guarding it catches adversarial nesting from either path.
        self.enter_depth()?;
        let result = self.parse_primary_inner();
        self.exit_depth();
        result
    }

    fn parse_primary_inner(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        self.skip_ws();
        match self.peek() {
            Some('"') => self.parse_string().map(Value::Text),
            Some('\'') => self.parse_quoted_sheet_reference(),
            Some('#') => Ok(Value::Error(self.parse_error())),
            Some('0'..='9') | Some('.') => self.parse_number().map(Value::Number),
            Some('(') => {
                self.bump();
                let value = self.parse_comparison()?;
                self.skip_ws();
                if self.consume_char(')') {
                    Ok(value)
                } else {
                    Err(FormulaUnsupportedReason::UnparsableExpression)
                }
            }
            Some(c) if c.is_ascii_alphabetic() || c == '_' => self.parse_identifier(),
            _ => Err(FormulaUnsupportedReason::UnparsableExpression),
        }
    }

    fn parse_identifier(&mut self) -> std::result::Result<Value, FormulaUnsupportedReason> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '.') {
            self.bump();
        }
        let ident = self.input[start..self.pos].to_string();
        let ident_upper = ident.to_ascii_uppercase();
        self.skip_ws();
        if self.consume_char('!') {
            return self.parse_sheet_reference(ident);
        }
        if self.consume_char('(') {
            if is_volatile(&ident_upper) {
                return Err(FormulaUnsupportedReason::Volatile);
            }
            if !is_deterministic_function(&ident_upper) {
                return Err(FormulaUnsupportedReason::UnsupportedFunction);
            };
            let mut args = Vec::new();
            self.skip_ws();
            if self.consume_char(')') {
                return evaluate_function(&ident_upper, &args);
            }
            loop {
                args.push(self.parse_comparison()?);
                self.skip_ws();
                if self.consume_char(')') {
                    return evaluate_function(&ident_upper, &args);
                }
                if self.consume_char(',') {
                    continue;
                }
                return Err(FormulaUnsupportedReason::UnparsableExpression);
            }
        }
        if self.consume_char(':') {
            self.skip_ws();
            let end = self.parse_a1_reference()?;
            return (self.resolve_ref)(RefRequest::Range {
                sheet: None,
                start: ident.clone(),
                end,
            });
        }
        if parse_a1_ref(&ident).is_some() {
            return (self.resolve_ref)(RefRequest::Cell {
                sheet: None,
                reference: ident,
            });
        }
        match ident_upper.as_str() {
            "TRUE" => Ok(Value::Bool(true)),
            "FALSE" => Ok(Value::Bool(false)),
            _ => Err(FormulaUnsupportedReason::UnresolvedName),
        }
    }

    fn parse_quoted_sheet_reference(
        &mut self,
    ) -> std::result::Result<Value, FormulaUnsupportedReason> {
        if !self.consume_char('\'') {
            return Err(FormulaUnsupportedReason::UnparsableExpression);
        }
        let mut sheet = String::new();
        loop {
            match self.bump() {
                Some('\'') if self.consume_char('\'') => sheet.push('\''),
                Some('\'') => break,
                Some(c) => sheet.push(c),
                None => return Err(FormulaUnsupportedReason::UnparsableExpression),
            }
        }
        self.skip_ws();
        if !self.consume_char('!') {
            return Err(FormulaUnsupportedReason::UnparsableExpression);
        }
        self.parse_sheet_reference(sheet)
    }

    fn parse_sheet_reference(
        &mut self,
        sheet: String,
    ) -> std::result::Result<Value, FormulaUnsupportedReason> {
        self.skip_ws();
        let start = self.parse_a1_reference()?;
        self.skip_ws();
        if self.consume_char(':') {
            self.skip_ws();
            let end = self.parse_a1_reference()?;
            return (self.resolve_ref)(RefRequest::Range {
                sheet: Some(sheet),
                start,
                end,
            });
        }
        (self.resolve_ref)(RefRequest::Cell {
            sheet: Some(sheet),
            reference: start,
        })
    }

    fn parse_a1_reference(&mut self) -> std::result::Result<String, FormulaUnsupportedReason> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '$') {
            self.bump();
        }
        if start == self.pos {
            return Err(FormulaUnsupportedReason::UnparsableExpression);
        }
        let reference = self.input[start..self.pos].to_string();
        if parse_a1_ref(&reference).is_none() {
            Err(FormulaUnsupportedReason::UnparsableExpression)
        } else {
            Ok(reference)
        }
    }

    fn parse_string(&mut self) -> std::result::Result<String, FormulaUnsupportedReason> {
        if !self.consume_char('"') {
            return Err(FormulaUnsupportedReason::UnparsableExpression);
        }
        let mut out = String::new();
        loop {
            match self.bump() {
                Some('"') if self.consume_char('"') => out.push('"'),
                Some('"') => return Ok(out),
                Some(c) => out.push(c),
                None => return Err(FormulaUnsupportedReason::UnparsableExpression),
            }
        }
    }

    fn parse_number(&mut self) -> std::result::Result<f64, FormulaUnsupportedReason> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_digit() || matches!(c, '.' | 'e' | 'E' | '+' | '-'))
        {
            let c = self.peek().unwrap_or_default();
            if matches!(c, '+' | '-') && self.pos > start {
                let prev = self.input[..self.pos]
                    .chars()
                    .next_back()
                    .unwrap_or_default();
                if !matches!(prev, 'e' | 'E') {
                    break;
                }
            }
            self.bump();
        }
        self.input[start..self.pos]
            .parse()
            .map_err(|_| FormulaUnsupportedReason::UnparsableExpression)
    }

    fn parse_error(&mut self) -> String {
        // Try the fixed canonical error literals first (greedy exact match
        // at the current position) so #N/A and #DIV/0! -- the two literals
        // that contain a '/', which the generic operator-stopping scan below
        // would otherwise cut short -- parse correctly. Only fall back to
        // the generic scan for an unrecognized "#..." token.
        for literal in error_literals() {
            if self.input[self.pos..].starts_with(literal) {
                self.pos += literal.len();
                return literal.to_string();
            }
        }
        let start = self.pos;
        while matches!(self.peek(), Some(c) if !c.is_whitespace() && !matches!(c, ')' | ',' | '+' | '-' | '*' | '/' | '^' | '&' | '=' | '<' | '>'))
        {
            self.bump();
        }
        self.input[start..self.pos].to_string()
    }

    fn consume_comparison_op(&mut self) -> Option<CompareOp> {
        for (text, op) in [
            ("<>", CompareOp::Ne),
            ("<=", CompareOp::Le),
            (">=", CompareOp::Ge),
            ("=", CompareOp::Eq),
            ("<", CompareOp::Lt),
            (">", CompareOp::Gt),
        ] {
            if self.input[self.pos..].starts_with(text) {
                self.pos += text.len();
                return Some(op);
            }
        }
        None
    }

    fn consume_char(&mut self, needle: char) -> bool {
        if self.peek() == Some(needle) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn peek(&self) -> Option<char> {
        self.input[self.pos..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }
}

#[derive(Debug, Clone, Copy)]
enum CompareOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

fn is_deterministic_function(ident: &str) -> bool {
    matches!(
        ident,
        "SUM"
            | "MIN"
            | "MAX"
            | "AVERAGE"
            | "COUNT"
            | "COUNTA"
            | "IF"
            | "ROUND"
            | "ROUNDUP"
            | "ROUNDDOWN"
            | "ABS"
            | "INT"
            | "MOD"
            | "LEN"
            | "TRIM"
            | "UPPER"
            | "LOWER"
            | "LEFT"
            | "RIGHT"
            | "MID"
            | "CONCATENATE"
            | "AND"
            | "OR"
            | "NOT"
            | "ISNA"
            | "ISERROR"
            | "ISNUMBER"
            | "ISTEXT"
            | "ISBLANK"
            | "EXACT"
            | "VALUE"
            | "TRUNC"
            | "SIGN"
            | "SQRT"
            | "POWER"
            | "PRODUCT"
    )
}

fn evaluate_function(
    ident: &str,
    args: &[Value],
) -> std::result::Result<Value, FormulaUnsupportedReason> {
    match ident {
        "SUM" => Ok(eval_sum(args)),
        "MIN" => Ok(eval_number_fold(args, f64::INFINITY, f64::min)),
        "MAX" => Ok(eval_number_fold(args, f64::NEG_INFINITY, f64::max)),
        "AVERAGE" => Ok(eval_average(args)),
        "COUNT" => Ok(eval_count(args)),
        "COUNTA" => Ok(eval_counta(args)),
        "IF" => eval_if(args),
        "ROUND" => eval_round(args, RoundKind::Standard),
        "ROUNDUP" => eval_round(args, RoundKind::AwayFromZero),
        "ROUNDDOWN" => eval_round(args, RoundKind::TowardZero),
        "ABS" => eval_unary_numeric(args, f64::abs),
        "INT" => eval_unary_numeric(args, f64::trunc),
        "TRUNC" => eval_trunc(args),
        "SIGN" => eval_sign(args),
        "SQRT" => eval_sqrt(args),
        "POWER" => eval_power(args),
        "PRODUCT" => Ok(eval_product(args)),
        "MOD" => eval_mod(args),
        "LEN" => eval_len(args),
        "TRIM" => eval_trim(args),
        "UPPER" => eval_case(args, |s| s.to_uppercase()),
        "LOWER" => eval_case(args, |s| s.to_lowercase()),
        "LEFT" => eval_left_or_right(args, true),
        "RIGHT" => eval_left_or_right(args, false),
        "MID" => eval_mid(args),
        "CONCATENATE" => Ok(eval_concat(args)),
        "AND" => eval_and_or_or(args, false),
        "OR" => eval_and_or_or(args, true),
        "NOT" => eval_not(args),
        "ISNA" => {
            if args.len() != 1 {
                Ok(Value::Error("#VALUE!".to_string()))
            } else {
                Ok(eval_error_check(args, "#N/A"))
            }
        }
        "ISERROR" => {
            if args.len() != 1 {
                Ok(Value::Error("#VALUE!".to_string()))
            } else {
                Ok(eval_any_error(args))
            }
        }
        "ISNUMBER" => {
            if args.len() != 1 {
                Ok(Value::Error("#VALUE!".to_string()))
            } else {
                Ok(eval_is_number(args))
            }
        }
        "ISTEXT" => {
            if args.len() != 1 {
                Ok(Value::Error("#VALUE!".to_string()))
            } else {
                Ok(eval_is_text(args))
            }
        }
        "ISBLANK" => {
            if args.len() != 1 {
                Ok(Value::Error("#VALUE!".to_string()))
            } else {
                Ok(eval_is_blank(args))
            }
        }
        "EXACT" => eval_exact(args),
        "VALUE" => eval_value(args),
        _ => Err(FormulaUnsupportedReason::UnsupportedFunction),
    }
}

#[derive(Clone, Copy)]
enum RoundKind {
    Standard,
    AwayFromZero,
    TowardZero,
}

fn for_each_value<'a>(
    values: &'a [Value],
    visit: &mut impl FnMut(&'a Value) -> std::result::Result<(), Value>,
) -> std::result::Result<(), Value> {
    for value in values {
        match value {
            Value::Range(values) => for_each_value(values, visit)?,
            value => visit(value)?,
        }
    }
    Ok(())
}

fn aggregate_number(value: &Value) -> std::result::Result<Option<f64>, Value> {
    match value {
        Value::Number(number) => Ok(Some(*number)),
        Value::Bool(number) => Ok(Some(if *number { 1.0 } else { 0.0 })),
        Value::Text(text) => {
            let text = text.trim();
            if text.is_empty() {
                Ok(None)
            } else {
                Ok(text.parse::<f64>().ok())
            }
        }
        Value::Blank => Ok(None),
        Value::Error(error) => Err(Value::Error(error.clone())),
        Value::Range(_) => Err(Value::Error("#VALUE!".to_string())),
    }
}

fn count_if_number_like(value: &Value) -> std::result::Result<bool, Value> {
    match value {
        Value::Number(_) => Ok(true),
        Value::Text(text) => Ok(!text.trim().is_empty() && text.parse::<f64>().is_ok()),
        Value::Blank => Ok(false),
        Value::Error(error) => Err(Value::Error(error.clone())),
        Value::Bool(_) | Value::Range(_) => Ok(false),
    }
}

fn eval_sum(args: &[Value]) -> Value {
    let mut total = 0.0;
    let mut have_any = false;
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        if let Some(number) = aggregate_number(value)? {
            total += number;
            have_any = true;
        }
        Ok(())
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return error;
    }
    if have_any {
        Value::Number(total)
    } else {
        Value::Number(0.0)
    }
}

fn eval_number_fold(args: &[Value], init: f64, f: fn(f64, f64) -> f64) -> Value {
    let mut acc = init;
    let mut seen = false;
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        if let Some(number) = aggregate_number(value)? {
            acc = f(acc, number);
            seen = true;
        }
        Ok(())
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return error;
    }
    if seen {
        Value::Number(acc)
    } else {
        Value::Error("#NUM!".to_string())
    }
}

fn eval_average(args: &[Value]) -> Value {
    let mut total = 0.0;
    let mut count = 0;
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        if let Some(number) = aggregate_number(value)? {
            total += number;
            count += 1;
        }
        Ok(())
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return error;
    }
    if count == 0 {
        Value::Error("#DIV/0!".to_string())
    } else {
        Value::Number(total / count as f64)
    }
}

fn eval_count(args: &[Value]) -> Value {
    let mut count = 0;
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        if count_if_number_like(value)? {
            count += 1;
        }
        Ok(())
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return error;
    }

    Value::Number(count as f64)
}

fn eval_counta(args: &[Value]) -> Value {
    let mut count = 0;
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        match value {
            Value::Blank => Ok(()),
            Value::Error(error) => Err(Value::Error(error.clone())),
            _ => {
                count += 1;
                Ok(())
            }
        }
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return error;
    }
    Value::Number(count as f64)
}

fn eval_if(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    if !(2..=3).contains(&args.len()) {
        return Ok(Value::Error("#VALUE!".to_string()));
    }
    let condition = match args[0].as_bool() {
        Ok(condition) => condition,
        Err(error) => return Ok(error),
    };
    let true_value = args.get(1).cloned().unwrap_or(Value::Bool(false));
    let false_value = args.get(2).cloned().unwrap_or(Value::Bool(false));
    Ok(if condition { true_value } else { false_value })
}

fn eval_round(
    args: &[Value],
    kind: RoundKind,
) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let number = match required_number(args, 0) {
        Ok(number) => number,
        Err(error) => return Ok(error),
    };
    let digits = match required_number(args, 1) {
        Ok(digits) => digits,
        Err(error) => return Ok(error),
    };
    let precision = digits.trunc() as i32;
    let factor = 10_f64.powi(precision);
    let rounded = match kind {
        RoundKind::Standard => (number * factor).round() / factor,
        RoundKind::AwayFromZero => {
            if number >= 0.0 {
                (number * factor).ceil() / factor
            } else {
                (number * factor).floor() / factor
            }
        }
        RoundKind::TowardZero => {
            if number >= 0.0 {
                (number * factor).floor() / factor
            } else {
                (number * factor).ceil() / factor
            }
        }
    };
    // A very negative precision underflows `factor` to exactly 0.0 (still
    // finite), which turns the division above into 0.0/0.0 == NaN. Checking
    // the *final* result (rather than just `factor.is_finite()`) catches
    // that case -- and any other path to a non-finite result -- uniformly.
    if rounded.is_finite() {
        Ok(Value::Number(rounded))
    } else {
        Ok(Value::Error("#NUM!".to_string()))
    }
}

fn eval_unary_numeric(
    args: &[Value],
    f: fn(f64) -> f64,
) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let number = match required_number(args, 0) {
        Ok(number) => number,
        Err(error) => return Ok(error),
    };
    Ok(Value::Number(f(number)))
}

fn eval_trunc(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let number = match required_number(args, 0) {
        Ok(number) => number,
        Err(error) => return Ok(error),
    };
    let digits = match args.get(1) {
        Some(value) => match value.as_number() {
            Ok(value) => value.trunc() as i32,
            Err(error) => return Ok(error),
        },
        None => 0,
    };
    let factor = 10_f64.powi(digits);
    let result = (number * factor).trunc() / factor;
    // See eval_round: check the final result's finiteness, not just the
    // factor's, so an underflowed factor (very negative `digits`) can't
    // sneak a NaN into a Cell::Number.
    if result.is_finite() {
        Ok(Value::Number(result))
    } else {
        Ok(Value::Error("#NUM!".to_string()))
    }
}

fn eval_sqrt(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let number = match required_number(args, 0) {
        Ok(number) => number,
        Err(error) => return Ok(error),
    };
    if number < 0.0 {
        Ok(Value::Error("#NUM!".to_string()))
    } else {
        Ok(Value::Number(number.sqrt()))
    }
}

fn eval_sign(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let number = match required_number(args, 0) {
        Ok(number) => number,
        Err(error) => return Ok(error),
    };
    Ok(Value::Number(if number > 0.0 {
        1.0
    } else if number < 0.0 {
        -1.0
    } else {
        0.0
    }))
}

fn eval_power(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let number = match required_number(args, 0) {
        Ok(number) => number,
        Err(error) => return Ok(error),
    };
    let power = match required_number(args, 1) {
        Ok(power) => power,
        Err(error) => return Ok(error),
    };
    Ok(numeric_power(number, power))
}

fn eval_mod(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let numerator = match required_number(args, 0) {
        Ok(numerator) => numerator,
        Err(error) => return Ok(error),
    };
    let denominator = match required_number(args, 1) {
        Ok(denominator) => denominator,
        Err(error) => return Ok(error),
    };
    if denominator == 0.0 {
        return Ok(Value::Error("#DIV/0!".to_string()));
    }
    // Excel's MOD takes the sign of the divisor (MOD(n,d) == n - d*FLOOR(n/d)),
    // unlike Rust's truncating '%' which takes the sign of the dividend.
    Ok(Value::Number(
        numerator - denominator * (numerator / denominator).floor(),
    ))
}

fn eval_product(args: &[Value]) -> Value {
    let mut product = 1.0;
    let mut have_any = false;
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        if let Some(number) = aggregate_number(value)? {
            product *= number;
            have_any = true;
        }
        Ok(())
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return error;
    }
    if have_any {
        Value::Number(product)
    } else {
        Value::Number(0.0)
    }
}

fn eval_len(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let text = match required_text(args, 0) {
        Ok(text) => text,
        Err(error) => return Ok(error),
    };
    Ok(Value::Number(text.chars().count() as f64))
}

fn eval_trim(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let text = match required_text(args, 0) {
        Ok(text) => text,
        Err(error) => return Ok(error),
    };
    Ok(Value::Text(
        text.split_ascii_whitespace().collect::<Vec<_>>().join(" "),
    ))
}

fn eval_case(
    args: &[Value],
    map: impl FnOnce(String) -> String,
) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let text = match required_text(args, 0) {
        Ok(text) => text,
        Err(error) => return Ok(error),
    };
    Ok(Value::Text(map(text)))
}

fn eval_left_or_right(
    args: &[Value],
    from_left: bool,
) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let text = match required_text(args, 0) {
        Ok(text) => text,
        Err(error) => return Ok(error),
    };
    let mut count = 1usize;
    if let Some(value) = args.get(1) {
        // Match the Result explicitly instead of collapsing it with `.ok()`:
        // an error argument (e.g. from 1/0) must propagate, and non-numeric
        // non-error text must be #VALUE!, not silently fall back to the
        // default count of 1.
        let raw_count = match value.as_number() {
            Ok(raw_count) => raw_count,
            Err(error) => return Ok(error),
        };
        if !raw_count.is_finite() {
            return Ok(Value::Error("#VALUE!".to_string()));
        }
        let rounded = raw_count.trunc();
        if rounded <= 0.0 {
            return Ok(Value::Text(String::new()));
        }
        let chars = text.chars().count() as f64;
        if rounded > chars {
            count = chars as usize;
        } else {
            count = rounded as usize;
        }
    }
    let chars: Vec<char> = text.chars().collect();
    if from_left {
        Ok(Value::Text(chars.into_iter().take(count).collect()))
    } else {
        let skip = chars.len().saturating_sub(count);
        Ok(Value::Text(chars.into_iter().skip(skip).collect()))
    }
}

fn eval_mid(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let text = match required_text(args, 0) {
        Ok(text) => text,
        Err(error) => return Ok(error),
    };
    let start = match required_number(args, 1) {
        Ok(start) => start,
        Err(error) => return Ok(error),
    };
    let len = match required_number(args, 2) {
        Ok(len) => len,
        Err(error) => return Ok(error),
    };
    if start <= 0.0 || !start.is_finite() || !len.is_finite() {
        return Ok(Value::Error("#VALUE!".to_string()));
    }
    let start = start.trunc() as usize;
    let len = len.trunc() as usize;
    let chars: Vec<char> = text.chars().collect();
    if start == 0 || start > chars.len() || len == 0 {
        return Ok(Value::Text(String::new()));
    }
    let start = start - 1;
    // Clamp defensively: `len` comes from a truncated/saturated f64 with no
    // upper bound, so a huge length literal must not be added to `start`
    // with a raw unchecked '+' (that overflows/panics). Mirror the
    // LEFT/RIGHT clamping style (saturating arithmetic against chars.len()).
    let end = start.saturating_add(len).min(chars.len());
    Ok(Value::Text(chars[start..end].iter().collect()))
}

fn eval_concat(args: &[Value]) -> Value {
    let mut out = String::new();
    for arg in args {
        match arg.as_text() {
            Ok(text) => out.push_str(&text),
            Err(error) => return error,
        }
    }
    Value::Text(out)
}

fn eval_and_or_or(
    args: &[Value],
    any_true: bool,
) -> std::result::Result<Value, FormulaUnsupportedReason> {
    if args.is_empty() {
        return Ok(Value::Error("#VALUE!".to_string()));
    }
    let mut truth_values = Vec::new();
    let mut visit = |value: &Value| -> std::result::Result<(), Value> {
        let value = value.as_bool()?;
        truth_values.push(value);
        Ok(())
    };
    if let Err(error) = for_each_value(args, &mut visit) {
        return Ok(error);
    }
    if any_true {
        Ok(Value::Bool(truth_values.into_iter().any(|value| value)))
    } else {
        Ok(Value::Bool(truth_values.into_iter().all(|value| value)))
    }
}

fn eval_not(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    let Some(value) = args.first() else {
        return Ok(Value::Error("#VALUE!".to_string()));
    };
    let value = match value.as_bool() {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    Ok(Value::Bool(!value))
}

fn eval_error_check(args: &[Value], target: &str) -> Value {
    let Some(value) = args.first() else {
        return Value::Error("#VALUE!".to_string());
    };
    if let Value::Error(error) = value {
        Value::Bool(error == target)
    } else {
        Value::Bool(false)
    }
}

fn eval_any_error(args: &[Value]) -> Value {
    let Some(value) = args.first() else {
        return Value::Error("#VALUE!".to_string());
    };
    Value::Bool(matches!(value, Value::Error(_)))
}

fn eval_is_number(args: &[Value]) -> Value {
    let Some(value) = args.first() else {
        return Value::Error("#VALUE!".to_string());
    };
    let numeric = match value {
        Value::Number(_) => true,
        Value::Text(text) => text.parse::<f64>().is_ok(),
        _ => false,
    };
    Value::Bool(numeric)
}

fn eval_is_text(args: &[Value]) -> Value {
    let Some(value) = args.first() else {
        return Value::Error("#VALUE!".to_string());
    };
    Value::Bool(matches!(value, Value::Text(_)))
}

fn eval_is_blank(args: &[Value]) -> Value {
    let Some(value) = args.first() else {
        return Value::Error("#VALUE!".to_string());
    };
    match value {
        Value::Blank => Value::Bool(true),
        Value::Text(text) => Value::Bool(text.is_empty()),
        _ => Value::Bool(false),
    }
}

fn eval_exact(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    if args.len() != 2 {
        return Ok(Value::Error("#VALUE!".to_string()));
    }
    let left = match args[0].as_text() {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    let right = match args[1].as_text() {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    Ok(Value::Bool(left == right))
}

fn eval_value(args: &[Value]) -> std::result::Result<Value, FormulaUnsupportedReason> {
    if args.len() != 1 {
        return Ok(Value::Error("#VALUE!".to_string()));
    }
    let text = match args[0].as_text() {
        Ok(value) => value,
        Err(error) => return Ok(error),
    };
    match text.trim().parse::<f64>() {
        Ok(number) => Ok(Value::Number(number)),
        Err(_) => Ok(Value::Error("#VALUE!".to_string())),
    }
}

fn binary_number(left: Value, right: Value, f: impl FnOnce(f64, f64) -> f64) -> Value {
    match (left.as_number(), right.as_number()) {
        (Err(e), _) | (_, Err(e)) => e,
        (Ok(a), Ok(b)) => Value::Number(f(a, b)),
    }
}

fn binary_text(left: Value, right: Value) -> Value {
    match (left.as_text(), right.as_text()) {
        (Err(e), _) | (_, Err(e)) => e,
        (Ok(a), Ok(b)) => Value::Text(format!("{a}{b}")),
    }
}

/// Raise `base` to `exponent`, reporting #NUM! for non-finite results
/// instead of silently storing NaN/Infinity. Shared by the '^' operator and
/// POWER() so the two can't drift apart.
fn numeric_power(base: f64, exponent: f64) -> Value {
    let result = base.powf(exponent);
    if result.is_finite() {
        Value::Number(result)
    } else {
        Value::Error("#NUM!".to_string())
    }
}

fn binary_power(left: Value, right: Value) -> Value {
    match (left.as_number(), right.as_number()) {
        (Err(e), _) | (_, Err(e)) => e,
        (Ok(a), Ok(b)) => numeric_power(a, b),
    }
}

/// Fetch a required numeric argument by position, propagating whatever
/// specific error the argument carries (e.g. #DIV/0!) rather than masking it
/// with a generic #VALUE!, which is still what `as_number` itself returns
/// for genuinely non-numeric, non-error input. A missing argument is
/// #VALUE! since every call site here requires it.
fn required_number(args: &[Value], index: usize) -> std::result::Result<f64, Value> {
    match args.get(index) {
        Some(value) => value.as_number(),
        None => Err(Value::Error("#VALUE!".to_string())),
    }
}

/// Text-argument counterpart to `required_number`.
fn required_text(args: &[Value], index: usize) -> std::result::Result<String, Value> {
    match args.get(index) {
        Some(value) => value.as_text(),
        None => Err(Value::Error("#VALUE!".to_string())),
    }
}

/// Type rank Excel uses to order comparison operands when they aren't both
/// the same kind: Number < Text < Logical. Comparison operators (unlike
/// arithmetic) never coerce text to a number, so a number is never equal to
/// or greater than any text regardless of its content.
fn compare_rank(value: &Value) -> u8 {
    match value {
        Value::Number(_) => 0,
        // A blank operand behaves like the number 0 in comparisons, matching
        // how `Value::as_number` already treats blanks everywhere else in
        // this evaluator.
        Value::Blank => 0,
        Value::Text(_) => 1,
        Value::Bool(_) => 2,
        // Errors are propagated before ranking is consulted; Range never
        // reaches a scalar comparison. Rank is irrelevant for either, so
        // give them a stable (unreachable in practice) rank.
        Value::Error(_) | Value::Range(_) => 3,
    }
}

fn compare_values(left: Value, right: Value, op: CompareOp) -> Value {
    if let Value::Error(e) = &left {
        return Value::Error(e.clone());
    }
    if let Value::Error(e) = &right {
        return Value::Error(e.clone());
    }
    // A bare range (e.g. `A1:A3=5`) was never a valid scalar comparison
    // operand -- both `as_number` and `as_text` already rejected it with
    // #VALUE! before this rewrite. Keep that behavior explicitly instead of
    // letting it fall through to rank-based ordering.
    if matches!(left, Value::Range(_)) || matches!(right, Value::Range(_)) {
        return Value::Error("#VALUE!".to_string());
    }
    let ordering = if compare_rank(&left) == compare_rank(&right) {
        match (&left, &right) {
            (Value::Text(a), Value::Text(b)) => {
                Some(a.to_ascii_uppercase().cmp(&b.to_ascii_uppercase()))
            }
            (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
            // Number vs Number, Number vs Blank, or Blank vs Blank: compare
            // numerically (blank reads as 0.0, matching `as_number`).
            _ => match (left.as_number(), right.as_number()) {
                (Ok(a), Ok(b)) => a.partial_cmp(&b),
                (Err(e), _) | (_, Err(e)) => return e,
            },
        }
    } else {
        // Different ranks: use Excel's fixed cross-type ordering, never
        // coercing text to a number for the comparison.
        Some(compare_rank(&left).cmp(&compare_rank(&right)))
    };
    let Some(ordering) = ordering else {
        return Value::Error("#VALUE!".to_string());
    };
    let result = match op {
        CompareOp::Eq => ordering.is_eq(),
        CompareOp::Ne => !ordering.is_eq(),
        CompareOp::Lt => ordering.is_lt(),
        CompareOp::Le => !ordering.is_gt(),
        CompareOp::Gt => ordering.is_gt(),
        CompareOp::Ge => !ordering.is_lt(),
    };
    Value::Bool(result)
}

fn is_volatile(ident: &str) -> bool {
    matches!(
        ident,
        "NOW" | "TODAY" | "RAND" | "RANDBETWEEN" | "INDIRECT" | "OFFSET" | "CELL" | "ADDRESS"
    )
}

fn parse_a1_ref(reference: &str) -> Option<(u32, u16)> {
    let normalized = reference.replace('$', "");
    let reference = normalized.as_str();
    let split = reference.find(|c: char| c.is_ascii_digit())?;
    if split == 0 {
        return None;
    }
    let (letters, digits) = reference.split_at(split);
    if letters.len() > 3 || digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut col = 0u32;
    for b in letters.bytes() {
        if !b.is_ascii_alphabetic() {
            return None;
        }
        col = col * 26 + u32::from(b.to_ascii_uppercase() - b'A' + 1);
    }
    let row = digits.parse::<u32>().ok()?;
    if row == 0 || row > 1_048_576 || col == 0 || col > 16_384 {
        return None;
    }
    Some((row - 1, (col - 1) as u16))
}

#[cfg(test)]
mod tests {
    use super::{FormulaEvaluation, FormulaUnsupportedReason, MAX_EXPR_DEPTH};
    use crate::{Cell, Workbook};

    #[test]
    fn evaluates_literal_arithmetic_formula() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").write_formula(0, 0, "1+2*3", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(7.0))
        );
    }

    #[test]
    fn evaluates_concat_comparison_percent_and_errors() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, r#""a"&"b""#, "");
        sheet.write_formula(0, 1, "10%=0.1", false);
        sheet.write_formula(0, 2, "1/0", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Text("ab".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into()))
        );
    }

    #[test]
    fn unsupported_formula_returns_cached_value_with_reason() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").write_formula(0, 0, "FOO(A1)", 42.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(42.0),
                reason: FormulaUnsupportedReason::UnsupportedFunction,
            }
        );
    }

    #[test]
    fn evaluates_deterministic_range_fns_with_counts_and_average() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 1.0);
        sheet.write(1, 0, 2.0);
        sheet.write(2, 0, "3");
        sheet.write_formula(0, 1, "SUM(A1:A3)", 0.0);
        sheet.write_formula(0, 2, "AVERAGE(A1:A3)", 0.0);
        sheet.write_formula(0, 3, "COUNT(A1:A3)", 0.0);
        sheet.write_formula(0, 4, "COUNTA(A1:A3)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Number(6.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 3),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 4),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
    }

    #[test]
    fn aggregates_ignore_blank_and_non_numeric_range_values() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 1.0);
        sheet.write(1, 0, "2");
        sheet.write(2, 0, "text");
        sheet.write_formula(0, 4, "SUM(A1:A5)", 0.0);
        sheet.write_formula(0, 5, "MIN(A1:A5)", 0.0);
        sheet.write_formula(0, 6, "MAX(A1:A5)", 0.0);
        sheet.write_formula(0, 7, "AVERAGE(A1:A5)", 0.0);
        sheet.write_formula(0, 8, "COUNT(A1:A5)", 0.0);
        sheet.write_formula(0, 9, "COUNTA(A1:A5)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 4),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 5),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 6),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 7),
            FormulaEvaluation::Computed(Cell::Number(1.5))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 8),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 9),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
    }

    #[test]
    fn evaluates_if_text_logical_and_info_functions() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 1.0);
        sheet.write(0, 1, 0.0);
        sheet.write(2, 0, "");
        sheet.write_formula(1, 0, "IF(A1>0,\"pos\",\"neg\")", "");
        sheet.write_formula(1, 1, "IF(A1>0,A1,A2)", 0.0);
        sheet.write_formula(1, 2, "AND(TRUE,FALSE,1>0)", false);
        sheet.write_formula(1, 3, "OR(FALSE,1>0)", false);
        sheet.write_formula(1, 4, "NOT(FALSE)", false);
        sheet.write_formula(1, 5, "ISNA(1/0)", false);
        sheet.write_formula(1, 6, "ISERROR(1/0)", false);
        sheet.write_formula(1, 7, "ISNUMBER(\"3\")", false);
        sheet.write_formula(1, 8, "ISTEXT(A1)", false);
        sheet.write_formula(1, 9, "ISBLANK(A3)", false);

        assert_eq!(
            wb.evaluate_cell("Data", 1, 0),
            FormulaEvaluation::Computed(Cell::Text("pos".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 1),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 2),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 3),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 4),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 5),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 6),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 7),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 8),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 9),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
    }

    #[test]
    fn info_predicates_enforce_arity() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 0.0);
        sheet.write_formula(0, 0, "ISNA(1/0, 0)", false);
        sheet.write_formula(0, 1, "ISERROR(1/0, 0)", false);
        sheet.write_formula(0, 2, "ISNUMBER(1, 2)", false);
        sheet.write_formula(0, 3, "ISTEXT(\"a\", \"b\")", false);
        sheet.write_formula(0, 4, "ISBLANK(A1, B1)", false);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".to_string()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".to_string()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".to_string()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 3),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".to_string()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 4),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".to_string()))
        );
    }

    #[test]
    fn range_reference_refuses_oversized_range() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data")
            .write_formula(0, 0, "SUM(A1:A12001)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::RangeTooLarge,
            }
        );
    }

    #[test]
    fn reversed_ranges_keep_the_full_span() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 1.0);
        sheet.write(0, 1, 2.0);
        sheet.write(0, 2, 3.0);
        sheet.write(1, 4, 4.0);
        sheet.write(2, 4, 5.0);
        sheet.write_formula(1, 0, "SUM(C1:A1)", 0.0);
        sheet.write_formula(1, 1, "SUM(E3:E2)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 1, 0),
            FormulaEvaluation::Computed(Cell::Number(6.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 1),
            FormulaEvaluation::Computed(Cell::Number(9.0))
        );
    }

    #[test]
    fn reversed_range_reference_refuses_oversized_range() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data")
            .write_formula(0, 0, "SUM(A12001:A1)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::RangeTooLarge,
            }
        );
    }

    #[test]
    fn evaluates_sheet_qualified_cell_and_range_refs() {
        let mut wb = Workbook::new();
        {
            let sheet = wb.add_sheet("Rates");
            sheet.write(0, 0, 2.0);
            sheet.write(1, 0, 3.0);
        }
        {
            let sheet = wb.add_sheet("Quoted Sheet");
            sheet.write(0, 1, 4.0);
            sheet.write(1, 1, 5.0);
        }
        {
            let sheet = wb.add_sheet("O'Brien");
            sheet.write(0, 0, 7.0);
        }
        {
            let sheet = wb.add_sheet("Data");
            sheet.write_formula(0, 0, "Rates!A1+Rates!A2", 0.0);
            sheet.write_formula(0, 1, "SUM('Quoted Sheet'!B1:B2)", 0.0);
            sheet.write_formula(0, 2, "'O''Brien'!A1", 0.0);
            sheet.write_formula(0, 3, "Missing!A1", 9.0);
        }

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(5.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Number(9.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Number(7.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 3),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(9.0),
                reason: FormulaUnsupportedReason::SheetNotFound,
            }
        );
    }

    #[test]
    fn evaluates_remaining_mvp_math_and_text_functions() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 2.0);
        sheet.write(1, 0, 3.0);
        sheet.write(2, 0, "text");
        sheet.write_formula(0, 1, "PRODUCT(A1:A3,4)", 0.0);
        sheet.write_formula(0, 2, "POWER(2,3)", 0.0);
        sheet.write_formula(0, 3, "SQRT(9)", 0.0);
        sheet.write_formula(0, 4, "SIGN(-7)", 0.0);
        sheet.write_formula(0, 5, "SIGN(0)", 1.0);
        sheet.write_formula(0, 6, "TRUNC(3.987,2)", 0.0);
        sheet.write_formula(0, 7, "TRUNC(-3.987,1)", 0.0);
        sheet.write_formula(0, 8, "VALUE(\" 42.5 \")", 0.0);
        sheet.write_formula(0, 9, "EXACT(\"Road\",\"Road\")", false);
        sheet.write_formula(0, 10, "EXACT(\"Road\",\"road\")", true);
        sheet.write_formula(1, 1, "SQRT(-1)", 0.0);
        sheet.write_formula(1, 2, "VALUE(\"not a number\")", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Number(24.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Number(8.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 3),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 4),
            FormulaEvaluation::Computed(Cell::Number(-1.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 5),
            FormulaEvaluation::Computed(Cell::Number(0.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 6),
            FormulaEvaluation::Computed(Cell::Number(3.98))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 7),
            FormulaEvaluation::Computed(Cell::Number(-3.9))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 8),
            FormulaEvaluation::Computed(Cell::Number(42.5))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 9),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 10),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 1),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 2),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn evaluates_text_functions() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "LEN(\" ab \")", 0);
        sheet.write_formula(0, 1, "TRIM(\" a   b  \")", 0);
        sheet.write_formula(0, 2, "UPPER(\"Abc\")", 0);
        sheet.write_formula(0, 3, "LOWER(\"aBc\")", 0);
        sheet.write_formula(0, 4, "LEFT(\"abcdef\",3)", 0);
        sheet.write_formula(0, 5, "RIGHT(\"abcdef\",2)", 0);
        sheet.write_formula(0, 6, "MID(\"abcdef\",2,3)", 0);
        sheet.write_formula(0, 7, "CONCATENATE(\"a\",\"bc\",3)", 0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(4.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Text("a b".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Text("ABC".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 3),
            FormulaEvaluation::Computed(Cell::Text("abc".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 4),
            FormulaEvaluation::Computed(Cell::Text("abc".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 5),
            FormulaEvaluation::Computed(Cell::Text("ef".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 6),
            FormulaEvaluation::Computed(Cell::Text("bcd".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 7),
            FormulaEvaluation::Computed(Cell::Text("abc3".into()))
        );
    }

    #[test]
    fn volatile_functions_fallback_to_cached_value() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data")
            .write_formula(0, 0, "TODAY()", Cell::Date(44927.0));

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Date(44927.0),
                reason: FormulaUnsupportedReason::Volatile,
            }
        );
    }

    #[test]
    fn evaluates_same_sheet_reference_chain() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(2, 0, 1.0);
        sheet.write_formula(1, 0, "A3+1", 0.0);
        sheet.write_formula(0, 0, "A2+1", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
    }

    #[test]
    fn detects_same_sheet_formula_cycles() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "A2", 10.0);
        sheet.write_formula(1, 0, "A1", 20.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(10.0),
                reason: FormulaUnsupportedReason::CircularReference,
            }
        );
    }

    // -- Regression: 0158-formula-evaluator-correctness-fixes -------------

    #[test]
    fn mid_with_huge_length_clamps_without_panicking() {
        // BUG 1: an unclamped `start + len` overflowed/panicked for a huge
        // length literal. Excel itself clamps MID's length to whatever text
        // remains after `start`, so the sane result is the clamped
        // substring, not an error -- we assert that value here.
        let mut wb = Workbook::new();
        wb.add_sheet("Data")
            .write_formula(0, 0, "MID(\"abcd\",2,18446744073709551615)", "");

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Text("bcd".into()))
        );
    }

    #[test]
    fn deeply_nested_parens_return_expression_too_complex_not_a_crash() {
        // BUG 2: unbounded recursion in parse_primary's '(' branch could
        // blow the native stack on adversarial input.
        let mut wb = Workbook::new();
        let nesting = 5_000;
        let formula = format!("{}1{}", "(".repeat(nesting), ")".repeat(nesting));
        wb.add_sheet("Data").write_formula(0, 0, formula, 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::ExpressionTooComplex,
            }
        );
    }

    #[test]
    fn deeply_nested_function_calls_return_expression_too_complex_not_a_crash() {
        // BUG 2 sibling path: function-call argument parsing recurses the
        // same way parenthesized expressions do (SUM(SUM(SUM(...)))).
        let mut wb = Workbook::new();
        let nesting = 5_000;
        let formula = format!("{}1{}", "SUM(".repeat(nesting), ")".repeat(nesting));
        wb.add_sheet("Data").write_formula(0, 0, formula, 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::ExpressionTooComplex,
            }
        );
    }

    #[test]
    fn deeply_chained_unary_minus_returns_expression_too_complex_not_a_crash() {
        // BUG 2 sibling path: parse_unary self-recurses for each leading
        // sign without ever touching parse_primary.
        let mut wb = Workbook::new();
        let nesting = 5_000;
        let formula = format!("{}1", "-".repeat(nesting));
        wb.add_sheet("Data").write_formula(0, 0, formula, 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::ExpressionTooComplex,
            }
        );
    }

    #[test]
    fn power_operator_is_left_associative() {
        // BUG 3: '^' self-recursed on the right operand, making "2^3^2"
        // evaluate right-to-left (512) instead of Excel's left-to-right (64).
        let mut wb = Workbook::new();
        wb.add_sheet("Data").write_formula(0, 0, "2^3^2", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(64.0))
        );
    }

    #[test]
    fn power_operator_returns_num_error_for_non_finite_results() {
        // BUG 4: the '^' operator stored NaN/Infinity straight into a
        // Cell::Number instead of reporting #NUM! like POWER() already does.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "(-1)^0.5", 0.0);
        sheet.write_formula(0, 1, "10^400", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
    }

    #[test]
    fn comparison_operators_use_excel_type_ordering_not_numeric_coercion() {
        // BUG 5: comparisons coerced text to numbers before falling back to
        // text comparison, so "10>\"5\"" numerically coerced "5" and
        // returned TRUE. Excel never coerces text for comparisons; it uses a
        // fixed Number < Text < Logical rank.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "10>\"5\"", false);
        sheet.write_formula(0, 1, "5=5", false);
        sheet.write_formula(0, 2, "\"abc\"<TRUE", false);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Bool(false)),
            "a number is never greater than any text"
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Bool(true)),
            "same-rank numeric comparison"
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Bool(true)),
            "text ranks below logical"
        );
    }

    #[test]
    fn comparing_a_bare_range_still_returns_value_error() {
        // Guard against a regression the BUG 5 rank-based rewrite could
        // introduce: a bare range operand (neither Number/Text/Bool) must
        // keep failing with #VALUE!, not silently rank as "greatest".
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 1.0);
        sheet.write(1, 0, 2.0);
        sheet.write_formula(2, 0, "A1:A2=5", false);

        assert_eq!(
            wb.evaluate_cell("Data", 2, 0),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn left_propagates_count_argument_errors_instead_of_defaulting() {
        // BUG 6: the optional count argument's error was swallowed by
        // `.ok()`, silently falling back to the default count of 1.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "LEFT(\"abcdef\",1/0)", "");
        sheet.write_formula(0, 1, "LEFT(\"abcdef\",\"x\")", "");

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn functions_propagate_the_underlying_argument_error_not_a_generic_value_error() {
        // BUG 7: LEN/ABS/SQRT/ROUND (and siblings) discarded the argument's
        // actual error via `.ok()` and substituted a hardcoded #VALUE!.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "LEN(1/0)", 0.0);
        sheet.write_formula(0, 1, "ABS(1/0)", 0.0);
        sheet.write_formula(0, 2, "SQRT(1/0)", 0.0);
        sheet.write_formula(0, 3, "ROUND(1/0,2)", 0.0);

        for col in 0..4u16 {
            assert_eq!(
                wb.evaluate_cell("Data", 0, col),
                FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into())),
                "column {col}"
            );
        }
    }

    #[test]
    fn round_and_trunc_return_num_error_instead_of_nan_for_extreme_precision() {
        // BUG 8: a very negative precision underflows `10^precision` to
        // 0.0, and dividing by that finite-but-zero factor yields NaN.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "ROUND(5,-400)", 0.0);
        sheet.write_formula(0, 1, "TRUNC(5,-400)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
    }

    #[test]
    fn mod_takes_the_sign_of_the_divisor_like_excel() {
        // BUG 9: Rust's `%` takes the sign of the dividend; Excel's MOD
        // takes the sign of the divisor.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "MOD(-7,3)", 0.0);
        sheet.write_formula(0, 1, "MOD(7,-3)", 0.0);
        sheet.write_formula(0, 2, "MOD(5,0)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Number(-2.0))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 2),
            FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into()))
        );
    }

    #[test]
    fn error_literals_containing_a_slash_parse_as_their_canonical_error() {
        // BUG 10: the generic '#...' scan stopped at '/', breaking literal
        // parsing of #N/A and #DIV/0! inside formula text.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "ISNA(#N/A)", false);
        sheet.write_formula(0, 1, "ISERROR(#DIV/0!)", false);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
    }

    // -- WS2: error-propagation table (every dispatched function) ---------

    /// Evaluate `=formula` on a fresh one-cell sheet and return the result.
    fn eval(formula: &str) -> FormulaEvaluation {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").write_formula(0, 0, formula, 0.0);
        wb.evaluate_cell("Data", 0, 0)
    }

    fn assert_div0(formula: &str) {
        assert_eq!(
            eval(formula),
            FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into())),
            "formula {formula:?} should propagate the original #DIV/0! error"
        );
    }

    #[test]
    fn error_propagates_through_aggregate_functions() {
        for formula in [
            "SUM(1/0)",
            "MIN(1/0)",
            "MAX(1/0)",
            "AVERAGE(1/0)",
            "COUNT(1/0)",
            "COUNTA(1/0)",
            "PRODUCT(1/0)",
        ] {
            assert_div0(formula);
        }
    }

    #[test]
    fn error_propagates_through_rounding_functions_both_argument_positions() {
        for func in ["ROUND", "ROUNDUP", "ROUNDDOWN"] {
            assert_div0(&format!("{func}(1/0,2)"));
            assert_div0(&format!("{func}(2,1/0)"));
        }
    }

    #[test]
    fn error_propagates_through_trunc_both_argument_positions() {
        assert_div0("TRUNC(1/0)");
        assert_div0("TRUNC(1/0,2)");
        assert_div0("TRUNC(5,1/0)");
    }

    #[test]
    fn error_propagates_through_unary_math_functions() {
        for func in ["ABS", "INT", "SIGN", "SQRT"] {
            assert_div0(&format!("{func}(1/0)"));
        }
    }

    #[test]
    fn error_propagates_through_mod_both_operands() {
        assert_div0("MOD(1/0,3)");
        assert_div0("MOD(3,1/0)");
    }

    #[test]
    fn error_propagates_through_power_both_operands() {
        assert_div0("POWER(1/0,2)");
        assert_div0("POWER(2,1/0)");
    }

    #[test]
    fn error_propagates_through_text_functions() {
        for formula in [
            "LEN(1/0)",
            "TRIM(1/0)",
            "UPPER(1/0)",
            "LOWER(1/0)",
            "CONCATENATE(1/0)",
            "VALUE(1/0)",
        ] {
            assert_div0(formula);
        }
    }

    #[test]
    fn error_propagates_through_left_right_mid_all_argument_positions() {
        assert_div0("LEFT(1/0)");
        assert_div0("LEFT(\"abc\",1/0)");
        assert_div0("RIGHT(1/0)");
        assert_div0("RIGHT(\"abc\",1/0)");
        assert_div0("MID(1/0,1,1)");
        assert_div0("MID(\"abc\",1/0,1)");
        assert_div0("MID(\"abc\",1,1/0)");
    }

    #[test]
    fn error_propagates_through_logical_functions() {
        assert_div0("AND(1/0)");
        assert_div0("AND(TRUE,1/0)");
        assert_div0("OR(1/0)");
        assert_div0("OR(FALSE,1/0)");
        assert_div0("NOT(1/0)");
    }

    #[test]
    fn error_propagates_through_exact_both_operands() {
        assert_div0("EXACT(1/0,\"a\")");
        assert_div0("EXACT(\"a\",1/0)");
    }

    #[test]
    fn if_condition_error_propagates_but_untaken_branch_error_does_not() {
        // The condition's error always propagates.
        assert_div0("IF(1/0,1,2)");
        // The evaluator evaluates every argument eagerly (it isn't lazy like
        // real Excel), but IF still only *returns* the selected branch, so
        // an error sitting in the branch that was NOT selected never
        // surfaces in the result -- matching Excel's observable behavior
        // even though the mechanism differs.
        assert_eq!(
            eval("IF(FALSE,1/0,5)"),
            FormulaEvaluation::Computed(Cell::Number(5.0))
        );
        // The SELECTED branch's error is returned as-is.
        assert_div0("IF(TRUE,1/0,5)");
    }

    #[test]
    fn is_functions_classify_errors_instead_of_propagating() {
        // ISNA/ISERROR/ISNUMBER/ISTEXT/ISBLANK all inspect an error
        // argument's *type*; none of them propagate the error itself.
        assert_eq!(
            eval("ISNA(1/0)"),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            eval("ISNA(#N/A)"),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            eval("ISERROR(1/0)"),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            eval("ISNUMBER(1/0)"),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            eval("ISTEXT(1/0)"),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            eval("ISBLANK(1/0)"),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
    }

    // -- WS2: coercion tables ----------------------------------------------

    #[test]
    fn arithmetic_coerces_blank_to_zero() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        // A1 is left unwritten (blank); the formula lives in a different
        // cell so it isn't a self-reference.
        sheet.write_formula(1, 0, "A1+5", 0.0);
        assert_eq!(
            wb.evaluate_cell("Data", 1, 0),
            FormulaEvaluation::Computed(Cell::Number(5.0))
        );
    }

    #[test]
    fn arithmetic_coerces_bool_to_one_or_zero() {
        assert_eq!(
            eval("TRUE+1"),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
        assert_eq!(
            eval("FALSE+1"),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
        assert_eq!(
            eval("TRUE*3"),
            FormulaEvaluation::Computed(Cell::Number(3.0))
        );
    }

    #[test]
    fn arithmetic_coerces_numeric_text_to_number() {
        assert_eq!(
            eval("\"3\"+2"),
            FormulaEvaluation::Computed(Cell::Number(5.0))
        );
        assert_eq!(
            eval("\" 3 \"+2"),
            FormulaEvaluation::Computed(Cell::Number(5.0)),
            "numeric text is trimmed before parsing"
        );
    }

    #[test]
    fn arithmetic_rejects_non_numeric_text_with_value_error() {
        assert_eq!(
            eval("\"abc\"+1"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn concat_operator_coerces_number_bool_blank_to_text() {
        assert_eq!(
            eval("1&\"x\""),
            FormulaEvaluation::Computed(Cell::Text("1x".into()))
        );
        assert_eq!(
            eval("TRUE&\"x\""),
            FormulaEvaluation::Computed(Cell::Text("TRUEx".into()))
        );
        assert_eq!(
            eval("FALSE&\"x\""),
            FormulaEvaluation::Computed(Cell::Text("FALSEx".into()))
        );
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        // A1 stays blank; the formula lives elsewhere to avoid self-reference.
        sheet.write_formula(1, 0, "A1&\"y\"", "");
        assert_eq!(
            wb.evaluate_cell("Data", 1, 0),
            FormulaEvaluation::Computed(Cell::Text("y".into()))
        );
    }

    #[test]
    fn comparison_number_vs_text_all_six_operators() {
        // Excel's fixed type rank (Number < Text) means a number is NEVER
        // equal to or greater than any text, regardless of content.
        let cases: [(&str, bool); 6] = [
            ("10=\"5\"", false),
            ("10<>\"5\"", true),
            ("10<\"5\"", true),
            ("10<=\"5\"", true),
            ("10>\"5\"", false),
            ("10>=\"5\"", false),
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval(formula),
                FormulaEvaluation::Computed(Cell::Bool(expected)),
                "formula {formula:?}"
            );
        }
    }

    #[test]
    fn comparison_text_vs_number_all_six_operators() {
        // Mirrored operand order: text always outranks number.
        let cases: [(&str, bool); 6] = [
            ("\"5\"=10", false),
            ("\"5\"<>10", true),
            ("\"5\"<10", false),
            ("\"5\"<=10", false),
            ("\"5\">10", true),
            ("\"5\">=10", true),
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval(formula),
                FormulaEvaluation::Computed(Cell::Bool(expected)),
                "formula {formula:?}"
            );
        }
    }

    #[test]
    fn comparison_number_vs_bool_uses_type_rank_not_numeric_value() {
        // Logical outranks Number, so 1=TRUE is FALSE even though both would
        // numerically coerce to 1.
        assert_eq!(
            eval("1=TRUE"),
            FormulaEvaluation::Computed(Cell::Bool(false))
        );
        assert_eq!(
            eval("1<TRUE"),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            eval("TRUE>1"),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            eval("0=FALSE"),
            FormulaEvaluation::Computed(Cell::Bool(false)),
            "a number is never equal to a logical, even 0 vs FALSE"
        );
    }

    #[test]
    fn comparison_text_vs_bool_uses_type_rank() {
        // Text < Logical, confirmed via the existing "abc"<TRUE test; extend
        // to the equality/greater-than directions too.
        assert_eq!(
            eval("\"TRUE\"=TRUE"),
            FormulaEvaluation::Computed(Cell::Bool(false)),
            "text is never equal to a logical, even matching text"
        );
        assert_eq!(
            eval("TRUE>\"z\""),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
    }

    #[test]
    fn comparison_text_is_case_insensitive() {
        assert_eq!(
            eval("\"abc\"=\"ABC\""),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            eval("\"abc\"<\"ABD\""),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
    }

    #[test]
    fn comparison_blank_behaves_like_zero() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        // A1 stays blank; formulas live in other cells to avoid
        // self-reference.
        sheet.write_formula(1, 0, "A1=0", false);
        sheet.write_formula(1, 1, "A1<1", false);
        assert_eq!(
            wb.evaluate_cell("Data", 1, 0),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
        assert_eq!(
            wb.evaluate_cell("Data", 1, 1),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
    }

    // -- WS2: reference parser table ---------------------------------------

    fn eval_ref_against(row0: (f64, f64), row1: (f64, f64), formula: &str) -> FormulaEvaluation {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, row0.0); // A1
        sheet.write(0, 1, row0.1); // B1
        sheet.write(1, 0, row1.0); // A2
        sheet.write(1, 1, row1.1); // B2
                                   // Put the formula far away so it never collides with a referenced
                                   // cell/range/row/col above.
        sheet.write_formula(25, 25, formula, 0.0);
        wb.evaluate_cell("Data", 25, 25)
    }

    #[test]
    fn reference_forms_bare_a1_dollar_variants() {
        // Only a bare, undecorated "A1" is recognized as a top-level
        // reference by the identifier scanner (it doesn't include '$' in
        // its character class). A leading '$' isn't a valid primary-token
        // start at all, and a mid-token '$' truncates the identifier before
        // the digits, so "A$1" is read as bare identifier "A" instead.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "A1"),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "$A$1"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnparsableExpression,
            }
        );
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "$A1"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnparsableExpression,
            }
        );
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "A$1"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnresolvedName,
            }
        );
    }

    #[test]
    fn reference_forms_sheet_qualified_dollar_variants_all_resolve() {
        // Sheet-qualified references always go through the dollar-aware
        // `parse_a1_reference` scanner, regardless of where the '$' signs
        // sit, so all four combinations resolve identically.
        for formula in ["Data!A1", "Data!$A$1", "Data!A$1", "Data!$A1"] {
            assert_eq!(
                eval_ref_against((1.0, 2.0), (3.0, 4.0), formula),
                FormulaEvaluation::Computed(Cell::Number(1.0)),
                "formula {formula:?}"
            );
        }
    }

    #[test]
    fn reference_range_dollar_variants() {
        // A bare (non-sheet-qualified) range's END operand goes through the
        // dollar-aware scanner even though the START does not (it's read as
        // a plain identifier first), so a '$' on the end alone still works.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "SUM(A1:$B$2)"),
            FormulaEvaluation::Computed(Cell::Number(10.0))
        );
        // Sheet-qualified ranges are dollar-aware on both operands.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "SUM(Data!$A$1:$B$2)"),
            FormulaEvaluation::Computed(Cell::Number(10.0))
        );
        // A '$' on the bare START of a range is not recognized at all.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "SUM($A$1:$B$2)"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnparsableExpression,
            }
        );
    }

    #[test]
    fn reference_whole_row_and_whole_column_are_rejected() {
        // Whole-row/whole-column references are not part of this MVP
        // grammar (tracked separately on the roadmap); they must fail
        // gracefully, never miscompute or panic.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "3:5"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnparsableExpression,
            }
        );
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "B:D"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnparsableExpression,
            }
        );
    }

    #[test]
    fn reference_max_coordinate_xfd1048576_resolves() {
        // XFD1048576 is Excel's actual maximum cell (column 16384, row
        // 1,048,576); it must parse and resolve (to blank, since nothing is
        // written there), not be rejected as out of range.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "XFD1048576"),
            FormulaEvaluation::Computed(Cell::Text(String::new()))
        );
    }

    #[test]
    fn reference_one_past_max_coordinates_is_rejected() {
        // One column past XFD (XFE) and one row past 1,048,576 must both be
        // rejected, not silently wrapped or truncated.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "XFE1"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnresolvedName,
            }
        );
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "A1048577"),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::UnresolvedName,
            }
        );
    }

    #[test]
    fn reference_garbage_forms_are_rejected_not_miscomputed() {
        let garbage = [
            "AAAA1",        // 4-letter column prefix exceeds the 3-letter max
            "R1C1",         // R1C1-style, not A1-style
            "A0",           // row 0 does not exist (1-based)
            "A1B2",         // not a valid column/row split
            "A",            // letters with no digits
            "A-1",          // negative row
            "A99999999999", // row overflows Excel's row limit
        ];
        for formula in garbage {
            assert_eq!(
                eval_ref_against((1.0, 2.0), (3.0, 4.0), formula),
                FormulaEvaluation::Fallback {
                    cached: Cell::Number(0.0),
                    reason: FormulaUnsupportedReason::UnresolvedName,
                },
                "formula {formula:?}"
            );
        }
        // A lone number is valid syntax (a numeric literal), just not a
        // reference -- it computes rather than erroring.
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "1"),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
    }

    #[test]
    fn reference_case_insensitive_column_letters() {
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), "a1"),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
    }

    #[test]
    fn reference_tolerates_surrounding_whitespace() {
        assert_eq!(
            eval_ref_against((1.0, 2.0), (3.0, 4.0), " A1 "),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
    }

    #[test]
    fn reference_quoted_sheet_range_with_escaped_apostrophe() {
        let mut wb = Workbook::new();
        {
            let sheet = wb.add_sheet("Bob's Sheet");
            sheet.write(0, 0, 5.0);
            sheet.write(0, 1, 6.0);
        }
        wb.add_sheet("Data")
            .write_formula(0, 0, "SUM('Bob''s Sheet'!A1:B1)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Computed(Cell::Number(11.0))
        );
    }

    // -- WS2: cycle detection -----------------------------------------------

    #[test]
    fn detects_direct_self_reference_cycle() {
        let mut wb = Workbook::new();
        wb.add_sheet("Data").write_formula(0, 0, "A1", 5.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(5.0),
                reason: FormulaUnsupportedReason::CircularReference,
            }
        );
    }

    #[test]
    fn detects_three_cell_cycle() {
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write_formula(0, 0, "A2", 1.0); // A1 = A2
        sheet.write_formula(1, 0, "A3", 2.0); // A2 = A3
        sheet.write_formula(2, 0, "A1", 3.0); // A3 = A1

        assert_eq!(
            wb.evaluate_cell("Data", 0, 0),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(1.0),
                reason: FormulaUnsupportedReason::CircularReference,
            }
        );
    }

    #[test]
    fn diamond_dependency_is_not_a_false_positive_cycle() {
        // A1 feeds both B1 and C1, which both feed D1. The shared ancestor
        // must not trip the in-progress cycle guard: memoization caches A1's
        // result the first time it's visited, so the second visit (via the
        // other branch) is a cache hit, not a re-entrant "visiting" check.
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 10.0); // A1
        sheet.write_formula(0, 1, "A1*2", 0.0); // B1 = A1*2
        sheet.write_formula(0, 2, "A1*3", 0.0); // C1 = A1*3
        sheet.write_formula(0, 3, "B1+C1", 0.0); // D1 = B1+C1

        assert_eq!(
            wb.evaluate_cell("Data", 0, 3),
            FormulaEvaluation::Computed(Cell::Number(50.0))
        );
    }

    // -- WS2: depth budget boundary ------------------------------------------

    #[test]
    fn expression_at_exact_depth_limit_computes() {
        // MAX_EXPR_DEPTH is 128. A chained-unary-minus expression with N
        // signs costs N+1 parse_unary calls plus 1 final parse_primary call
        // for the trailing digit, i.e. depth N+2. N=126 lands exactly on
        // the 128 budget and must still compute (126 is even, so the sign
        // cancels out to +1).
        assert_eq!(
            MAX_EXPR_DEPTH, 128,
            "boundary math below assumes this constant"
        );
        let formula = format!("{}1", "-".repeat(126));
        assert_eq!(
            eval(&formula),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
    }

    #[test]
    fn expression_one_past_depth_limit_falls_back() {
        let formula = format!("{}1", "-".repeat(127));
        assert_eq!(
            eval(&formula),
            FormulaEvaluation::Fallback {
                cached: Cell::Number(0.0),
                reason: FormulaUnsupportedReason::ExpressionTooComplex,
            }
        );
    }

    // -- WS2: volatile completeness ------------------------------------------

    #[test]
    fn all_eight_volatile_functions_fall_back_with_volatile_reason() {
        // The volatile check happens on the function name alone, before any
        // argument parsing, so empty-arg calls are enough to exercise it.
        for func in [
            "NOW",
            "TODAY",
            "RAND",
            "RANDBETWEEN",
            "INDIRECT",
            "OFFSET",
            "CELL",
            "ADDRESS",
        ] {
            assert_eq!(
                eval(&format!("{func}()")),
                FormulaEvaluation::Fallback {
                    cached: Cell::Number(0.0),
                    reason: FormulaUnsupportedReason::Volatile,
                },
                "function {func}"
            );
        }
    }

    // -- WS2: recently-fixed semantics, extended coverage --------------------

    #[test]
    fn mod_sign_table_covers_all_four_sign_combinations() {
        // MOD(n,d) == n - d*FLOOR(n/d): result always takes the divisor's
        // sign, matching Excel (not Rust's dividend-sign '%').
        let cases: [(&str, f64); 4] = [
            ("MOD(7,3)", 1.0),    // (+,+)
            ("MOD(-7,3)", 2.0),   // (-,+)
            ("MOD(7,-3)", -2.0),  // (+,-)
            ("MOD(-7,-3)", -1.0), // (-,-)
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval(formula),
                FormulaEvaluation::Computed(Cell::Number(expected)),
                "formula {formula:?}"
            );
        }
    }

    #[test]
    fn round_returns_num_error_for_a_precision_so_large_the_factor_overflows() {
        // A large *positive* precision overflows `10^precision` to +inf
        // (not just a very negative one underflowing to 0.0); the
        // finite-result check must catch this direction too.
        assert_eq!(
            eval("ROUND(5,400)"),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
    }

    #[test]
    fn left_right_clamp_boundary_table() {
        let cases: [(&str, &str); 8] = [
            ("LEFT(\"abc\",0)", ""),
            ("LEFT(\"abc\",3)", "abc"),
            ("LEFT(\"abc\",4)", "abc"),
            ("LEFT(\"abc\",1000000)", "abc"),
            ("RIGHT(\"abc\",0)", ""),
            ("RIGHT(\"abc\",3)", "abc"),
            ("RIGHT(\"abc\",4)", "abc"),
            ("RIGHT(\"abc\",1000000)", "abc"),
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval(formula),
                FormulaEvaluation::Computed(Cell::Text(expected.into())),
                "formula {formula:?}"
            );
        }
    }

    #[test]
    fn mid_clamp_boundary_table() {
        let cases: [(&str, &str); 5] = [
            ("MID(\"abcd\",1,0)", ""),           // len=0
            ("MID(\"abcd\",1,4)", "abcd"),       // len=exact remaining
            ("MID(\"abcd\",1,5)", "abcd"),       // len=remaining+1, clamped
            ("MID(\"abcd\",1,1000000)", "abcd"), // huge len, clamped
            ("MID(\"abcd\",5,1)", ""),           // start beyond text length
        ];
        for (formula, expected) in cases {
            assert_eq!(
                eval(formula),
                FormulaEvaluation::Computed(Cell::Text(expected.into())),
                "formula {formula:?}"
            );
        }
    }

    #[test]
    fn mid_start_zero_is_value_error() {
        assert_eq!(
            eval("MID(\"abcd\",0,1)"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn error_literal_table_all_seven_canonical_codes_parse() {
        // Every canonical error string ISERROR can be asked about must
        // round-trip through the '#...' literal parser, including the two
        // (#N/A, #DIV/0!) that contain a '/' the generic scanner would
        // otherwise cut short.
        for literal in [
            "#NULL!", "#DIV/0!", "#VALUE!", "#REF!", "#NAME?", "#NUM!", "#N/A",
        ] {
            assert_eq!(
                eval(&format!("ISERROR({literal})")),
                FormulaEvaluation::Computed(Cell::Bool(true)),
                "literal {literal}"
            );
        }
    }

    // -- WS2: string/function edge cases -------------------------------------

    #[test]
    fn nested_double_quotes_inside_a_string_literal_unescape_to_one_quote() {
        // Excel escapes a literal '"' inside a string literal as '""'; the
        // 16-character formula text below (`"he said ""hi"""`) unescapes to
        // `he said "hi"`.
        assert_eq!(
            eval(r#""he said ""hi""""#),
            FormulaEvaluation::Computed(Cell::Text("he said \"hi\"".into()))
        );
    }

    #[test]
    fn empty_string_arguments_are_well_defined_across_text_functions() {
        assert_eq!(
            eval("LEN(\"\")"),
            FormulaEvaluation::Computed(Cell::Number(0.0))
        );
        assert_eq!(
            eval("TRIM(\"\")"),
            FormulaEvaluation::Computed(Cell::Text(String::new()))
        );
        assert_eq!(
            eval("UPPER(\"\")"),
            FormulaEvaluation::Computed(Cell::Text(String::new()))
        );
        assert_eq!(
            eval("CONCATENATE(\"\",\"\")"),
            FormulaEvaluation::Computed(Cell::Text(String::new()))
        );
    }

    #[test]
    fn trim_upper_lower_on_unicode_text() {
        // TRIM only strips ASCII whitespace; UPPER/LOWER on a caseless
        // script (Korean) is a documented no-op, not an error.
        assert_eq!(
            eval("UPPER(\"한글\")"),
            FormulaEvaluation::Computed(Cell::Text("한글".into()))
        );
        assert_eq!(
            eval("LOWER(\"한글\")"),
            FormulaEvaluation::Computed(Cell::Text("한글".into()))
        );
        assert_eq!(
            eval("UPPER(\"café\")"),
            FormulaEvaluation::Computed(Cell::Text("CAFÉ".into()))
        );
    }

    #[test]
    fn len_counts_unicode_scalar_values_not_bytes() {
        // An emoji outside the BMP is still exactly one Rust `char` (one
        // Unicode scalar value), so LEN must report 1, not its 4-byte UTF-8
        // encoded length.
        assert_eq!(
            eval("LEN(\"😀\")"),
            FormulaEvaluation::Computed(Cell::Number(1.0))
        );
    }

    #[test]
    fn concatenate_function_and_ampersand_operator_are_equivalent() {
        assert_eq!(
            eval("CONCATENATE(\"a\",\"b\")=(\"a\"&\"b\")"),
            FormulaEvaluation::Computed(Cell::Bool(true))
        );
    }

    #[test]
    fn boolean_returning_functions_feed_arithmetic_via_coercion() {
        // A comparison's Bool result coerces to 1/0 when used arithmetically.
        assert_eq!(
            eval("(1=1)+1"),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
        assert_eq!(
            eval("AND(TRUE,TRUE)*10"),
            FormulaEvaluation::Computed(Cell::Number(10.0))
        );
        assert_eq!(
            eval("NOT(TRUE)+5"),
            FormulaEvaluation::Computed(Cell::Number(5.0))
        );
    }

    // -- WS2: empty-argument and zero-numeric-input edge cases --------------

    #[test]
    fn sum_and_product_of_no_numeric_input_use_their_identity_value() {
        assert_eq!(
            eval("SUM()"),
            FormulaEvaluation::Computed(Cell::Number(0.0))
        );
        assert_eq!(
            eval("PRODUCT()"),
            FormulaEvaluation::Computed(Cell::Number(0.0)),
            "PRODUCT with no numeric input is documented to fall back to 0, not 1"
        );
    }

    #[test]
    fn min_and_max_of_no_numeric_input_is_num_error() {
        assert_eq!(
            eval("MIN()"),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
        assert_eq!(
            eval("MAX()"),
            FormulaEvaluation::Computed(Cell::Error("#NUM!".into()))
        );
    }

    #[test]
    fn average_of_no_numeric_input_is_div0_error() {
        assert_eq!(
            eval("AVERAGE()"),
            FormulaEvaluation::Computed(Cell::Error("#DIV/0!".into()))
        );
    }

    #[test]
    fn count_and_counta_of_no_input_is_zero() {
        assert_eq!(
            eval("COUNT()"),
            FormulaEvaluation::Computed(Cell::Number(0.0))
        );
        assert_eq!(
            eval("COUNTA()"),
            FormulaEvaluation::Computed(Cell::Number(0.0))
        );
    }

    #[test]
    fn and_or_not_reject_zero_arguments() {
        assert_eq!(
            eval("AND()"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
        assert_eq!(
            eval("OR()"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
        assert_eq!(
            eval("NOT()"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn is_functions_reject_zero_arguments_with_value_error() {
        for func in ["ISNA", "ISERROR", "ISNUMBER", "ISTEXT", "ISBLANK"] {
            assert_eq!(
                eval(&format!("{func}()")),
                FormulaEvaluation::Computed(Cell::Error("#VALUE!".into())),
                "function {func}"
            );
        }
    }

    #[test]
    fn exact_and_value_reject_wrong_argument_counts() {
        assert_eq!(
            eval("EXACT(\"a\")"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
        assert_eq!(
            eval("EXACT(\"a\",\"b\",\"c\")"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
        assert_eq!(
            eval("VALUE()"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
        assert_eq!(
            eval("VALUE(\"1\",\"2\")"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn if_rejects_too_few_or_too_many_arguments() {
        assert_eq!(
            eval("IF(TRUE)"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
        assert_eq!(
            eval("IF(TRUE,1,2,3)"),
            FormulaEvaluation::Computed(Cell::Error("#VALUE!".into()))
        );
    }

    #[test]
    fn range_reference_accepts_the_exact_cell_budget_boundary() {
        // MAX_RANGE_CELLS is 10,000; a range of exactly that many cells must
        // still be traversed (only one past it is rejected -- already
        // covered by the existing 12,001-cell regression tests).
        let mut wb = Workbook::new();
        let sheet = wb.add_sheet("Data");
        sheet.write(0, 0, 1.0);
        sheet.write(9_999, 0, 1.0); // A10000, the last row in a 10,000-row range
        sheet.write_formula(0, 1, "COUNT(A1:A10000)", 0.0);

        assert_eq!(
            wb.evaluate_cell("Data", 0, 1),
            FormulaEvaluation::Computed(Cell::Number(2.0))
        );
    }
}
