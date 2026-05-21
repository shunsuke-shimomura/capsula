//! Query builder for constructing SQL queries with `JSONPath` filters
//!
//! This module provides a builder for constructing dynamic SQL queries
//! that filter runs based on metadata and hook outputs using `JSONPath` expressions.

use crate::models::{ComparisonOp, HookFilter, ParameterMatch, SearchRunsRequest, SortOrder};
use chrono::{DateTime, Utc};
use sql_json_path::JsonPath;
use std::fmt::Write;

/// Maximum length for `JSONPath` expressions (prevents denial-of-service)
const MAX_JSONPATH_LENGTH: usize = 500;

/// Maximum LIMIT value: callers asking for more than this are clamped.
const MAX_LIMIT: i64 = 1_000;

/// Maximum OFFSET value: callers asking for more than this are clamped.
/// Prevents slow-query `DoS` from a large `OFFSET` forcing a sequential scan.
const MAX_OFFSET: i64 = 100_000;

/// Error types for query building
#[derive(Debug, thiserror::Error)]
pub enum QueryError {
    #[error("Invalid JSONPath expression: {0}")]
    InvalidJsonPath(String),
}

/// Builder for constructing run search queries
#[derive(Debug, Default)]
pub struct RunQueryBuilder {
    /// WHERE conditions for the runs table
    conditions: Vec<String>,
    /// EXISTS subqueries for hook filters
    hook_exists_clauses: Vec<String>,
    /// Bind parameters (as strings for now, will be used with sqlx)
    bind_values: Vec<BindValue>,
    /// Current parameter index
    param_index: usize,
    /// Sort order
    order: SortOrder,
    /// Limit
    limit: i64,
    /// Offset
    offset: i64,
}

/// A bind value for SQL queries
#[derive(Debug, Clone)]
#[expect(dead_code, reason = "Some variants are reserved for future use")]
pub enum BindValue {
    String(String),
    I32(i32),
    I64(i64),
    DateTime(DateTime<Utc>),
    Bool(bool),
}

impl RunQueryBuilder {
    /// Create a new query builder
    pub const fn new() -> Self {
        Self {
            conditions: Vec::new(),
            hook_exists_clauses: Vec::new(),
            bind_values: Vec::new(),
            param_index: 1,
            order: SortOrder::LatestFirst,
            limit: 100,
            offset: 0,
        }
    }

    /// Build from a search request
    pub fn from_request(request: &SearchRunsRequest) -> Result<Self, QueryError> {
        let mut builder = Self::new();

        if let Some(vault) = &request.vault {
            builder = builder.with_vault(vault);
        }

        if let Some(from) = request.from {
            builder = builder.with_from_timestamp(from);
        }

        if let Some(to) = request.to {
            builder = builder.with_to_timestamp(to);
        }

        if let Some(exit_code) = request.exit_code {
            builder = builder.with_exit_code(exit_code);
        }

        if let Some(success) = request.success {
            builder = builder.with_success(success);
        }

        for hook_filter in &request.hook_filters {
            builder = builder.with_hook_filter(hook_filter)?;
        }

        for param_match in &request.parameter_matches {
            builder = builder.with_parameter_match(param_match)?;
        }

        builder.order = request.order.clone();
        builder.limit = request.limit.unwrap_or(100);
        builder.offset = request.offset.unwrap_or(0);

        Ok(builder)
    }

    /// Add a vault filter
    pub fn with_vault(mut self, vault: &str) -> Self {
        self.conditions
            .push(format!("r.vault = ${}", self.param_index));
        self.bind_values.push(BindValue::String(vault.to_string()));
        self.param_index += 1;
        self
    }

    /// Add a from timestamp filter
    pub fn with_from_timestamp(mut self, from: DateTime<Utc>) -> Self {
        self.conditions
            .push(format!("r.timestamp >= ${}", self.param_index));
        self.bind_values.push(BindValue::DateTime(from));
        self.param_index += 1;
        self
    }

    /// Add a to timestamp filter
    pub fn with_to_timestamp(mut self, to: DateTime<Utc>) -> Self {
        self.conditions
            .push(format!("r.timestamp <= ${}", self.param_index));
        self.bind_values.push(BindValue::DateTime(to));
        self.param_index += 1;
        self
    }

    /// Add an exit code filter
    pub fn with_exit_code(mut self, exit_code: i32) -> Self {
        self.conditions
            .push(format!("r.exit_code = ${}", self.param_index));
        self.bind_values.push(BindValue::I32(exit_code));
        self.param_index += 1;
        self
    }

