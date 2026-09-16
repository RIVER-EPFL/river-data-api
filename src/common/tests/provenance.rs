use super::source_system;

#[test]
fn test_source_system_is_the_name_the_request_gives() {
    assert_eq!(source_system(" cnet ").unwrap(), "cnet");
    assert!(source_system("  ").is_err());
}

#[test]
fn a_lost_insert_is_told_apart_from_a_real_database_failure() {
    let duplicate = crudcrate::ApiError::database(sea_orm::DbErr::Custom(
        "error returned from database: 23505 duplicate key value violates unique constraint"
            .to_string(),
    ));
    assert!(super::lost_the_insert(&duplicate));
    let unrelated =
        crudcrate::ApiError::database(sea_orm::DbErr::Custom("connection closed".to_string()));
    assert!(!super::lost_the_insert(&unrelated));
    assert!(!super::lost_the_insert(&crudcrate::ApiError::bad_request(
        "23505".to_string()
    )));
}
