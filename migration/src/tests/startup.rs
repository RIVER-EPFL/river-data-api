use super::*;

#[test]
fn test_missing_migration_names_rebuild_remedy() {
    let error = DbErr::Custom("Migration file of version 'm20260907_000007_meteoswiss_pressure' is missing, this migration has been applied but its file is missing\nMigration file of version 'another' is missing".into());
    let message = startup_error(error).to_string();
    assert!(message.contains("docker compose down -v"));
    assert!(message.contains("restore_cutover"));
    assert!(!message.contains('\n'));
}

#[test]
fn test_other_migration_error_is_preserved() {
    let error = DbErr::Custom("permission denied".into());
    assert_eq!(
        startup_error(error).to_string(),
        "Custom Error: permission denied"
    );
}