    /// Add a success filter (`exit_code` = 0 or `exit_code` != 0)
    pub fn with_success(mut self, success: bool) -> Self {
        if success {
            self.conditions.push("r.exit_code = 0".to_string());
        } else {
            self.conditions
                .push("r.exit_code IS NOT NULL AND r.exit_code != 0".to_string());
        }
        self
    }

    /// Add a hook filter using `JSONPath`
    pub fn with_hook_filter(mut self, filter: &HookFilter) -> Result<Self, QueryError> {
        // Validate JSONPath expressions (basic validation)
        Self::validate_jsonpath(&filter.output_filter)?;
        if let Some(config_filter) = &filter.config_filter {
            Self::validate_jsonpath(config_filter)?;
        }

        // Build the EXISTS subquery
        let mut subquery_conditions = vec![
            "ro.run_id = r.id".to_string(),
            format!("ro.hook_id = ${}", self.param_index),
        ];
        self.bind_values
            .push(BindValue::String(filter.hook_id.clone()));
        self.param_index += 1;

        // Add config filter if present
        if let Some(config_filter) = &filter.config_filter {
            subquery_conditions.push(format!(
                "jsonb_path_exists(ro.config, ${}::jsonpath)",
                self.param_index
            ));
            self.bind_values
                .push(BindValue::String(config_filter.clone()));
            self.param_index += 1;
        }

        // Add output filter
        subquery_conditions.push(format!(
            "jsonb_path_exists(ro.output, ${}::jsonpath)",
            self.param_index
        ));
        self.bind_values
            .push(BindValue::String(filter.output_filter.clone()));
        self.param_index += 1;

        let exists_clause = format!(
            "EXISTS (SELECT 1 FROM run_outputs ro WHERE {})",
            subquery_conditions.join(" AND ")
        );
        self.hook_exists_clauses.push(exists_clause);

        Ok(self)
    }

    /// Add a structured parameter match filter
    ///
    /// Generates a `JSONPath` expression from the structured match condition
    /// and adds it as an EXISTS subquery on `run_outputs`.
    ///
    /// For example, `ParameterMatch { parameter: "solar_flux", operator: Ge, value: 1347.39 }`
    /// generates: `jsonb_path_exists(ro.output, '$.solar_flux ? (@ >= 1347.39)'::jsonpath)`
    pub fn with_parameter_match(
        mut self,
        param_match: &ParameterMatch,
    ) -> Result<Self, QueryError> {
        // Validate phase
        let phase = match param_match.phase.as_str() {
            "pre" | "post" => &param_match.phase,
            _ => {
                return Err(QueryError::InvalidJsonPath(
                    "phase must be 'pre' or 'post'".to_string(),
                ))
            }
        };

        // Build JSONPath expression from the structured match
        let jsonpath_expr = build_jsonpath_from_match(
            &param_match.parameter,
            &param_match.operator,
            &param_match.value,
        )?;

        // Validate the generated JSONPath
        Self::validate_jsonpath(&jsonpath_expr)?;

        // Build EXISTS subquery
        let mut subquery_conditions = vec![
            "ro.run_id = r.id".to_string(),
            format!("ro.hook_id = ${}", self.param_index),
        ];
        self.bind_values
            .push(BindValue::String(param_match.hook_id.clone()));
        self.param_index += 1;

        // Add phase filter
        subquery_conditions.push(format!("ro.phase = ${}", self.param_index));
        self.bind_values
            .push(BindValue::String(phase.clone()));
        self.param_index += 1;

        // Add JSONPath filter on output
        subquery_conditions.push(format!(
            "jsonb_path_exists(ro.output, ${}::jsonpath)",
            self.param_index
        ));
        self.bind_values.push(BindValue::String(jsonpath_expr));
        self.param_index += 1;

        let exists_clause = format!(
            "EXISTS (SELECT 1 FROM run_outputs ro WHERE {})",
            subquery_conditions.join(" AND ")
        );
        self.hook_exists_clauses.push(exists_clause);

        Ok(self)
    }

    /// Validate a `JSONPath` expression using SQL/JSON path parser
    ///
    /// Uses `sql-json-path` crate which is compatible with `PostgreSQL`'s SQL/JSON
    /// path language, including `starts with`, `like_regex`, `.type()`, `.size()`,
    /// arithmetic operators, etc.
    fn validate_jsonpath(expr: &str) -> Result<(), QueryError> {
        // Length limit for DoS prevention
        if expr.len() > MAX_JSONPATH_LENGTH {
            return Err(QueryError::InvalidJsonPath(
                "JSONPath expression too long (max 500 characters)".to_string(),
            ));
        }

        // Parse using SQL/JSON path parser (PostgreSQL compatible)
        JsonPath::new(expr)
            .map_err(|e| QueryError::InvalidJsonPath(format!("Invalid JSONPath syntax: {e}")))?;

        Ok(())
    }

