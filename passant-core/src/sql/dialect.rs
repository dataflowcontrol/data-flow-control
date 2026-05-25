use std::str::FromStr;

use serde::Deserialize;

/// SQL dialect for parsing and (eventually) generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SqlDialect {
    #[default]
    DuckDb,
    Postgres,
    SQLite,
    ClickHouse,
    DataFusion,
    Umbra,
    GenericAnsi,
}

impl SqlDialect {
    pub fn parse(value: &str) -> Self {
        Self::from_str(value).unwrap_or(Self::DuckDb)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::DuckDb => "duckdb",
            Self::Postgres => "postgres",
            Self::SQLite => "sqlite",
            Self::ClickHouse => "clickhouse",
            Self::DataFusion => "datafusion",
            Self::Umbra => "umbra",
            Self::GenericAnsi => "generic_ansi",
        }
    }

    /// Quote a single SQL identifier for this dialect.
    pub fn quote_identifier(self, name: &str) -> String {
        let quote = '"';
        format!(
            "{quote}{}{quote}",
            name.replace(quote, &format!("{quote}{quote}"))
        )
    }
}

impl FromStr for SqlDialect {
    type Err = ();

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Ok(match value.trim().to_ascii_lowercase().as_str() {
            "duckdb" => Self::DuckDb,
            "postgres" | "postgresql" => Self::Postgres,
            "sqlite" => Self::SQLite,
            "clickhouse" => Self::ClickHouse,
            "datafusion" => Self::DataFusion,
            "umbra" => Self::Umbra,
            "generic" | "generic_ansi" | "ansi" => Self::GenericAnsi,
            _ => return Err(()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::SqlDialect;
    use crate::catalog::CatalogSnapshot;

    #[test]
    fn parses_dialect_aliases() {
        assert_eq!(
            "postgres".parse::<SqlDialect>().unwrap(),
            SqlDialect::Postgres
        );
        assert_eq!("duckdb".parse::<SqlDialect>().unwrap(), SqlDialect::DuckDb);
    }

    #[test]
    fn unknown_dialect_falls_back_to_duckdb_in_catalog() {
        let snapshot: CatalogSnapshot =
            serde_json::from_str(r#"{"tables":{},"dialect":"unknown_engine"}"#).unwrap();
        assert_eq!(snapshot.sql_dialect(), SqlDialect::DuckDb);
    }

    #[test]
    fn quote_identifier_doubles_embedded_quotes() {
        assert_eq!(SqlDialect::Postgres.quote_identifier(r#"a"b"#), r#""a""b""#);
        assert_eq!(SqlDialect::SQLite.quote_identifier("foo"), r#""foo""#);
    }
}
