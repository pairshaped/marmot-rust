#[derive(marmot::FromSqlRow)]
#[from_sql(SourceRow)]
struct ProjectedRow {
    id: i64,
    #[from_sql(fnmap = "display_name")]
    name: String,
    #[from_sql(func = "origin")]
    source: &'static str,
}

struct SourceRow {
    id: i64,
    name_en: String,
}

fn display_name(row: &SourceRow) -> String {
    row.name_en.clone()
}

fn origin() -> &'static str {
    "database"
}

#[test]
fn derives_complete_named_row_projection_with_explicit_mapping_functions() {
    let projected = ProjectedRow::from(SourceRow {
        id: 42,
        name_en: "Monday League".to_string(),
    });

    assert_eq!(projected.id, 42);
    assert_eq!(projected.name, "Monday League");
    assert_eq!(projected.source, "database");
}

#[derive(marmot::FromSqlRow)]
#[from_sql(CollectionRow)]
#[from_sql(DetailRow)]
struct SharedProjection {
    id: i64,
    label: String,
}

struct CollectionRow {
    id: i64,
    label: String,
}

struct DetailRow {
    id: i64,
    label: String,
}

#[test]
fn derives_the_same_projection_from_collection_and_detail_rows() {
    let collection = SharedProjection::from(CollectionRow {
        id: 7,
        label: "Collection".to_string(),
    });
    let detail = SharedProjection::from(DetailRow {
        id: 8,
        label: "Detail".to_string(),
    });
    assert_eq!(
        (collection.id, collection.label.as_str()),
        (7, "Collection")
    );
    assert_eq!((detail.id, detail.label.as_str()), (8, "Detail"));
}
