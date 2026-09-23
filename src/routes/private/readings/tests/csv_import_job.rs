use super::should_drop_staged;

#[test]
fn test_should_drop_staged_keeps_the_rows_for_a_retry() {
    assert!(!should_drop_staged(false, false));
}

#[test]
fn test_should_drop_staged_takes_them_on_the_last_failure_or_a_success() {
    assert!(should_drop_staged(false, true));
    assert!(should_drop_staged(true, false));
    assert!(should_drop_staged(true, true));
}

#[test]
fn test_orphaned_tokens_keeps_what_a_live_import_will_read() {
    use super::orphaned_tokens;
    use uuid::Uuid;
    let (live, dead, gone) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
    assert_eq!(
        orphaned_tokens(&[live, dead, gone], &[live]),
        vec![dead, gone]
    );
    assert!(orphaned_tokens(&[live], &[live]).is_empty());
    assert!(orphaned_tokens(&[], &[live]).is_empty());
}

mod attribute_staged {
    use super::super::{StagedRow, attribute_staged};
    use crate::routes::private::readings::service::TargetStream;
    use uuid::Uuid;

    const STREAM: Uuid = Uuid::from_u128(1);
    const SITE: Uuid = Uuid::from_u128(2);
    const PARAMETER: Uuid = Uuid::from_u128(3);
    const INSTRUMENT: Uuid = Uuid::from_u128(4);

    fn row(slot: Option<(Uuid, Uuid)>) -> StagedRow {
        let attributed = slot.is_some();
        StagedRow {
            stream_id: STREAM,
            site_id: slot.map(|(s, _)| s),
            parameter_id: slot.map(|(_, p)| p),
            time: chrono::DateTime::parse_from_rfc3339("2025-06-01T00:00:00Z").unwrap(),
            raw_value: 1.5,
            sensor_id: attributed.then_some(Uuid::from_u128(10)),
            calibration_id: attributed.then_some(Uuid::from_u128(11)),
            deployment_id: attributed.then_some(Uuid::from_u128(12)),
        }
    }

    fn paired(slot: (Uuid, Uuid)) -> TargetStream {
        TargetStream {
            slot: Some(slot),
            instrument: Some(INSTRUMENT),
        }
    }

    #[test]
    fn test_attribute_staged_keeps_a_row_whose_pairing_held() {
        let staged = row(Some((SITE, PARAMETER)));
        let attributed = attribute_staged(staged, paired((SITE, PARAMETER)));
        assert_eq!(attributed.slot_since_staging, None);
        assert_eq!(attributed.row.sensor_id, staged.sensor_id);
        assert_eq!(attributed.row.deployment_id, staged.deployment_id);
        assert_eq!(attributed.row.calibration_id, staged.calibration_id);
    }

    #[test]
    fn test_attribute_staged_unpaired_since_staging_stores_nothing_attributed() {
        let attributed = attribute_staged(row(Some((SITE, PARAMETER))), TargetStream::default());
        let r = attributed.row;
        assert_eq!(
            (
                r.site_id,
                r.parameter_id,
                r.sensor_id,
                r.deployment_id,
                r.calibration_id
            ),
            (None, None, None, None, None)
        );
        assert_eq!(attributed.slot_since_staging, None);
    }

    #[test]
    fn test_attribute_staged_unpaired_either_way_stays_unattributed() {
        let attributed = attribute_staged(row(None), TargetStream::default());
        assert_eq!(attributed.row.site_id, None);
        assert_eq!(attributed.row.sensor_id, None);
        assert_eq!(attributed.slot_since_staging, None);
    }

    #[test]
    fn test_attribute_staged_paired_since_staging_stamps_the_slot() {
        let attributed = attribute_staged(row(None), paired((SITE, PARAMETER)));
        let r = attributed.row;
        assert_eq!((r.site_id, r.parameter_id), (Some(SITE), Some(PARAMETER)));
        assert_eq!(r.sensor_id, Some(INSTRUMENT), "the channel's instrument");
        assert_eq!(
            (r.deployment_id, r.calibration_id),
            (None, None),
            "the slot reprocess resolves both"
        );
        assert_eq!(attributed.slot_since_staging, Some((SITE, PARAMETER)));
    }

    #[test]
    fn test_attribute_staged_repaired_to_another_slot_drops_the_old_slot_owner() {
        let other = (Uuid::from_u128(20), Uuid::from_u128(21));
        let attributed = attribute_staged(row(Some((SITE, PARAMETER))), paired(other));
        let r = attributed.row;
        assert_eq!((r.site_id, r.parameter_id), (Some(other.0), Some(other.1)));
        assert_eq!(r.sensor_id, Some(INSTRUMENT));
        assert_eq!((r.deployment_id, r.calibration_id), (None, None));
        assert_eq!(attributed.slot_since_staging, Some(other));
    }
}
