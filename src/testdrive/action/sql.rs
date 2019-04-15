// Copyright 2019 Materialize, Inc. All rights reserved.
//
// This file is part of Materialize. Materialize may not be used or
// distributed without the express permission of Materialize, Inc.

use postgres::error::DbError;
use sqlparser::dialect::AnsiSqlDialect;
use sqlparser::sqlast::SQLStatement;
use sqlparser::sqlparser::Parser as SQLParser;
use std::thread;
use std::time::Duration;

use crate::action::{Action, State};
use crate::parser::SqlCommand;
use ore::vec::VecExt;

pub struct SqlAction {
    cmd: SqlCommand,
    stmt: SQLStatement,
}

pub fn build_sql(mut cmd: SqlCommand) -> Result<SqlAction, String> {
    let stmts = SQLParser::parse_sql(&AnsiSqlDialect {}, cmd.query.clone())
        .map_err(|e| format!("unable to parse SQL: {}: {}", cmd.query, e))?;
    if stmts.len() != 1 {
        return Err(format!("expected one statement, but got {}", stmts.len()));
    }
    // TODO(benesch): one day we'll support SQL queries where order matters.
    cmd.expected_rows.sort();
    Ok(SqlAction {
        cmd,
        stmt: stmts.into_element(),
    })
}

impl Action for SqlAction {
    fn undo(&self, state: &mut State) -> Result<(), String> {
        match &self.stmt {
            SQLStatement::SQLCreateDataSource { name, .. } => self.try_drop(
                &mut state.pgconn,
                &format!("DROP DATA SOURCE {}", name.to_string()),
            ),
            SQLStatement::SQLCreateView { name, .. } => self.try_drop(
                &mut state.pgconn,
                &format!("DROP VIEW {}", name.to_string()),
            ),
            _ => Ok(()),
        }
    }

    fn redo(&self, state: &mut State) -> Result<(), String> {
        print_query(&self.cmd.query);
        let max = match self.stmt {
            SQLStatement::SQLPeek { .. } => 5,
            _ => 0,
        };
        let mut i = 0;
        loop {
            let backoff = Duration::from_millis(100 * 2_u64.pow(i));
            match self.try_redo(&mut state.pgconn) {
                Ok(()) => return Ok(()),
                Err(err) => {
                    if i >= max {
                        return Err(err);
                    } else {
                        println!(
                            "rows didn't match; sleeping {:?} to see if dataflow catches up",
                            backoff
                        );
                    }
                }
            }
            thread::sleep(backoff);
            i += 1;
        }
    }
}

impl SqlAction {
    fn try_drop(&self, pgconn: &mut postgres::Client, query: &str) -> Result<(), String> {
        print_query(&query);
        match pgconn.simple_query(query) {
            Err(err) => {
                let err_string = err.to_string();
                if let Some(err) = err.into_source() {
                    if let Ok(err) = err.downcast::<DbError>() {
                        if err.message() == "target node does not exist" {
                            return Ok(());
                        }
                    }
                }
                Err(err_string)
            }
            Ok(_) => Ok(()),
        }
    }

    fn try_redo(&self, pgconn: &mut postgres::Client) -> Result<(), String> {
        let mut rows = Vec::new();
        let msgs = pgconn
            .simple_query(&self.cmd.query)
            .map_err(|e| format!("query failed: {}", e))?;
        for msg in &msgs {
            match msg {
                postgres::SimpleQueryMessage::CommandComplete(_) => (),
                postgres::SimpleQueryMessage::Row(row) => {
                    let mut row0 = Vec::new();
                    for i in 0..row.len() {
                        row0.push(row.get(i).unwrap().to_owned());
                    }
                    rows.push(row0);
                }
                _ => unimplemented!(),
            }
        }
        rows.sort();
        if rows == self.cmd.expected_rows {
            Ok(())
        } else {
            // TODO(benesch): a better diff here would be nice.
            Err(format!(
                "non-matching rows: expected:\n{:?}\ngot:\n{:?}",
                self.cmd.expected_rows, rows
            ))
        }
    }
}

fn print_query(query: &str) {
    if query.len() > 72 {
        println!("> {}...", &query[..72]);
    } else {
        println!("> {}", &query);
    }
}
