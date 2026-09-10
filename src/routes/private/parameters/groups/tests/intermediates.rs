use super::plan_for;
use uuid::Uuid;

#[test]
fn a_code_the_catalog_holds_is_reused_and_a_member_is_added_once() {
    let doc = Uuid::new_v4();
    let catalog = vec![("DOC_A".to_string(), doc)];

    let fresh = super::plan_for("CO2_HS_Um_A", &catalog, &[]);
    assert!(fresh.mint_parameter, "a code nothing holds is minted");
    assert!(fresh.add_member);

    let existing = plan_for("doc_a", &catalog, &[]);
    assert!(
        !existing.mint_parameter,
        "the catalog's uniqueness is case-insensitive, so this is the same parameter"
    );
    assert!(existing.add_member);

    let already = plan_for("DOC_A", &catalog, &[doc]);
    assert!(!already.mint_parameter);
    assert!(
        !already.add_member,
        "re-declaring what the group already carries adds nothing"
    );
}