    /// Build the main SELECT query
    pub fn build_query(&self) -> String {
        let mut query = String::from(
            "SELECT DISTINCT r.id, r.name, r.timestamp, r.command, r.vault, r.project_root, \
             r.exit_code, r.duration_ms, r.stdout, r.stderr, r.created_at, r.updated_at \
             FROM runs r",
        );

        // Add WHERE clause
        let all_conditions: Vec<&str> = self
            .conditions
            .iter()
            .map(String::as_str)
            .chain(self.hook_exists_clauses.iter().map(String::as_str))
            .collect();

        if !all_conditions.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&all_conditions.join(" AND "));
        }

        // Add ORDER BY
        match self.order {
            SortOrder::LatestFirst => query.push_str(" ORDER BY r.timestamp DESC"),
            SortOrder::OldestFirst => query.push_str(" ORDER BY r.timestamp ASC"),
        }

        // Add LIMIT and OFFSET (clamped to avoid DoS from large scans)
        let _ = write!(
            query,
            " LIMIT {} OFFSET {}",
            self.limit.clamp(0, MAX_LIMIT),
            self.offset.clamp(0, MAX_OFFSET),
        );

        query
    }

    /// Build the COUNT query for total results
    pub fn build_count_query(&self) -> String {
        let mut query = String::from("SELECT COUNT(DISTINCT r.id) FROM runs r");

        let all_conditions: Vec<&str> = self
            .conditions
            .iter()
            .map(String::as_str)
            .chain(self.hook_exists_clauses.iter().map(String::as_str))
            .collect();

        if !all_conditions.is_empty() {
            query.push_str(" WHERE ");
            query.push_str(&all_conditions.join(" AND "));
        }

        query
    }

    /// Get bind values for the query
    pub fn bind_values(&self) -> &[BindValue] {
        &self.bind_values
    }
}

