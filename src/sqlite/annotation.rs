#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnsAnnotationError {
    InvalidTypeName { name: String, reason: String },
}

impl std::fmt::Display for ReturnsAnnotationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidTypeName { name, reason } => {
                write!(f, "`-- returns: {name}` is invalid: {reason}")
            }
        }
    }
}

pub fn parse_returns_annotation(
    sql: &str,
) -> std::result::Result<Option<String>, ReturnsAnnotationError> {
    for line in sql.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !trimmed.starts_with("--") {
            return Ok(None);
        }

        let body = trimmed.trim_start_matches("--").trim();
        if let Some(name) = body.strip_prefix("returns:") {
            return validate_returns_type_name(name.trim()).map(Some);
        }
    }

    Ok(None)
}

fn validate_returns_type_name(name: &str) -> std::result::Result<String, ReturnsAnnotationError> {
    if name.is_empty() {
        return Err(ReturnsAnnotationError::InvalidTypeName {
            name: name.to_string(),
            reason: "type name is empty".to_string(),
        });
    }

    if !name.ends_with("Row") {
        return Err(ReturnsAnnotationError::InvalidTypeName {
            name: name.to_string(),
            reason: "type name must end with `Row` (e.g., `OrgRow`)".to_string(),
        });
    }

    if !is_pascal_case_identifier(name) {
        return Err(ReturnsAnnotationError::InvalidTypeName {
            name: name.to_string(),
            reason: "type name must be PascalCase with only letters and digits".to_string(),
        });
    }

    Ok(name.to_string())
}

fn is_pascal_case_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    first.is_ascii_uppercase() && chars.all(|c| c.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_missing_and_valid_returns_annotations() {
        assert_eq!(parse_returns_annotation("SELECT 1").unwrap(), None);
        assert_eq!(
            parse_returns_annotation("-- returns: OrgRow\nSELECT 1").unwrap(),
            Some("OrgRow".to_string())
        );
        assert_eq!(
            parse_returns_annotation("\n\n-- returns: OrgRow\nSELECT 1").unwrap(),
            Some("OrgRow".to_string())
        );
        assert_eq!(
            parse_returns_annotation("-- this is a comment\n-- returns: OrgRow\nSELECT 1").unwrap(),
            Some("OrgRow".to_string())
        );
        assert_eq!(
            parse_returns_annotation("--    returns:    OrgRow   \nSELECT 1").unwrap(),
            Some("OrgRow".to_string())
        );
    }

    #[test]
    fn ignores_returns_annotations_after_sql_starts() {
        assert_eq!(
            parse_returns_annotation("SELECT 1\n-- returns: OrgRow\n").unwrap(),
            None
        );
    }

    #[test]
    fn rejects_invalid_returns_annotation_names() {
        assert!(matches!(
            parse_returns_annotation("-- returns: Org\nSELECT 1"),
            Err(ReturnsAnnotationError::InvalidTypeName { .. })
        ));
        assert!(matches!(
            parse_returns_annotation("-- returns: orgRow\nSELECT 1"),
            Err(ReturnsAnnotationError::InvalidTypeName { .. })
        ));
        assert!(matches!(
            parse_returns_annotation("-- returns: Org-Row\nSELECT 1"),
            Err(ReturnsAnnotationError::InvalidTypeName { .. })
        ));
    }
}