/// Build a PostgreSQL-compatible `JSONPath` expression from a structured match condition.
///
/// Converts a parameter path, operator, and value into a `JSONPath` expression like:
/// `$.solar_flux ? (@ >= 1347.39)`
///
/// For nested parameters (e.g., "params.temperature"), generates:
/// `$.params.temperature ? (@ >= 300.0)`
fn build_jsonpath_from_match(
    parameter: &str,
    operator: &ComparisonOp,
    value: &serde_json::Value,
) -> Result<String, QueryError> {
    // Validate parameter name (prevent injection)
    if parameter.is_empty() || parameter.len() > 200 {
        return Err(QueryError::InvalidJsonPath(
            "parameter name must be 1-200 characters".to_string(),
        ));
    }
    for ch in parameter.chars() {
        if !ch.is_alphanumeric() && ch != '_' && ch != '.' {
            return Err(QueryError::InvalidJsonPath(format!(
                "parameter name contains invalid character: '{ch}'"
            )));
        }
    }

    let op_str = match operator {
        ComparisonOp::Eq => "==",
        ComparisonOp::Ne => "!=",
        ComparisonOp::Gt => ">",
        ComparisonOp::Ge => ">=",
        ComparisonOp::Lt => "<",
        ComparisonOp::Le => "<=",
    };

    let value_str = match value {
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => {
            return Err(QueryError::InvalidJsonPath(
                "parameter match value must be a number, string, or boolean".to_string(),
            ))
        }
    };

    Ok(format!("$.{parameter} ? (@ {op_str} {value_str})"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_query() {
        let builder = RunQueryBuilder::new().with_vault("my-project");
        let query = builder.build_query();
        assert!(query.contains("r.vault = $1"));
        assert!(query.contains("ORDER BY r.timestamp DESC"));
    }

    #[test]
    fn test_hook_filter() {
        let filter = HookFilter {
            hook_id: "capture-git-repo".to_string(),
            config_filter: None,
            output_filter: "$.sha ? (@ starts with \"abc\")".to_string(),
        };
        let builder = RunQueryBuilder::new()
            .with_hook_filter(&filter)
            .expect("valid JSONPath filter should succeed");
        let query = builder.build_query();
        assert!(query.contains("EXISTS"));
        assert!(query.contains("jsonb_path_exists"));
    }

    #[test]
    fn test_invalid_jsonpath() {
        let filter = HookFilter {
            hook_id: "test".to_string(),
            config_filter: None,
            output_filter: "not-starting-with-dollar".to_string(),
        };
        let result = RunQueryBuilder::new().with_hook_filter(&filter);
        assert!(result.is_err());
    }

    #[test]
    fn test_parameter_match_numeric() {
        let pm = ParameterMatch {
            hook_id: "capture-command".to_string(),
            phase: "pre".to_string(),
            parameter: "solar_flux".to_string(),
            operator: ComparisonOp::Ge,
            value: serde_json::json!(1347.39),
        };
        let builder = RunQueryBuilder::new()
            .with_parameter_match(&pm)
            .expect("valid parameter match");
        let query = builder.build_query();
        assert!(query.contains("EXISTS"));
        assert!(query.contains("ro.phase = $"));
        assert!(query.contains("jsonb_path_exists"));
    }

    #[test]
    fn test_parameter_match_string() {
        let pm = ParameterMatch {
            hook_id: "capture-env".to_string(),
            phase: "pre".to_string(),
            parameter: "orbit_type".to_string(),
            operator: ComparisonOp::Eq,
            value: serde_json::json!("LEO"),
        };
        let builder = RunQueryBuilder::new()
            .with_parameter_match(&pm)
            .expect("valid parameter match");
        let query = builder.build_query();
        assert!(query.contains("EXISTS"));
    }

    #[test]
    fn test_parameter_match_invalid_phase() {
        let pm = ParameterMatch {
            hook_id: "test".to_string(),
            phase: "invalid".to_string(),
            parameter: "x".to_string(),
            operator: ComparisonOp::Eq,
            value: serde_json::json!(1),
        };
        let result = RunQueryBuilder::new().with_parameter_match(&pm);
        assert!(result.is_err());
    }

    #[test]
    fn test_parameter_match_nested() {
        let pm = ParameterMatch {
            hook_id: "capture-command".to_string(),
            phase: "post".to_string(),
            parameter: "results.max_temperature".to_string(),
            operator: ComparisonOp::Le,
            value: serde_json::json!(85.0),
        };
        let builder = RunQueryBuilder::new()
            .with_parameter_match(&pm)
            .expect("valid parameter match");
        let query = builder.build_query();
        assert!(query.contains("EXISTS"));
    }

    #[test]
    fn test_build_jsonpath_exact_number() {
        let expr =
            build_jsonpath_from_match("solar_flux", &ComparisonOp::Eq, &serde_json::json!(1361.0))
                .unwrap();
        assert_eq!(expr, "$.solar_flux ? (@ == 1361.0)");
    }

    #[test]
    fn test_build_jsonpath_range() {
        let lower =
            build_jsonpath_from_match("temp", &ComparisonOp::Ge, &serde_json::json!(10.0))
                .unwrap();
        let upper =
            build_jsonpath_from_match("temp", &ComparisonOp::Le, &serde_json::json!(90.0))
                .unwrap();
        assert_eq!(lower, "$.temp ? (@ >= 10.0)");
        assert_eq!(upper, "$.temp ? (@ <= 90.0)");
    }

    #[test]
    fn test_build_jsonpath_string_value() {
        let expr =
            build_jsonpath_from_match("orbit", &ComparisonOp::Eq, &serde_json::json!("LEO"))
                .unwrap();
        assert_eq!(expr, r#"$.orbit ? (@ == "LEO")"#);
    }

    #[test]
    fn test_build_jsonpath_invalid_param_name() {
        let result = build_jsonpath_from_match(
            "solar flux; DROP TABLE",
            &ComparisonOp::Eq,
            &serde_json::json!(1),
        );
        assert!(result.is_err());
    }

    #[test]
    fn test_combined_filters() {
        let filter = HookFilter {
            hook_id: "capture-env".to_string(),
            config_filter: Some("$.name ? (@ == \"PARAM1\")".to_string()),
            output_filter: "$.value ? (@ == \"production\")".to_string(),
        };
        let builder = RunQueryBuilder::new()
            .with_vault("my-project")
            .with_success(true)
            .with_hook_filter(&filter)
            .expect("valid JSONPath filter should succeed");
        let query = builder.build_query();

        assert!(query.contains("r.vault = $1"));
        assert!(query.contains("r.exit_code = 0"));
        assert!(query.contains("EXISTS"));
        assert!(query.contains("ro.config"));
        assert!(query.contains("ro.output"));
    }
}
